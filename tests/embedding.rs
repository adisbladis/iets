//! What a host that embeds the evaluator calls, through `iets::api` alone: a
//! session of its own with an isolated environment and bounded memory, a
//! pure evaluator on a thread with the demand fiber's stack, a value kept
//! across borrows, the upload plan of what the evaluation made, an upload the
//! host cancels, and the error kinds it tells apart
//! (docs/explanation/embedding-surface.md). The file names no other module
//! of the crate, and its last test holds it to that.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{fs, str, thread};

use iets::api::build::{self, Dispatch, Mode, Options, Provenance, Style};
use iets::api::memory::{Exhausted, Heap, MemoryGroup, MemoryHost, Release};
use iets::api::value::{Eval, Kept, Value};
use iets::api::{
    self, Cancel, Env, Error, ErrorKind, EvalConfig, EvalError, EvalResult, Evaluator, Report,
    Session, StorePath,
};
use tempfile::tempdir;

/// Allows the sessions of its group up to a limit the host moves.
struct Cap(Arc<AtomicUsize>);

impl MemoryHost for Cap {
    fn allow(&self, total: usize) -> bool {
        total <= self.0.load(Ordering::Relaxed)
    }
}

/// One session for one root: an environment of its own whose store answers
/// nowhere and which downloads nothing, its memory in `group` and unmapped
/// when it goes.
fn session(cache: &Path, group: &Arc<MemoryGroup>) -> Arc<Session> {
    let env = Env::isolated().with([
        (
            "NIX_CONFIG",
            "store = unix:///nonexistent/socket".to_string(),
        ),
        ("IETS_OFFLINE", "1".to_string()),
        ("IETS_CACHE_DIR", cache.display().to_string()),
    ]);
    let session = Session::with_env(env);
    let heap: &Heap = &session.heap;
    heap.join(group);
    heap.set_release(Release::Unmap);
    session
}

/// A pure evaluator that reports no lint and finds no source position.
fn evaluator(session: &Arc<Session>) -> Evaluator {
    EvalConfig::new()
        .session(session.clone())
        .pure(true)
        .keep_site(false)
        .no_lints()
        .build()
}

/// `f` on a thread with the stack the demand fiber has, so a deep evaluation
/// meets the call-depth error first.
fn on_main_stack<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    thread::scope(|s| {
        let t = thread::Builder::new().stack_size(api::MAIN_STACK);
        t.spawn_scoped(s, f).unwrap().join().unwrap()
    })
}

/// The store path the string `name` of `v` names.
fn path_at<'e>(e: &mut Eval<'e>, v: Value<'e>, name: &[u8]) -> EvalResult<StorePath> {
    let s = e.attr(v, name)?.expect("the attribute is there");
    let s = e.force(s)?;
    let text = str::from_utf8(s.as_bytes().expect("a string")).unwrap();
    Ok(StorePath::parse(text).expect("a store path"))
}

/// `src` evaluated and forced through.
fn eval(ev: &mut Evaluator, src: &str) -> EvalResult<()> {
    let mut e = Eval::new(ev);
    e.eval_expr(src, b"/")
        .and_then(|v| e.force_deep(v))
        .map(drop)
}

const ROOT: &str = r#"{
  greeting = builtins.toFile "greeting" "hello";
  drv = derivation { name = "x"; system = "x86_64-linux"; builder = "/bin/sh"; };
}"#;

