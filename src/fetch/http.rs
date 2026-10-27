//! The one HTTP client iets has, behind [`super::get`]: hyper over
//! tokio-rustls, the stack tonic already brings; no libcurl. Redirects are
//! followed like filetransfer.cc (`CURLOPT_FOLLOWLOCATION`,
//! `CURLOPT_MAXREDIRS` 10). Credentials follow a redirect only as libcurl
//! lets them: see `redirect`.
//!
//! The body is never held whole: hyper runs on a thread of its own and hands
//! its frames to a bounded queue, and the caller's thread reads that queue
//! through the decompressor. Both ends stop when the other does.

use std::error::Error as StdError;
use std::fmt::{self, Display, Formatter};
use std::future::Future;
use std::io::{self, ErrorKind, Read};
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;
use std::{result, thread};

use crate::error::{Error, Result};
use crate::log;
use crate::remote::worker;
use crate::store::hash;
use http_body_util::BodyExt;

use super::{Limits, bad, roots, shown};

/// filetransfer.cc:CURLOPT_MAXREDIRS.
const MAX_REDIRECTS: usize = 10;

/// filetransfer.hh:`download-attempts`. The setting itself is the daemon's
/// (`docs/reference/settings.md`), so this is its default, fixed.
const TRIES: u32 = 5;

/// Body frames in flight between hyper and the reader. hyper's frames are tens
/// of kilobytes, so this is the download's whole memory cost.
const QUEUE: usize = 16;

/// `fut`, or `why` once `limit` has passed.
async fn within<T>(
    limit: Option<Duration>,
    why: impl FnOnce() -> Error,
    fut: impl Future<Output = Result<T>>,
) -> Result<T> {
    match limit {
        None => fut.await,
        Some(d) => tokio::time::timeout(d, fut).await.map_err(|_| why())?,
    }
}

/// The socket under TLS and hyper, with `stalled-download-timeout`: a read
/// that has waited `limit` fails with [`Stalled`] only if the kernel holds no
/// byte for it either. Like curl, which tries the socket before
/// `Curl_speedcheck`, a read is tried before the clock, so the time the
/// process was stopped or not scheduled is no stall.
struct Watched {
    tcp: tokio::net::TcpStream,
    limit: Option<Duration>,
    wait: Option<Pin<Box<tokio::time::Sleep>>>,
}

/// The error under hyper's when [`Watched`] gives up.
#[derive(Debug)]
struct Stalled;

impl Display for Stalled {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("no byte arrived within stalled-download-timeout")
    }
}

impl StdError for Stalled {}

/// Whether `e` or one of its causes is [`Stalled`].
fn is_stall(e: &(dyn StdError + 'static)) -> bool {
    let mut at = Some(e);
    while let Some(e) = at {
        let ours = e.downcast_ref::<io::Error>().and_then(|e| e.get_ref());
        if ours.is_some_and(|e| e.is::<Stalled>()) {
            return true;
        }
        at = e.source();
    }
    false
}

/// tokio drops the readiness of an `epoll_wait` that SIGCONT interrupts and
/// still fires the timers that the stop overran, so the kernel decides. No
/// `MSG_DONTWAIT`, which Windows lacks: tokio sockets are nonblocking.
fn arrived(tcp: &tokio::net::TcpStream) -> bool {
    let peek = rustix::net::RecvFlags::PEEK;
    rustix::net::recv(tcp, &mut [0u8; 1][..], peek) != Err(rustix::io::Errno::WOULDBLOCK)
}

impl tokio::io::AsyncRead for Watched {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let read = Pin::new(&mut this.tcp).poll_read(cx, buf);
        let Some(limit) = this.limit.filter(|_| read.is_pending()) else {
            this.wait = None;
            return read;
        };
        let wait = (this.wait).get_or_insert_with(|| Box::pin(tokio::time::sleep(limit)));
        loop {
            if tokio::time::Instant::now() >= wait.deadline() {
                if !arrived(&this.tcp) {
                    return Poll::Ready(Err(io::Error::new(ErrorKind::TimedOut, Stalled)));
                }
                wait.as_mut().reset(tokio::time::Instant::now() + limit);
            }
            if wait.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
        }
    }
}

impl tokio::io::AsyncWrite for Watched {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().tcp).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().tcp).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().tcp).poll_shutdown(cx)
    }
}

