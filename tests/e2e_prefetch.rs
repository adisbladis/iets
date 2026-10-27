//! `iets prefetch` (docs/reference/commands.md) against `nix store
//! prefetch-file` and `nix hash`.
#![cfg(all(unix, feature = "cli"))]

mod e2e;
mod hermetic;

use std::fs;
use std::io::Write;
use std::process::{Command, Output};

use e2e::{Store, code, json, ok, stderr, stdout};

fn prefetched(s: &Store, args: &[&str]) -> String {
    let j = json(
        s.nix("nix")
            .args(["store", "prefetch-file", "--json"])
            .args(args),
    );
    j["hash"].as_str().unwrap().to_string()
}

fn nix_hash(s: &Store, args: &[&str], path: &str) -> String {
    stdout(&ok(s.nix("nix").arg("hash").args(args).arg(path)))
}

fn prefetch(s: &Store, args: &[&str]) -> String {
    stdout(&ok(s.iets().arg("prefetch").args(args)))
}

/// `src.tar.gz` of a small tree, and its URL.
fn tarball(s: &Store) -> (String, String) {
    s.write("src/a.txt", "hello\n");
    s.write("src/sub/b.txt", "sub\n");
    let tar = s.dir().join("src.tar.gz");
    ok(Command::new("tar")
        .arg("-czf")
        .arg(&tar)
        .arg("-C")
        .arg(s.dir())
        .arg("src"));
    let tar = tar.to_str().unwrap().to_string();
    (format!("file://{tar}"), tar)
}

#[test]
fn a_url() {
    let s = store!();
    let (url, tar) = tarball(&s);

    let flat = prefetch(&s, &[&url]);
    assert_eq!(flat, prefetched(&s, &[&url]));
    assert_eq!(flat, nix_hash(&s, &["file", "--sri"], &tar));

    let unpacked = prefetch(&s, &["--unpack", &url]);
    assert_eq!(unpacked, prefetched(&s, &["--unpack", &url]));
    let j = json(s.iets().args(["prefetch", "--json", "--unpack", &url]));
    assert_eq!(j, serde_json::json!({ "hash": unpacked, "url": url }));

    let sha512 = prefetch(&s, &["--hash-type", "sha512", &url]);
    assert_eq!(
        sha512,
        nix_hash(&s, &["file", "--sri", "--type", "sha512"], &tar)
    );
}

/// A fixed-output derivation that copies the bytes of `msg.txt`, with a
/// hash that is not the one it produces. With `dep` the bytes come from
/// another derivation, so the daemon builds a resolved copy and reports the
/// mismatch in the log.
const FOD: &str = r#"let src = @FROM@; in
derivation {
  name = "prefetch-msg";
  system = "@SYSTEM@";
  builder = "@SHELL@";
  args = [ "-c" "while IFS= read -r l; do echo \"$l\"; done < ${src} > $out" ];
  outputHash = "sha256-BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBA=";
  outputHashAlgo = "sha256";
  outputHashMode = "@MODE@";
}
"#;

const DEP: &str = r#"(derivation {
    name = "prefetch-dep";
    system = "@SYSTEM@";
    builder = "@SHELL@";
    args = [ "-c" "echo hello prefetch > $out" ];
  })"#;

fn prefetch_expr(s: &Store, args: &[&str]) -> Output {
    let builder = format!("builder = \"unix://{}\"", s.socket().display());
    s.iets()
        .env("IETS_CONFIG", builder)
        .arg("prefetch")
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn an_expression() {
    let s = store!();
    let msg = s.write("msg.txt", "hello prefetch\n");
    let msg = msg.to_str().unwrap();

    let fod = s.write(
        "fod.nix",
        &FOD.replace("@FROM@", DEP).replace("@MODE@", "flat"),
    );
    let r = prefetch_expr(&s, &[fod.to_str().unwrap()]);
    assert!(r.status.success(), "{}", stderr(&r));
    assert_eq!(stdout(&r), nix_hash(&s, &["file", "--sri"], msg));

    let from = format!("{}/msg.txt", s.dir().display());
    let fod = s.write(
        "fod.nix",
        &FOD.replace("@FROM@", &from).replace("@MODE@", "recursive"),
    );
    let r = prefetch_expr(&s, &["--json", fod.to_str().unwrap()]);
    assert!(r.status.success(), "{}", stderr(&r));
    let j = e2e::parse(&r);
    assert_eq!(j["hash"], nix_hash(&s, &["path", "--sri"], msg));
    assert!(j["drvPath"].as_str().unwrap().ends_with(".drv"), "{j}");
}

#[test]
fn sparse_members() {
    let s = store!();
    let forms: [(&str, &[&str]); 4] = [
        ("gnu", &["--format=gnu"]),
        ("pax0.0", &["--format=posix", "--sparse-version=0.0"]),
        ("pax0.1", &["--format=posix", "--sparse-version=0.1"]),
        ("pax1.0", &["--format=posix", "--sparse-version=1.0"]),
    ];
    let sp = s.dir().join("sp");
    fs::create_dir_all(sp.join("top")).unwrap();
    let hole = sp.join("top/hole");
    ok(Command::new("truncate").args(["-s", "4194304"]).arg(&hole));
    fs::OpenOptions::new()
        .append(true)
        .open(&hole)
        .and_then(|mut f| Write::write_all(&mut f, b"end\n"))
        .unwrap();
    for (tag, args) in forms {
        let tar = s.dir().join(format!("sp-{tag}.tar"));
        ok(Command::new("tar")
            .arg("-S")
            .args(args)
            .arg("-cf")
            .arg(&tar)
            .arg("-C")
            .arg(&sp)
            .arg("top"));
        let url = format!("file://{}", tar.display());
        let ours = prefetch(&s, &["--unpack", &url]);
        assert_eq!(ours, prefetched(&s, &["--unpack", &url]), "{tag}");
    }

    // the archive is 10 KiB and the file it names 4 MiB: the cap counts the
    // bytes the expansion writes
    let url = format!("file://{}/sp-gnu.tar", s.dir().display());
    let capped = s
        .iets()
        .args([
            "prefetch",
            "--unpack",
            "--iets-option",
            "fetch-max-size",
            "1048576",
            &url,
        ])
        .output()
        .unwrap();
    assert!(!capped.status.success());
    assert!(
        stderr(&capped).contains("fetch-max-size"),
        "{}",
        stderr(&capped)
    );
}

#[test]
fn errors() {
    let s = store!();
    let (url, _) = tarball(&s);
    let missing = format!("file://{}/nothing.tar.gz", s.dir().display());
    let codes = [
        code(s.iets().args(["prefetch", "--bogus"])),
        code(s.iets().args(["prefetch", "--hash-type", "sha7", &url])),
        code(s.iets().args(["prefetch", "--unpack", "-E", "1"])),
        code(s.iets().args(["prefetch", &missing])),
    ];
    assert_eq!(codes, [2, 2, 2, 1]);
}
