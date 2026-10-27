//! The connection to a `grpc://` store: one channel, one tokio runtime, and
//! the retry and error rules of nix-grpc-store's own client.

use std::future::{self, Future};
use std::path::Path;
use std::pin::pin;
use std::task::Poll;
use std::time::{Duration, Instant};
use std::{error, fs, result, thread};

use tokio::sync::watch;
use tonic::metadata::{MetadataMap, MetadataValue};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

use crate::error::{Error, Result};
use crate::fetch::roots::roots;
use crate::session::{Armed, Cancel, Session};

use super::GrpcConfig;
use super::proto::{self, nix_remote_client::NixRemoteClient, scheduler_client::SchedulerClient};

impl GrpcConfig {
    /// store.cc:statusError, with the same hints.
    pub fn status_error(&self, s: &tonic::Status, what: &str) -> Error {
        let mut hint = String::new();
        if !self.insecure
            && matches!(
                s.code(),
                tonic::Code::Unauthenticated | tonic::Code::Unavailable
            )
        {
            hint = connect_hint(self, s.message());
        }
        Error::Protocol(format!(
            "gRPC {what} on '{}' failed ({:?}): {}{hint}",
            self.authority,
            s.code(),
            s.message()
        ))
    }

    fn endpoint(&self) -> Result<Endpoint> {
        let scheme = if self.insecure { "http" } else { "https" };
        let mut ep = Endpoint::from_shared(format!("{scheme}://{}", self.authority))
            .map_err(|e| Error::Usage(format!("grpc://{}: {e}", self.authority)))?
            .connect_timeout(Duration::from_secs(self.connect_timeout))
            .tcp_nodelay(true)
            .initial_stream_window_size(8 << 20)
            .initial_connection_window_size(32 << 20);
        if !self.insecure {
            // store.cc:sslOptions. The balancer's own certificate is public, so
            // the shared roots verify it; `ca-cert` adds another CA.
            let mut tls = ClientTlsConfig::new().trust_anchors(roots()?.iter().cloned());
            if let Some(ca) = &self.ca_cert {
                tls = tls.ca_certificate(Certificate::from_pem(read(ca)?));
            }
            if let (Some(cert), Some(key)) = (&self.client_cert, &self.client_key) {
                tls = tls.identity(Identity::from_pem(read(cert)?, read(key)?));
            }
            ep = ep
                .tls_config(tls)
                .map_err(|e| Error::Protocol(format!("grpc://{}: TLS: {e}", self.authority)))?;
        }
        Ok(ep)
    }
}

/// `tonic::transport::Error` prints only "transport error"; the reason (a TLS
/// alert, a refused connection) is in its source chain.
fn causes(e: &dyn error::Error) -> String {
    let mut s = e.to_string();
    let mut src = e.source();
    while let Some(e) = src {
        let next = e.to_string();
        if !s.contains(&next) {
            s.push_str(": ");
            s.push_str(&next);
        }
        src = e.source();
    }
    s
}

fn read(p: &Path) -> Result<Vec<u8>> {
    fs::read(p).map_err(|e| Error::io(format!("cannot read '{}'", p.display()), e))
}

/// What `StoreInfo` reported.
#[derive(Debug, Clone, Copy, Default)]
pub struct StoreInfo {
    pub trusted: Option<bool>,
}

/// A connected `grpc://` store: one channel, one runtime, the headers every
/// call carries.
pub struct Client {
    pub cfg: GrpcConfig,
    pub rt: tokio::runtime::Runtime,
    pub stub: NixRemoteClient<Channel>,
    pub sched: SchedulerClient<Channel>,
    pub info: StoreInfo,
    /// The session's cancel, which turns `stop` true.
    cancel: Cancel,
    stop: watch::Receiver<bool>,
    _armed: Armed,
}

/// tonic caps decoded messages at 4 MiB by default; NAR frames are larger.
const MAX_MESSAGE: usize = 256 << 20;