/// What a URL was split into; `target` is the request line's path and query.
struct Url {
    https: bool,
    host: String,
    port: u16,
    target: String,
    /// `user:password` before the host, percent-decoded.
    userinfo: Option<String>,
}

impl Url {
    fn parse(url: &str) -> Result<Url> {
        let (scheme, rest) = url.split_once("://").ok_or_else(|| bad(url, "no scheme"))?;
        let https = match scheme {
            "https" => true,
            "http" => false,
            other => return Err(bad(url, &format!("unsupported scheme '{other}'"))),
        };
        let (authority, path) = match rest.find(['/', '?', '#']) {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        let (userinfo, authority) = match authority.rsplit_once('@') {
            Some((u, h)) => (Some(worker::percent_decode(u)), h),
            None => (None, authority),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
                (h, p.parse::<u16>().map_err(|_| bad(url, "bad port"))?)
            }
            _ => (authority, if https { 443 } else { 80 }),
        };
        if host.is_empty() {
            return Err(bad(url, "no host"));
        }
        Ok(Url {
            https,
            host: host.to_string(),
            port,
            target: if path.is_empty() {
                "/".to_string()
            } else {
                path.to_string()
            },
            userinfo,
        })
    }

    /// libcurl sends the URL's `user:password` as Basic credentials on the
    /// first request, without waiting for a challenge (`CURLAUTH_BASIC` is its
    /// default `CURLOPT_HTTPAUTH`).
    fn basic(&self) -> Option<(String, String)> {
        let u = self.userinfo.as_ref()?;
        let b64 = hash::base64_encode(u.as_bytes());
        Some(("Authorization".into(), format!("Basic {b64}")))
    }

    fn authority(&self) -> String {
        let default = if self.https { 443 } else { 80 };
        if self.port == default {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// A redirect from `from` to `to` in a transfer that began at `first`: whether
/// the credentials go along. libcurl sends a custom `Authorization` only to the
/// first scheme, host and port (http.c:Curl_allow_auth_to_host, the
/// CVE-2022-27776 fix), and filetransfer.cc never sets
/// `CURLOPT_UNRESTRICTED_AUTH`. iets's headers are all credentials, so the rule
/// covers `Private-token` too. An https-to-http redirect is refused: libcurl
/// allows it, but it drops TLS from a download nothing has verified yet.
fn redirect(first: &Url, from: &Url, to: &Url) -> result::Result<bool, &'static str> {
    if from.https && !to.https {
        return Err("refusing a redirect from https to http");
    }
    Ok(first.https == to.https
        && first.host.eq_ignore_ascii_case(&to.host)
        && first.port == to.port)
}

/// Whether `location` starts with a scheme, and so is absolute. What comes
/// before `://` must be a scheme (RFC 3986): a `://` in a query is not one.
fn has_scheme(location: &str) -> bool {
    let Some((s, _)) = location.split_once("://") else {
        return false;
    };
    s.starts_with(|c: char| c.is_ascii_alphabetic())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
}

/// `.` and `..` segments removed, as a URL path has them removed
/// (RFC 3986:5.2.4). A `..` at the top removes nothing.
fn normalise(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    let slash = path.ends_with(['/', '.']) && !out.is_empty();
    format!("/{}{}", out.join("/"), if slash { "/" } else { "" })
}

/// `Location` against the URL it came from: absolute, host-relative or
/// path-relative (libcurl resolves all three).
fn resolve(base: &str, location: &str) -> String {
    if has_scheme(location) {
        return location.to_string();
    }
    let (scheme, rest) = base.split_once("://").unwrap_or(("https", base));
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let joined = if location.starts_with('/') {
        location.to_string()
    } else {
        let path = &rest[authority.len()..];
        let path = path.split(['?', '#']).next().unwrap_or(path);
        format!(
            "{}/{location}",
            path.rsplit_once('/').map_or("", |(d, _)| d)
        )
    };
    let (path, tail) = joined.split_at(joined.find(['?', '#']).unwrap_or(joined.len()));
    format!("{scheme}://{authority}{}{tail}", normalise(path))
}

/// Process-wide on purpose: it is immutable, costly to build, and holds no
/// run state, so two sessions may share it.
fn tls() -> Result<Arc<rustls::ClientConfig>> {
    static TLS: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    if let Some(cfg) = TLS.get() {
        return Ok(cfg.clone());
    }
    let roots = rustls::RootCertStore::from_iter(roots::roots()?.iter().cloned());
    let mut cfg = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TLS.get_or_init(|| Arc::new(cfg)).clone())
}

/// `GET url` over HTTP or HTTPS: the download runs on a thread of its own,
/// and `read` takes the body on this one. A failed download is the error,
/// whatever `read` made of the body it cut short.
pub(super) fn get(
    url: &str,
    headers: &[(String, String)],
    limits: Limits,
    read: impl FnOnce(Box<dyn Read>) -> Result<u64>,
) -> Result<u64> {
    let (tx, rx) = tokio::sync::mpsc::channel(QUEUE);
    let target = url.to_string();
    let sent = headers.to_vec();
    let download = thread::spawn(move || -> Result<()> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| Error::io("cannot start a tokio runtime", e))?;
        rt.block_on(stream(&target, &sent, limits, tx))
    });
    let queued = Queued {
        rx,
        at: Vec::new(),
        pos: 0,
    };
    let read = read(Box::new(queued));
    download
        .join()
        .map_err(|_| Error::Store("the download thread panicked".into()))??;
    read
}

