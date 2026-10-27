//! Provenance end to end (docs/explanation/provenance.md): failures and build
//! logs name the working copy, not the store path.
#![cfg(all(unix, feature = "cli"))]

mod e2e;
mod hermetic;

use e2e::{Store, stderr, stdout};

/// A tree whose derivation fails after echoing its own source path.
fn make_case(s: &Store) -> String {
    s.write("case/src/a", "hello @NONCE@\n");
    s.write(
        "case/default.nix",
        r#"derivation {
  name = "prov-e2e-fail";
  system = "@SYSTEM@";
  builder = "@SHELL@";
  args = [ "-c" "echo source is ${./src}; exit 1" ];
}
"#,
    );
    format!("{}/case", s.dir().display())
}

/// A locked flake with one failing derivation and one throwing file.
fn make_flake(s: &Store) -> String {
    s.write(
        "flake/flake.nix",
        r#"{
  outputs = _: {
    packages.@SYSTEM@.boom = derivation {
      name = "prov-e2e-flake";
      system = "@SYSTEM@";
      builder = "@SHELL@";
      args = [ "-c" "echo tree is ${./.} @NONCE@; exit 1" ];
    };
  };
}
"#,
    );
    s.write(
        "flake/flake.lock",
        r#"{"nodes":{"root":{}},"root":"root","version":7}"#,
    );
    s.write("flake/bad.nix", r#"throw "boom from the tree""#);
    format!("{}/flake", s.dir().display())
}

#[test]
fn nix_build_names_the_local_tree() {
    let s = store!();
    let d = make_case(&s);
    let file = format!("{d}/default.nix");
    let r = s.iets().args(["nix-build", &file]).output().unwrap();
    let err = stderr(&r);
    assert!(!r.status.success());
    assert!(
        err.contains(&format!("defined at {d}/default.nix:")),
        "{err}"
    );
    assert!(err.contains(&format!("source is {d}/src")), "{err}");
    assert!(err.contains("prov-e2e-fail failed"), "{err}");

    let plain = s
        .iets()
        .env("IETS_PROVENANCE", "0")
        .args(["nix-build", &file])
        .output()
        .unwrap();
    let err = stderr(&plain);
    assert!(!err.contains("defined at"), "{err}");
    let store = format!("source is {}/", s.store_dir().display());
    assert!(err.contains(&store), "{err}");
}

#[test]
fn a_flake_names_the_working_copy() {
    let s = store!();
    let fd = make_flake(&s);
    let sys = e2e::system();
    let bad = s
        .iets()
        .args(["eval", "--no-tracing-cache", &format!("{fd}/bad.nix")])
        .output()
        .unwrap();
    assert!(
        stderr(&bad).contains(&format!("{fd}/bad.nix:1:1")),
        "{}",
        stderr(&bad)
    );

    // the flake tree is copied to the store, so the position comes back mapped
    let installable = format!("{fd}#packages.{sys}.boom");
    let fe = s
        .iets()
        .args([
            "build",
            "--no-tracing-cache",
            "--builder",
            "daemon",
            &installable,
        ])
        .output()
        .unwrap();
    let err = stderr(&fe);
    assert!(
        err.contains(&format!("defined at {fd}/flake.nix:")),
        "{err}"
    );
    assert!(err.contains(&format!("tree is {fd}")), "{err}");

    let jobs = |extra: &[&str]| {
        let o = s
            .iets()
            .args(["eval-jobs", "--no-tracing-cache"])
            .args(extra)
            .args(["--flake", &format!("{fd}#packages.{sys}")])
            .output()
            .unwrap();
        stdout(&o)
    };
    let with_pos = jobs(&["--provenance"]);
    assert!(
        with_pos.contains(&format!("\"pos\":\"{fd}/flake.nix:")),
        "{with_pos}"
    );
    assert!(!jobs(&[]).contains("\"pos\""));
}