/// `fut` on `rt`, or `None` once `stop` turns true.
fn until_stopped<T>(
    rt: &tokio::runtime::Runtime,
    mut stop: watch::Receiver<bool>,
    fut: impl Future<Output = T>,
) -> Option<T> {
    rt.block_on(async move {
        let stopped = async move {
            // a sender that is gone stops nothing
            if stop.wait_for(|stopped| *stopped).await.is_err() {
                future::pending::<()>().await;
            }
        };
        let (mut fut, mut stopped) = (pin!(fut), pin!(stopped));
        future::poll_fn(|cx| {
            if stopped.as_mut().poll(cx).is_ready() {
                return Poll::Ready(None);
            }
            fut.as_mut().poll(cx).map(Some)
        })
        .await
    })
}

/// What a call the session's cancel stopped reports in place of an answer.
pub fn cancelled() -> tonic::Status {
    tonic::Status::cancelled("cancelled")
}

impl Client {
    /// Connects and runs `StoreInfo`, which is what `GrpcStore::connect` does.
    /// The session this thread entered can cancel it (`session::Cancel`).
    pub fn open(cfg: &GrpcConfig) -> Result<Client> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| Error::io("cannot start a tokio runtime", e))?;
        let cancel = Session::current().cancel.clone();
        let (tx, stop) = watch::channel(false);
        let armed = cancel.arm(move || {
            let _ = tx.send(true);
        })?;
        let ep = cfg.endpoint()?;
        let channel = until_stopped(&rt, stop.clone(), ep.connect())
            .ok_or(Error::Cancelled)?
            .map_err(|e| connect_error(cfg, &causes(&e)))?;
        let mut c = Client {
            cfg: cfg.clone(),
            rt,
            sched: SchedulerClient::new(channel.clone()),
            stub: NixRemoteClient::new(channel)
                .max_decoding_message_size(MAX_MESSAGE)
                .max_encoding_message_size(MAX_MESSAGE),
            info: StoreInfo::default(),
            cancel,
            stop,
            _armed: armed,
        };
        let reply = c.call("StoreInfo", |mut stub, h| async move {
            stub.store_info(c_request(proto::StoreInfoRequest {}, &h))
                .await
        })?;
        c.info = StoreInfo {
            trusted: reply.trusted,
        };
        Ok(c)
    }

    /// One unary call with the store's fixed headers and the `UNAVAILABLE`
    /// retry loop of `GrpcStore::retrying`: the first call waits
    /// `connect-timeout` for the service, later ones get `RESTART_GRACE`.
    pub fn call<T, F, Fut>(&mut self, what: &str, mut attempt: F) -> Result<T>
    where
        F: FnMut(NixRemoteClient<Channel>, Headers) -> Fut,
        Fut: Future<Output = result::Result<tonic::Response<T>, tonic::Status>>,
    {
        let deadline = Instant::now() + Duration::from_secs(self.cfg.connect_timeout);
        let mut pause = Duration::from_millis(500);
        loop {
            let stub = self.stub.clone();
            let headers = self.headers()?;
            let r = self.block(attempt(stub, headers)).ok_or(Error::Cancelled)?;
            match r {
                Ok(resp) => return Ok(resp.into_inner()),
                Err(s) => {
                    if !gone_away(&s) || Instant::now() + pause > deadline {
                        return Err(self.status_error(&s, what));
                    }
                    errln!(
                        "{}: {}, retrying",
                        self.cfg.authority,
                        first_line(s.message())
                    );
                    thread::sleep(pause);
                    pause = (pause * 2).min(Duration::from_secs(4));
                }
            }
        }
    }

    /// `fut` on this client's runtime, or `None` once the session's cancel
    /// stopped it.
    pub fn block<T>(&self, fut: impl Future<Output = T>) -> Option<T> {
        until_stopped(&self.rt, self.stop.clone(), fut)
    }

    pub fn status_error(&self, s: &tonic::Status, what: &str) -> Error {
        if self.cancel.is_cancelled() {
            return Error::Cancelled;
        }
        self.cfg.status_error(s, what)
    }

    /// The headers `StaticHeaders` stamps on every call plus the per-call
    /// bearer token, which the C++ `TokenFileCredentials` re-reads each time.
    pub fn headers(&self) -> Result<Headers> {
        let mut h = Vec::new();
        if !self.cfg.system.is_empty() {
            h.push(("x-nix-system", self.cfg.system.clone()));
        }
        if let Some(path) = &self.cfg.token_file {
            let token = String::from_utf8_lossy(&read(path)?).trim().to_string();
            if token.is_empty() || token.contains(['\r', '\n']) {
                return Err(Error::Usage(format!(
                    "token file '{}' is empty or multi-line",
                    path.display()
                )));
            }
            h.push(("authorization", format!("Bearer {token}")));
        }
        Ok(h)
    }
}

