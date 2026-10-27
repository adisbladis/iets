//! A store path the evaluation made by copying is a root of the upload plan:
//! `api::build::plan` and `Dispatch::upload_only` make it valid on a daemon,
//! with its references, beside the derivations of the same plan. One test,
//! because the evaluation runs in this process and takes the daemon's store
//! directory (`pin_store_dir`).
#![cfg(all(unix, feature = "cli"))]

mod e2e;
mod hermetic;

use std::fs;
use std::path::Path;

use e2e::{Store, ok, stdout};
use iets::api::build::{self, Dispatch, Options, Style, Transport};
use iets::api::value::Eval;
use iets::build::render::Mode;
use iets::env::Env;
use iets::error::Error;
use iets::eval::{EvalConfig, Evaluator};
use iets::provenance::Provenance;
use iets::session::Session;
use iets::store::path::{StorePath, pin_store_dir};
use iets::sys::path_to_internal;
use serde_json::Value;

/// The store path the string `src` evaluates to.
fn eval_path(ev: &mut Evaluator, src: &str, dir: &Path) -> StorePath {
    let mut e = Eval::new(ev);
    let v = e
        .eval_expr(src, &path_to_internal(dir))
        .and_then(|v| e.force(v))
        .unwrap_or_else(|err| panic!("{src}: {}", err.msg));
    let s = std::str::from_utf8(v.as_bytes().expect("a string")).unwrap();
    StorePath::parse(s).unwrap()
}

/// The path Nix gives `src`, from a store that keeps what is added in
/// memory, so the daemon's store gets nothing from Nix.
fn nix_path(s: &Store, src: &str) -> String {
    let store = format!("dummy://?store={}&read-only=false", s.store_dir().display());
    let o = s
        .nix("nix")
        .args([
            "eval", "--impure", "--raw", "--store", &store, "--expr", src,
        ])
        .output()
        .unwrap();
    assert!(o.status.success(), "{src}: {}", e2e::stderr(&o));
    stdout(&o)
}

/// `nix path-info --json` of `p` on the daemon, `None` when it is not valid,
/// which Nix prints as `null`.
fn path_info(s: &Store, p: &StorePath) -> Option<Value> {
    let o = s
        .nix("nix")
        .args(["path-info", "--json"])
        .arg(p.to_string())
        .output()
        .unwrap();
    if !o.status.success() {
        return None;
    }
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    let info = match v {
        Value::Array(a) => a.into_iter().next(),
        Value::Object(m) => m.into_iter().next().map(|(_, i)| i),
        _ => None,
    };
    info.filter(|i| !i.is_null())
}

fn open(s: &Store, plan: build::Plan) -> Dispatch {
    let url = format!("unix://{}", s.socket().display());
    let style = Style {
        mode: Mode::Ci,
        color: false,
        fold: false,
        quiet: true,
    };
    let t = Transport::parse(&url).unwrap();
    Dispatch::open(&t, &Options::default(), plan, Provenance::off(), style).unwrap()
}

/// `p` is valid on the daemon, content-addressed as `ca` says, with the NAR
/// hash of what the store holds and exactly `refs` as its references.
fn check_valid(s: &Store, p: &StorePath, ca: &str, refs: &[&StorePath]) {
    let info = path_info(s, p).unwrap_or_else(|| panic!("{p} is not valid"));
    let got_ca = info["ca"].as_str().unwrap_or("");
    assert!(got_ca.starts_with(ca), "{p}: ca {got_ca}");
    let on_disk = s.store_dir().join(p.base_name());
    let hash = stdout(&ok(s.nix("nix").args(["hash", "path"]).arg(&on_disk)));
    assert_eq!(info["narHash"], hash, "{p}");
    let mut got: Vec<&str> = (info["references"].as_array().unwrap().iter())
        .map(|r| r.as_str().unwrap())
        .collect();
    got.sort();
    let mut want: Vec<String> = refs.iter().map(|r| r.to_string()).collect();
    want.sort();
    assert_eq!(got, want, "{p}");
}