/// The reading half of the queue: hyper's frames, one after the other.
struct Queued {
    rx: tokio::sync::mpsc::Receiver<result::Result<Vec<u8>, String>>,
    at: Vec<u8>,
    pos: usize,
}

impl Read for Queued {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.pos == self.at.len() {
            match self.rx.blocking_recv() {
                None => return Ok(0),
                Some(Err(e)) => return Err(io::Error::other(e)),
                Some(Ok(chunk)) => {
                    self.at = chunk;
                    self.pos = 0;
                }
            }
        }
        let n = buf.len().min(self.at.len() - self.pos);
        buf[..n].copy_from_slice(&self.at[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Follows redirects and pushes the frames of the 200 into `tx`.
async fn stream(
    url: &str,
    headers: &[(String, String)],
    limits: Limits,
    tx: tokio::sync::mpsc::Sender<result::Result<Vec<u8>, String>>,
) -> Result<()> {
    let first = Url::parse(url)?;
    let mut sent = headers.to_vec();
    sent.extend(first.basic());
    let headers = &sent[..];
    let mut current = url.to_string();
    let mut credentials = true;
    let (mut redirects, mut attempt) = (0usize, 0u32);
    loop {
        let creds = if credentials { headers } else { &[] };
        let (status, location, mut body, driver) = once(&current, creds, limits).await?;
        match status {
            200 => {
                let mut sent = Ok(());
                loop {
                    let next = body.frame().await.transpose().map_err(|e| {
                        if is_stall(&e) {
                            bad(&current, "the download stalled (stalled-download-timeout)")
                        } else {
                            bad(&current, &format!("reading the response failed: {e}"))
                        }
                    });
                    let frame = match next {
                        Ok(Some(f)) => f,
                        Ok(None) => break,
                        Err(e) => {
                            let _ = tx.send(Err(e.to_string())).await;
                            sent = Err(e);
                            break;
                        }
                    };
                    if let Ok(data) = frame.into_data()
                        && tx.send(Ok(data.to_vec())).await.is_err()
                    {
                        break;
                    }
                }
                driver.abort();
                return sent;
            }
            301 | 302 | 303 | 307 | 308 => {
                driver.abort();
                redirects += 1;
                if redirects > MAX_REDIRECTS {
                    return Err(bad(url, &format!("more than {MAX_REDIRECTS} redirects")));
                }
                let Some(to) = location else {
                    return Err(bad(&current, &format!("HTTP {status} with no Location")));
                };
                let next = resolve(&current, &to);
                credentials = redirect(&first, &Url::parse(&current)?, &Url::parse(&next)?)
                    .map_err(|why| bad(&next, why))?;
                current = next;
            }
            other => {
                driver.abort();
                attempt += 1;
                let Some(base) = transient(other).filter(|_| attempt < TRIES) else {
                    return Err(bad(&current, &format!("HTTP status {other}")));
                };
                let ms = base << (attempt - 1);
                log::line(&format!(
                    "warning: unable to download '{}': HTTP error {other}; \
                     retrying in {ms} ms (attempt {attempt}/{TRIES})",
                    shown(&current)
                ));
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }
        }
    }
}

/// filetransfer.cc:finish: the statuses Nix treats as transient, and the delay
/// each one starts from (`RETRY_TIME_MS_DEFAULT`, and a minute for 429). 404,
/// 410 and the authorisation failures are final, as are 501, 505 and 511.
fn transient(status: u16) -> Option<u64> {
    match status {
        429 => Some(60_000),
        408 => Some(250),
        501 | 505 | 511 => None,
        _ if (500..600).contains(&status) => Some(250),
        _ => None,
    }
}

type Started = (
    u16,
    Option<String>,
    hyper::body::Incoming,
    tokio::task::JoinHandle<result::Result<(), hyper::Error>>,
);

/// One request: the status, the `Location` header, the body not yet read, and
/// the connection task that must live as long as the body. `connect-timeout`
/// covers TCP and TLS, as `CURLOPT_CONNECTTIMEOUT` does; the wait for the
/// status line counts as a stall.
async fn once(url: &str, headers: &[(String, String)], limits: Limits) -> Result<Started> {
    let u = Url::parse(url)?;
    let timed_out = || bad(url, "the connection timed out (connect-timeout)");
    let io = within(limits.connect, timed_out, async {
        let stream = tokio::net::TcpStream::connect((u.host.as_str(), u.port))
            .await
            .map_err(|e| Error::io(format!("cannot connect to '{}'", u.authority()), e))?;
        let stream = Watched {
            tcp: stream,
            limit: limits.stall,
            wait: None,
        };
        if !u.https {
            return Ok(Box::new(stream) as Box<dyn Stream>);
        }
        let name = rustls::pki_types::ServerName::try_from(u.host.clone())
            .map_err(|_| bad(url, "the host is not a valid TLS name"))?;
        let tls = tokio_rustls::TlsConnector::from(tls()?);
        let tls = tls
            .connect(name, stream)
            .await
            .map_err(|e| Error::io(format!("TLS handshake with '{}'", u.host), e))?;
        Ok(Box::new(tls) as Box<dyn Stream>)
    })
    .await?;
    let (mut sender, conn) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(io))
            .await
            .map_err(|e| bad(url, &format!("HTTP handshake failed: {e}")))?;
    let driver = tokio::spawn(conn);

    let mut req = hyper::Request::builder()
        .method("GET")
        .uri(&u.target)
        .header("host", u.authority())
        .header("user-agent", concat!("iets/", env!("CARGO_PKG_VERSION")));
    for (name, value) in headers {
        req = req.header(name.as_str(), value.as_str());
    }
    let req = req
        .body(String::new())
        .map_err(|e| bad(url, &format!("bad request: {e}")))?;
    let res = sender.send_request(req).await.map_err(|e| {
        if is_stall(&e) {
            bad(url, "no response (stalled-download-timeout)")
        } else {
            bad(url, &format!("request failed: {e}"))
        }
    })?;
    let status = res.status().as_u16();
    let location = res
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    Ok((status, location, res.into_body(), driver))
}

trait Stream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin> Stream for T {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::get_decompressed;
    use crate::fetch::tests::limits;

    use std::future::poll_fn;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{self, Receiver};
    use std::task::Waker;
    use tokio::io::AsyncRead;

    #[test]
    fn urls_split() {
        let u = Url::parse("https://api.github.com/repos/a/b/tarball/c").unwrap();
        assert!(u.https);
        assert_eq!((u.host.as_str(), u.port), ("api.github.com", 443));
        assert_eq!(u.target, "/repos/a/b/tarball/c");
        assert_eq!(u.authority(), "api.github.com");

        let u = Url::parse("http://localhost:8080").unwrap();
        assert!(!u.https);
        assert_eq!(
            (u.host.as_str(), u.port, u.target.as_str()),
            ("localhost", 8080, "/")
        );
        assert_eq!(u.authority(), "localhost:8080");

        let u =
            Url::parse("https://gitlab.com/api/v4/projects/a%2Fb/repository/archive.tar.gz?sha=d")
                .unwrap();
        assert_eq!(
            u.target,
            "/api/v4/projects/a%2Fb/repository/archive.tar.gz?sha=d"
        );

        assert!(Url::parse("ftp://x/y").is_err());
        assert!(Url::parse("nonsense").is_err());
    }

    #[test]
    fn redirects_resolve() {
        assert_eq!(
            resolve(
                "https://api.github.com/repos/a/b/tarball/c",
                "https://codeload.github.com/a/b/tar.gz/c"
            ),
            "https://codeload.github.com/a/b/tar.gz/c"
        );
        assert_eq!(
            resolve("https://x.example/a/b?q=1", "/c/d"),
            "https://x.example/c/d"
        );
        assert_eq!(
            resolve("https://x.example/a/b", "c"),
            "https://x.example/a/c"
        );
        // a `://` in a query is not a scheme, and `.` and `..` are applied
        assert_eq!(
            resolve("https://x.example/a/b", "/ok.tar?next=http://y.invalid/x"),
            "https://x.example/ok.tar?next=http://y.invalid/x"
        );
        assert_eq!(
            resolve("https://x.example/a/b", "../ok.tar"),
            "https://x.example/ok.tar"
        );
        assert_eq!(
            resolve("https://x.example/a/b", "./d/"),
            "https://x.example/a/d/"
        );
    }

    /// A server on 127.0.0.1 for `n` connections, one after the other: `reply`
    /// answers each; the lowercased request heads come out of the receiver.
    fn serve(
        n: usize,
        reply: impl Fn(&str, &mut TcpStream) + Send + 'static,
    ) -> (u16, Receiver<String>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for c in l.incoming().take(n) {
                let mut c = c.unwrap();
                let mut head = Vec::new();
                let mut b = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && c.read(&mut b).unwrap_or(0) == 1 {
                    head.push(b[0]);
                }
                let head = String::from_utf8_lossy(&head).to_lowercase();
                reply(&head, &mut c);
                let _ = tx.send(head);
            }
        });
        (port, rx)
    }