#[test]
fn a_host_keeps_a_root_plans_its_paths_and_cancels_the_upload() {
    let dir = tempdir().unwrap();
    let group = MemoryGroup::new(Cap(Arc::new(AtomicUsize::new(usize::MAX))));
    on_main_stack(|| {
        let session = session(dir.path(), &group);
        let _entered = session.enter();
        let mut ev = evaluator(&session);

        // the root is evaluated once and read under a later borrow
        let kept: Kept = {
            let mut e = Eval::new(&mut ev);
            let root = e.eval_expr(ROOT, b"/").and_then(|v| e.force(v));
            e.keep(root.expect("evaluates"))
        };
        let mut e = Eval::new(&mut ev);
        let root = e.get(&kept).expect("kept by this evaluator");
        let greeting = path_at(&mut e, root, b"greeting").expect("a text");
        let drv = e.attr(root, b"drv").expect("evaluates").unwrap();
        let drv = e.force(drv).expect("evaluates");
        let drv = path_at(&mut e, drv, b"drvPath").expect("a derivation");

        // a root the evaluation did not make is dropped, and the rest planned
        let other = (greeting.to_full_string()).replace(
            greeting.base_name(),
            "00000000000000000000000000000000-other",
        );
        let other = StorePath::parse(&other).unwrap();
        match build::plan(e.evaluator(), &[other.clone(), greeting.clone()]) {
            Err(Error::UnknownRoot(p)) => assert_eq!(p, other),
            r => panic!("{r:?}"),
        }
        let roots = [greeting.clone(), drv.clone()];
        let plan = build::plan(e.evaluator(), &roots).expect("plans");
        let paths: Vec<&StorePath> = plan.paths().collect();
        assert_eq!(paths, [&greeting, &drv]);

        // the store the session's nix.conf names, which does not answer
        let cfg: build::Config = build::config();
        assert_eq!(cfg.store(), "unix:///nonexistent/socket");
        let transport = build::transport(None, &cfg).expect("a store URL");
        let probe = build::answers(&transport);
        assert!(matches!(probe, Err(Error::Io { .. })), "{probe:?}");

        // a cancel from another thread withdraws the session's store work
        let cancel: Cancel = session.cancel.clone();
        thread::spawn(move || cancel.cancel()).join().unwrap();
        let probe = build::answers(&transport);
        assert!(matches!(probe, Err(Error::Cancelled)), "{probe:?}");
        let quiet = Style {
            mode: Mode::Ci,
            color: false,
            fold: false,
            quiet: true,
        };
        let options = Options::default();
        let upload = Dispatch::open(&transport, &options, plan, Provenance::off(), quiet)
            .and_then(|mut d| d.upload_only(&roots));
        assert!(matches!(upload, Err(Error::Cancelled)), "{upload:?}");
    });
}

#[test]
fn a_host_tells_the_error_kinds_apart() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("f");
    fs::write(&file, "hello\n").unwrap();
    let limit = Arc::new(AtomicUsize::new(usize::MAX));
    let group = MemoryGroup::new(Cap(limit.clone()));
    on_main_stack(|| {
        let session = session(&dir.path().join("cache"), &group);
        let mut ev = evaluator(&session);

        // the program's own failure, rendered in Nix's words
        let thrown: Box<EvalError> = eval(&mut ev, r#"throw "no such package""#).unwrap_err();
        assert_eq!(thrown.kind, ErrorKind::Throw);
        let report: Report = api::report_of(&ev, &thrown);
        assert!(
            report.text().contains("no such package"),
            "{}",
            report.text()
        );

        // a download the session's IETS_OFFLINE refused
        let fetch = format!(
            r#"builtins.fetchurl {{ url = "file://{}"; sha256 = "sha256-WJG1tSLV3whtD/CxEPvZ0hu0/HFjrzTQgoai6Eb2vgM="; }}"#,
            file.display()
        );
        let kind = eval(&mut ev, &fetch).err().map(|e| e.kind);
        assert_eq!(kind, Some(ErrorKind::Offline));

        // the host's bound on the group's memory
        limit.store(group.bytes(), Ordering::Relaxed);
        let big = "builtins.length (builtins.genList (x: x) 100000000)";
        let kind = eval(&mut ev, big).err().map(|e| e.kind);
        assert_eq!(kind, Some(ErrorKind::Budget));

        // a refusal leaves the session usable, and it has not run out
        limit.store(usize::MAX, Ordering::Relaxed);
        assert_eq!(session.heap.exhausted(), None::<Exhausted>);
        eval(&mut ev, "(x: x + 1) 1").expect("evaluates");
    });
}

/// The calls above compile against `api` alone only while this file names no
/// other module of the crate.
#[test]
fn this_file_names_nothing_of_the_crate_outside_api() {
    let prefix = concat!("iets", "::");
    let text = include_str!("embedding.rs");
    for (at, _) in text.match_indices(prefix) {
        let rest = &text[at + prefix.len()..];
        let line = rest.lines().next().unwrap_or("");
        assert!(
            rest.starts_with("api"),
            "tests/embedding.rs names {prefix}{line}"
        );
    }
}