/// Metadata this client sets: `x-nix-system` and `authorization` on every
/// call, plus `x-nix-features` and `x-nix-worker` on a build.
pub type Headers = Vec<(&'static str, String)>;

/// A request with `headers` in its initial metadata.
pub fn c_request<T>(body: T, headers: &[(&'static str, String)]) -> tonic::Request<T> {
    let mut req = tonic::Request::new(body);
    put_headers(req.metadata_mut(), headers);
    req
}

fn put_headers(md: &mut MetadataMap, headers: &[(&'static str, String)]) {
    for (k, v) in headers {
        if let Ok(v) = MetadataValue::try_from(v.as_str()) {
            md.insert(*k, v);
        }
    }
}

/// store.cc:transportError: gRPC folds every connect failure into
/// `UNAVAILABLE`, only the text tells TCP from TLS apart.
fn transport_error(msg: &str) -> bool {
    [
        "Connection refused",
        "Connection reset",
        "No route",
        "unreachable",
        "timed out",
        "DNS",
        "GOAWAY",
        "error trying to connect",
        "tcp connect error",
    ]
    .iter()
    .any(|n| msg.contains(n))
}

/// store.cc:goneAway: a worker or the balancer restarting under us.
pub fn gone_away(s: &tonic::Status) -> bool {
    match s.code() {
        tonic::Code::Unavailable => {
            transport_error(s.message())
                || s.message().contains("upstream")
                || s.message().contains("shutting down")
        }
        tonic::Code::Cancelled => s.message().contains("Cancelling all calls"),
        _ => false,
    }
}

pub fn first_line(msg: &str) -> &str {
    msg.split('\n').next().unwrap_or(msg)
}

fn connect_hint(cfg: &GrpcConfig, msg: &str) -> String {
    if transport_error(msg) {
        return "\nhint: could not reach the server (TCP/DNS), not a certificate problem."
            .to_string();
    }
    if [
        "handshake",
        "Handshake",
        "SSL",
        "TLS",
        "certificate",
        "Socket closed",
    ]
    .iter()
    .any(|n| msg.contains(n))
    {
        if cfg.client_cert.is_none() && cfg.token_file.is_none() {
            return "\nhint: TLS handshake failed and no client certificate was presented. \
                    Set 'client-cert'/'client-key' or 'token-file'."
                .to_string();
        }
        return "\nhint: TLS handshake failed: the server certificate is not trusted by our CA \
                bundle, the server rejected our client certificate, or the peer is not \
                speaking TLS."
            .to_string();
    }
    String::new()
}

fn connect_error(cfg: &GrpcConfig, msg: &str) -> Error {
    Error::Protocol(format!(
        "cannot connect to '{}': {msg}{}",
        cfg.url(),
        connect_hint(cfg, msg)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A call the server never answers ends once the session is cancelled,
    /// and one that answers is not held back.
    #[test]
    fn a_cancel_stops_a_call_that_waits() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (tx, stop) = watch::channel(false);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            let _ = tx.send(true);
        });
        assert_eq!(until_stopped(&rt, stop, future::pending::<()>()), None);
        let (_tx, stop) = watch::channel(false);
        assert_eq!(until_stopped(&rt, stop, async { 7 }), Some(7));
    }
}