    fn respond(c: &mut TcpStream, status: &str, extra: &str, body: &[u8]) {
        let len = body.len();
        let head = format!("HTTP/1.1 {status}\r\n{extra}content-length: {len}\r\n\r\n");
        let _ = c.write_all(head.as_bytes());
        let _ = c.write_all(body);
    }

    #[test]
    fn credentials_stay_on_the_first_origin() {
        let (other, seen_other) = serve(1, |_, c| respond(c, "200 OK", "", b"hello"));
        let (first, seen_first) = serve(2, move |head, c| {
            let to = if head.starts_with("get / ") {
                "/again".to_string()
            } else {
                format!("http://127.0.0.1:{other}/x")
            };
            respond(c, "302 Found", &format!("location: {to}\r\n"), b"");
        });
        let auth = [("Authorization".to_string(), "token s3cret".to_string())];
        let mut out = Vec::new();
        let url = format!("http://127.0.0.1:{first}/");
        get_decompressed(&url, &auth, limits(), &mut out).unwrap();
        assert_eq!(out, b"hello");
        for _ in 0..2 {
            assert!(
                seen_first
                    .recv()
                    .unwrap()
                    .contains("authorization: token s3cret")
            );
        }
        assert!(!seen_other.recv().unwrap().contains("authorization"));
    }