#[test]
fn a_copied_path_is_uploaded_with_its_references() {
    let Some(s) = Store::start_with(&[]) else {
        return;
    };
    pin_store_dir(s.store_dir().to_str().unwrap());
    let up = s.dir().join("up");
    for (f, text) in [
        ("tree/a", "a\n"),
        ("tree/sub/b", "b\n"),
        ("file", "flat\n"),
        ("other/c", "c\n"),
        ("inner/d", "d\n"),
        ("mixed/e", "e\n"),
    ] {
        let p = up.join(f);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }
    let session = Session::with_env(Env::isolated());
    let mut ev = EvalConfig::new().session(session.clone()).build();

    // each kind of copy, by the expression that makes it
    let cases = [
        ("copy", r#""${./up/tree}""#, "fixed:r:sha256:"),
        (
            "path",
            r#"builtins.path { path = ./up/tree; name = "named"; }"#,
            "fixed:r:sha256:",
        ),
        (
            "filterSource",
            r#"builtins.filterSource (p: t: baseNameOf p != "b") ./up/tree"#,
            "fixed:r:sha256:",
        ),
        (
            "flat",
            "builtins.path { path = ./up/file; recursive = false; }",
            "fixed:sha256:",
        ),
        (
            "toFile",
            r#"builtins.toFile "text" "see ${./up/other}""#,
            "text:sha256:",
        ),
    ];
    let mut roots = Vec::new();
    for (what, src, _) in &cases {
        let p = eval_path(&mut ev, src, s.dir());
        assert_eq!(p.to_string(), nix_path(&s, src), "{what}");
        roots.push(p);
    }
    let other = eval_path(&mut ev, r#""${./up/other}""#, s.dir());
    let plan = build::plan(&ev, &roots).unwrap();
    assert!(plan.drvs.is_empty());
    let at = |p: &StorePath| plan.sources.iter().position(|q| q == p).unwrap();
    assert!(at(&other) < at(&roots[4]), "a text after what it names");
    for p in &roots {
        assert!(
            path_info(&s, p).is_none(),
            "{p} was valid before the upload"
        );
    }
    open(&s, plan).upload_only(&roots).unwrap();
    for ((what, _, ca), p) in cases.iter().zip(&roots) {
        let refs: &[&StorePath] = if *what == "toFile" { &[&other] } else { &[] };
        check_valid(&s, p, ca, refs);
    }
    check_valid(&s, &other, "fixed:r:sha256:", &[]);

    // a derivation and a copy in one plan; the drv names a text whose tree no
    // drv names, which must land before the text
    let drv = eval_path(
        &mut ev,
        r#"(derivation {
          name = "mixed";
          system = "x86_64-linux";
          builder = "/bin/sh";
          args = [ "-c" "cat ${builtins.toFile "inner" "${./up/inner}"} > $out" ];
        }).drvPath"#,
        s.dir(),
    );
    let copy = eval_path(&mut ev, r#""${./up/mixed}""#, s.dir());
    let inner_tree = eval_path(&mut ev, r#""${./up/inner}""#, s.dir());
    let inner = eval_path(
        &mut ev,
        r#"builtins.toFile "inner" "${./up/inner}""#,
        s.dir(),
    );
    let roots = [drv.clone(), copy.clone()];
    let plan = build::plan(&ev, &roots).unwrap();
    open(&s, plan).upload_only(&roots).unwrap();
    check_valid(&s, &copy, "fixed:r:sha256:", &[]);
    check_valid(&s, &inner_tree, "fixed:r:sha256:", &[]);
    check_valid(&s, &inner, "text:sha256:", &[&inner_tree]);
    let info = path_info(&s, &drv).expect("the drv is valid");
    let refs = info["references"].as_array().unwrap();
    assert!(refs.iter().any(|r| r == &inner.to_string()), "{info}");

    // a path no evaluation of this session added, even one the store holds
    let unknown = |p: &StorePath, ev: &Evaluator| match build::plan(ev, &[drv.clone(), p.clone()]) {
        Err(Error::UnknownRoot(q)) => assert_eq!(&q, p),
        Err(e) => panic!("{p}: {e}"),
        Ok(_) => panic!("{p} was planned"),
    };
    let base = format!("{}-unknown", "0".repeat(32));
    unknown(&StorePath::from_base_name(&base).unwrap(), &ev);
    let fresh = EvalConfig::new()
        .session(Session::with_env(Env::isolated()))
        .build();
    unknown(&copy, &fresh);
}
