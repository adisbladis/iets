//! A read of a derivation output that is already in the store, under a
//! store directory of the test's own.
#![cfg(feature = "cli")]

mod hermetic;

use hermetic::{Iets, bin};
use rstest::rstest;
use std::fs;
use std::path::Path;
use std::process::Output;
use tempfile::tempdir;

/// `"hello"\n`, the content of the fixed output.
const HELLO_SHA256: &str = "0n1p3c7pvgamwbaan7r2rmk10jxs8wy45h7mbc89y4x44nyr1z0i";

/// The store directory in the evaluator's form, so a store path string is
/// absolute on Windows too: `C:\x` is `/C:/x` there (src/sys/winpath.rs).
fn store_dir(dir: &Path) -> String {
    let s = dir.join("store").to_str().unwrap().replace('\\', "/");
    if s.starts_with('/') {
        s
    } else {
        format!("/{s}")
    }
}

/// With the tracing cache on: only a run with a cache keeps the text of the
/// `.drv` that gives the output path.
fn iets(bin: &Iets, dir: &Path, args: &[&str]) -> Output {
    bin.cmd()
        .args(args)
        .current_dir(dir)
        .env("NIX_STORE_DIR", store_dir(dir))
        .env("IETS_CACHE_DIR", dir.join("cache"))
        .output()
        .expect("run iets")
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn fails_unbuilt(o: &Output) {
    let err = text(&o.stderr);
    assert!(!o.status.success(), "{err}");
    assert!(err.contains("is not built"), "{err}");
}

#[rstest]
fn an_output_in_the_store_is_read_with_a_warning(bin: Iets) {
    let d = tempdir().unwrap();
    let dir = d.path();
    fs::create_dir(dir.join("store")).unwrap();
    fs::write(
        dir.join("drv.nix"),
        format!(
            "derivation {{ name = \"x.nix\"; system = \"s\"; builder = \"b\"; outputHashMode = \"flat\"; outputHashAlgo = \"sha256\"; outputHash = \"{HELLO_SHA256}\"; }}\n"
        ),
    )
    .unwrap();
    fs::write(dir.join("out.nix"), "(import ./drv.nix).outPath\n").unwrap();
    fs::write(dir.join("read.nix"), "import (import ./drv.nix)\n").unwrap();

    let o = iets(&bin, dir, &["eval", "--raw", "out.nix"]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    let out = text(&o.stdout);

    fails_unbuilt(&iets(&bin, dir, &["eval", "read.nix"]));

    let file = dir.join("store").join(out.rsplit('/').next().unwrap());
    fs::write(&file, "\"hello\"\n").unwrap();
    let warning = format!("warning: [ifd-local-output] import from derivation: '{out}'");
    for _ in 0..2 {
        let o = iets(&bin, dir, &["eval", "read.nix"]);
        let err = text(&o.stderr);
        assert!(o.status.success(), "{err}");
        assert_eq!(text(&o.stdout).trim_end(), "\"hello\"");
        assert!(err.contains(&warning), "{err}");
    }

    fails_unbuilt(&iets(
        &bin,
        dir,
        &[
            "eval",
            "--option",
            "allow-import-from-derivation",
            "false",
            "read.nix",
        ],
    ));

    let o = iets(
        &bin,
        dir,
        &["eval", "--lint", "ifd-local-output=error", "read.nix"],
    );
    let err = text(&o.stderr);
    assert!(err.starts_with("error: [ifd-local-output]"), "{err}");

    fs::remove_file(&file).unwrap();
    fails_unbuilt(&iets(&bin, dir, &["eval", "read.nix"]));
}