    #[test]
    fn redirects_keep_credentials_only_on_the_same_origin() {
        let u = |s: &str| Url::parse(s).unwrap();
        let first = u("https://api.github.com/a");
        assert_eq!(
            redirect(&first, &first, &u("https://API.github.com/b")),
            Ok(true)
        );
        let other = u("https://codeload.github.com/b");
        assert_eq!(redirect(&first, &first, &other), Ok(false));
        assert_eq!(
            redirect(&first, &other, &u("https://api.github.com/c")),
            Ok(true)
        );
        assert_eq!(
            redirect(&first, &first, &u("https://api.github.com:8443/")),
            Ok(false)
        );
        assert!(redirect(&first, &first, &u("http://api.github.com/b")).is_err());
        let plain = u("http://x.example/a");
        assert_eq!(
            redirect(&plain, &plain, &u("https://x.example/a")),
            Ok(false)
        );
    }

    /// A download against a server that sends `sent` and then holds the
    /// connection open until the call has returned: only the stall limit can
    /// end the call, however long the host takes to schedule either side.
    fn stalled(sent: &'static [u8]) -> String {
        let (hold, held) = mpsc::channel::<()>();
        let (port, _) = serve(1, move |_, c| {
            let _ = c.write_all(sent);
            let _ = held.recv();
        });
        let l = Limits {
            stall: Some(Duration::from_millis(50)),
            ..limits()
        };
        let url = format!("http://127.0.0.1:{port}/");
        let e = get_decompressed(&url, &[], l, &mut Vec::new()).unwrap_err();
        drop(hold);
        e.to_string()
    }

    #[test]
    fn a_server_that_never_answers_is_a_stall() {
        let e = stalled(b"");
        assert!(e.contains("no response (stalled-download-timeout)"), "{e}");
    }

    #[test]
    fn a_stalled_download_fails() {
        // a server the host does not schedule within the limit ends in the
        // wait for the status line instead, which is the same limit
        let e = stalled(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nhalf");
        assert!(e.contains("(stalled-download-timeout)"), "{e}");
    }

    /// libcurl sends `user:password@host` as Basic credentials, and no message
    /// ever prints the password.
    #[test]
    fn credentials_in_the_url_are_sent_and_never_printed() {
        // `serve` hands the request head over in lower case
        let want = format!(
            "authorization: basic {}",
            crate::store::hash::base64_encode(b"user:s3cret").to_lowercase()
        );
        let sent = want.clone();
        let (port, seen) = serve(1, move |head, c| {
            let ok = head.contains(&sent);
            let status = if ok { "200 OK" } else { "401 Unauthorized" };
            respond(c, status, "", if ok { b"hello" } else { b"" });
        });
        let mut out = Vec::new();
        let url = format!("http://user:s3cret@127.0.0.1:{port}/x");
        get_decompressed(&url, &[], limits(), &mut out).unwrap();
        assert_eq!(out, b"hello");
        assert!(seen.recv().unwrap().contains(&want));

        let (port, _) = serve(1, |_, c| respond(c, "403 Forbidden", "", b""));
        let url = format!("http://user:s3cret@127.0.0.1:{port}/x");
        let e = get_decompressed(&url, &[], limits(), &mut Vec::new())
            .unwrap_err()
            .to_string();
        assert!(!e.contains("s3cret"), "{e}");
        assert!(e.contains("user:***@"), "{e}");
        assert_eq!(shown("https://x.example/a?u=1"), "https://x.example/a?u=1");
    }

    /// filetransfer.cc retries a 5xx and gives up on a 404.
    #[test]
    fn a_transient_status_is_retried() {
        let n = AtomicUsize::new(0);
        let (port, _) = serve(3, move |_, c| {
            let i = n.fetch_add(1, Ordering::Relaxed);
            if i < 2 {
                respond(c, "503 Service Unavailable", "", b"");
            } else {
                respond(c, "200 OK", "", b"late");
            }
        });
        let mut out = Vec::new();
        let url = format!("http://127.0.0.1:{port}/");
        get_decompressed(&url, &[], limits(), &mut out).unwrap();
        assert_eq!(out, b"late");

        let (port, _) = serve(1, |_, c| respond(c, "404 Not Found", "", b""));
        let url = format!("http://127.0.0.1:{port}/");
        let e = get_decompressed(&url, &[], limits(), &mut Vec::new()).unwrap_err();
        assert!(e.to_string().contains("HTTP status 404"), "{e}");
    }

    /// The runtime thread blocks past the limit, as SIGSTOP or a busy host
    /// would hold it, with no reactor turn in between: bytes waiting in the
    /// kernel are no stall, and an empty socket is one.
    #[test]
    fn time_the_reader_did_not_run_is_no_stall() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            let client = TcpStream::connect(l.local_addr().unwrap()).unwrap();
            let (mut server, _) = l.accept().unwrap();
            client.set_nonblocking(true).unwrap();
            let limit = Duration::from_millis(1);
            let mut w = Watched {
                tcp: tokio::net::TcpStream::from_std(client).unwrap(),
                limit: Some(limit),
                wait: None,
            };
            let read = |w: &mut Watched, cx: &mut Context<'_>| {
                let mut buf = [0u8; 8];
                let mut rb = tokio::io::ReadBuf::new(&mut buf);
                Pin::new(w)
                    .poll_read(cx, &mut rb)
                    .map_ok(|()| rb.filled().to_vec())
            };
            let poll = |w: &mut Watched| read(w, &mut Context::from_waker(Waker::noop()));

            server.write_all(b"data").unwrap();
            assert!(poll(&mut w).is_pending());
            thread::sleep(limit * 2);
            assert!(poll(&mut w).is_pending());
            let got = poll_fn(|cx| read(&mut w, cx)).await.unwrap();
            assert_eq!(got, b"data");

            let e = loop {
                thread::sleep(limit * 2);
                if let Poll::Ready(r) = poll(&mut w) {
                    break r.unwrap_err();
                }
            };
            assert!(is_stall(&e), "{e}");
        });
    }
}
