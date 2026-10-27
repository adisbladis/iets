//! What a run owns. A library must not keep this in process-wide statics:
//! two evaluators in one process each want their own settings, their own
//! log destination and their own mismatch list. `Session` holds all of it.
//!
//! An `Evaluator` keeps a handle to the session it was made in, so the hot
//! paths read a field. Code with no evaluator at hand — the command line,
//! a fetch, a build — uses [`Session::current`], the session this thread
//! entered; the library's entry points that take an evaluator enter its
//! session first (`Evaluator::enter`). A thread the library starts inherits
//! it through [`bind`]. docs/reference/library.md says what a host owns.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::error::Error as StdError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::{fmt, io, mem, panic, process};

use crate::env::Env;
use crate::events::Events;
use crate::ignored::Ignored;
use crate::jobs::Run;
use crate::log::Log;
use crate::profile::Profile;
use crate::provenance::SourceMap;
use crate::runtime::heap::Heap;
use crate::settings::Settings;
use crate::stdout::Stdout;
use crate::store::path::StorePath;
use crate::{config, error, treestore};

#[derive(Default)]
pub struct Session {
    pub log: Log,
    pub env: Env,
    pub command: Mutex<Option<&'static str>>,
    pub catch_exit: AtomicBool,
    pub out: Stdout,
    /// This run's sampled flame graph (`crate::profile`).
    pub profile: Profile,
    /// Where this run's instrumentation events go (`crate::events`).
    pub events: Events,
    settings: OnceLock<Settings>,
    /// What this run was asked for and does not do (`crate::ignored`).
    pub ignored: Ignored,
    /// The local tree store this run reads fetched trees from
    /// (`crate::treestore`).
    pub trees: treestore::Store,
    /// The `eval-jobs` walk of this run (`crate::jobs`).
    pub jobs: Run,
    /// nix.conf, read once per run: it cannot change under an evaluation.
    pub nix_conf: OnceLock<config::Config>,
    /// Store path to local tree, for the commands that build the map while
    /// the evaluation is live (`crate::provenance`).
    pub source_map: OnceLock<SourceMap>,
    /// The `parse-toml-timestamps` experimental feature.
    pub toml_timestamps: AtomicBool,
    /// Every arena chunk and symbol name of this session's evaluators.
    /// Released when the last of the session, its evaluators and its
    /// worker threads lets go (docs/reference/library.md).
    pub heap: Arc<Heap>,
    /// The cache of `store::locate::references_of`.
    pub(crate) store_refs: Mutex<HashMap<String, BTreeSet<StorePath>>>,
    /// Withdraws this session's work on a store (docs/reference/library.md).
    pub cancel: Cancel,
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<Session>>> = const { RefCell::new(None) };
}

/// The session of a thread that entered none: everything the library does
/// before a host makes one of its own. It writes nowhere.
fn fallback() -> Arc<Session> {
    static D: OnceLock<Arc<Session>> = OnceLock::new();
    D.get_or_init(|| Arc::new(Session::default())).clone()
}

impl Session {
    pub fn new() -> Arc<Session> {
        Arc::new(Session::default())
    }

    pub fn with_env(env: Env) -> Arc<Session> {
        Arc::new(Session {
            env,
            ..Session::default()
        })
    }

    /// The session this thread entered, else the fallback one.
    // never inlined: a fiber that migrates must not reuse another thread's slot
    #[inline(never)]
    pub fn current() -> Arc<Session> {
        CURRENT
            .with(|c| c.borrow().clone())
            .unwrap_or_else(fallback)
    }

    /// Makes `self` this thread's session until the guard drops.
    #[inline(never)]
    pub fn enter(self: &Arc<Session>) -> Entered {
        let prev = CURRENT.with(|c| c.borrow_mut().replace(self.clone()));
        Entered(prev)
    }

    /// This session's settings, read from its own environment whichever
    /// session the thread entered: the environment knobs alone until
    /// [`Session::load_settings`] reads the files (docs/reference/settings.md).
    pub fn settings(&self) -> &Settings {
        self.settings.get_or_init(|| Settings::of_env(&self.env))
    }

    /// The environment settings with `IETS_STATS` at `level`, for a test that
    /// reads the arena totals under its own session.
    #[cfg(test)]
    pub(crate) fn set_stats_for_test(&self, level: u8) {
        let s = Settings {
            stats: level,
            ..Settings::of_env(&self.env)
        };
        let _ = self.settings.set(s);
    }

    /// Reads the `iets.toml` settings files and applies the `--iets-option`
    /// pairs. Call it once, before anything reads a setting.
    pub fn load_settings(&self, options: &[(String, String)]) -> error::Result<()> {
        let s = Settings::load(&self.env, options)?;
        let _ = self.settings.set(s);
        Ok(())
    }

    /// nix.conf as this session's environment names it, read on first use
    /// unless the command line loaded it already.
    pub fn nix_conf(&self) -> &config::Config {
        self.nix_conf.get_or_init(|| {
            config::Config::read_in(&self.env, &[]).unwrap_or_else(|_| config::Config::empty())
        })
    }
}

/// A host's withdrawal of a session's work on a store. Once cancelled, an
/// upload, a build or a query in flight fails with `Error::Cancelled` as soon
/// as its connection is shut, and every later one fails at once. A clone
/// cancels the same session.
#[derive(Clone, Default)]
pub struct Cancel(Arc<Mutex<Aborts>>);

#[derive(Default)]
struct Aborts {
    cancelled: bool,
    next: u64,
    /// What shuts each open connection: a socket, an ssh child, a gRPC call.
    armed: Vec<(u64, Box<dyn FnOnce() + Send>)>,
}

impl Cancel {
    /// Shuts every connection the session has open to a store and refuses
    /// new ones. Any thread may call it, any number of times.
    pub fn cancel(&self) {
        let armed = {
            let mut a = self.lock();
            a.cancelled = true;
            mem::take(&mut a.armed)
        };
        // outside the lock, so a connection that drops meanwhile does not wait
        for (_, abort) in armed {
            abort();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.lock().cancelled
    }

    /// Runs `abort` on a cancel while the guard lives; `Error::Cancelled`
    /// when the session is cancelled already.
    pub(crate) fn arm(&self, abort: impl FnOnce() + Send + 'static) -> error::Result<Armed> {
        let mut a = self.lock();
        if a.cancelled {
            return Err(error::Error::Cancelled);
        }
        let id = a.next;
        a.next += 1;
        a.armed.push((id, Box::new(abort)));
        Ok(Armed {
            cancel: self.clone(),
            id,
        })
    }

    fn lock(&self) -> MutexGuard<'_, Aborts> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A connection's place in its session's [`Cancel`], given up on drop.
pub(crate) struct Armed {
    cancel: Cancel,
    id: u64,
}

impl Drop for Armed {
    fn drop(&mut self) {
        self.cancel.lock().armed.retain(|(id, _)| *id != self.id);
    }
}

/// The I/O error of a connection past a cancel (`is_cancelled_io`).
#[derive(Debug)]
struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("cancelled")
    }
}

impl StdError for Cancelled {}

pub(crate) fn cancelled_io() -> io::Error {
    io::Error::other(Cancelled)
}

pub(crate) fn is_cancelled_io(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|inner| inner.is::<Cancelled>())
}

pub struct Exit(pub i32);

/// Ends the process, or unwinds to `cli::run` when the host asked to catch it.
pub fn exit(code: i32) -> ! {
    if Session::current().catch_exit.load(Ordering::Relaxed) {
        panic::resume_unwind(Box::new(Exit(code)));
    }
    process::exit(code)
}

/// Puts back the session [`Session::enter`] replaced.
pub struct Entered(Option<Arc<Session>>);

impl Drop for Entered {
    #[inline(never)]
    fn drop(&mut self) {
        CURRENT.with(|c| *c.borrow_mut() = self.0.take());
    }
}

/// `f` on a new thread, with this thread's session entered there.
pub fn bind<R>(f: impl FnOnce() -> R) -> impl FnOnce() -> R {
    let s = Session::current();
    move || {
        let _entered = s.enter();
        f()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::api::{self, EvalArgs, Input};
    use crate::eval::{ErrorKind, EvalConfig, Evaluator, Pos};
    use crate::parallel::interner::SOFT_SYMS;
    use crate::runtime::Word;
    use crate::runtime::heap::{self, Exhausted, MemoryGroup, MemoryHost, Release, is_released};
    use crate::sink::Sink;
    use crate::{parallel, print};
    use std::env;
    use std::process::{self, Command};
    use std::sync::atomic::AtomicUsize;

    use crate::log;

    #[test]
    fn two_sessions_share_no_state() {
        let line = format!(
            "{}:\n  specified: sha256-A=\n     got:    sha256-B=",
            crate::log::MISMATCH
        );
        let (a, b) = (Session::new(), Session::new());
        {
            let _entered = a.enter();
            log::note_mismatch(&line);
            log::resolve_mismatches(&|_| true);
        }
        {
            let _entered = b.enter();
            assert!(crate::log::take_mismatches().is_empty());
        }
        let _entered = a.enter();
        assert_eq!(crate::log::take_mismatches().len(), 1);
    }

    /// A session with `IETS_OFFLINE` set or not, `connect-timeout` in its
    /// nix.conf, and a log of its own.
    fn session_with(dir: &std::path::Path, offline: bool, timeout: u64) -> (Arc<Session>, Sink) {
        let conf = format!("store = unix:///nonexistent/socket\nconnect-timeout = {timeout}");
        let mut env = Env::isolated().with([
            (
                "IETS_CACHE_DIR",
                dir.join(timeout.to_string()).into_os_string(),
            ),
            ("NIX_CONFIG", conf.into()),
        ]);
        if offline {
            env = env.with([("IETS_OFFLINE", "1")]);
        }
        let s = Session::with_env(env);
        let log = Sink::default();
        s.log.write_to(Box::new(log.clone()));
        (s, log)
    }

    /// Two sessions used from one thread that entered a third: each reads its
    /// own settings, nix.conf and log, and so do its evaluators.
    #[test]
    fn an_evaluator_reads_its_own_session_wherever_the_thread_is() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, "hello\n").unwrap();
        let (a, a_log) = session_with(dir.path(), true, 7);
        let (b, b_log) = session_with(dir.path(), false, 9);
        let (third, third_log) = session_with(dir.path(), false, 11);
        let _entered = third.enter();

        assert!(a.settings().offline && !b.settings().offline);
        let timeout = |s: &Session| s.nix_conf().int("connect-timeout").unwrap();
        assert_eq!((timeout(&a), timeout(&b)), (Some(7), Some(9)));

        let fetch = |name: &str| {
            let url = format!("file://{}", file.display());
            let hash = "sha256-WJG1tSLV3whtD/CxEPvZ0hu0/HFjrzTQgoai6Eb2vgM=";
            format!(
                r#"builtins.trace "in {name}" (builtins.fetchurl {{ url = "{url}"; sha256 = "{hash}"; }})"#
            )
        };
        let (mut ev_a, mut ev_b) = (
            EvalConfig::new().session(a.clone()).build(),
            EvalConfig::new().session(b.clone()).build(),
        );
        let refused = evaluate(&mut ev_a, &fetch("a")).err();
        assert_eq!(refused, Some(ErrorKind::Offline));
        let got = evaluate(&mut ev_b, &fetch("b")).expect("b is not offline");
        assert!(json(&mut ev_b, got).ends_with("-f\""));
        assert!(a_log.text().contains("trace: in a"), "{}", a_log.text());
        assert!(b_log.text().contains("trace: in b"), "{}", b_log.text());
        assert_eq!(third_log.text(), "");
        // each input store is under its own session's cache directory
        assert!(dir.path().join("9").exists() && !dir.path().join("11").exists());
    }

    fn eval_in(s: &Arc<Session>, expr: &str) -> (Evaluator, Word) {
        let mut ev = EvalConfig::new().session(s.clone()).build();
        let a = EvalArgs {
            input: Some(Input::Expr(expr.into())),
            no_ir_cache: true,
            no_tracing_cache: true,
            workers: 1,
            ..EvalArgs::new()
        };
        let w = api::evaluate(&mut ev, a).expect("evaluates");
        (ev, w)
    }

    fn json(ev: &mut Evaluator, w: Word) -> String {
        let mut out = Vec::new();
        print::print_json(ev, w, &mut out, &mut Vec::new(), true, Pos::NONE).expect("prints");
        String::from_utf8(out).unwrap()
    }

    const EXPR: &str = r#"builtins.genList (i: { n = i; s = "x${toString i}"; }) 2000"#;

    #[test]
    fn a_released_session_leaves_the_other_alone() {
        let (a, b) = (Session::new(), Session::new());
        let (ev_a, wa) = eval_in(&a, EXPR);
        let (mut ev_b, wb) = eval_in(&b, EXPR);
        let at = wa.bits() as usize & 0xffff_ffff_fff8;
        assert!(a.heap.bytes() > 0);
        drop(ev_a);
        drop(a);
        assert!(is_released(at));
        let (mut ev_c, wc) = eval_in(&Session::new(), EXPR);
        assert_eq!(json(&mut ev_b, wb), json(&mut ev_c, wc));
    }

    #[test]
    fn a_parallel_run_releases_its_session() {
        let s = Session::new();
        let ev = EvalConfig::new().session(s.clone()).build();
        let at = parallel::try_run(4, ev, |ev| {
            let a = EvalArgs {
                input: Some(Input::Expr(EXPR.into())),
                no_ir_cache: true,
                no_tracing_cache: true,
                ..EvalArgs::new()
            };
            let w = api::evaluate(ev, a).expect("evaluates");
            json(ev, w);
            w.bits() as usize & 0xffff_ffff_fff8
        })
        .expect("runs");
        assert!(!is_released(at));
        drop(s);
        assert!(is_released(at));
    }

    /// Allows up to `limit` bytes; a test changes the limit as it goes.
    struct Limit(Arc<AtomicUsize>);

    impl MemoryHost for Limit {
        fn allow(&self, total: usize) -> bool {
            total <= self.0.load(Ordering::Relaxed)
        }
    }

    fn evaluate(ev: &mut Evaluator, expr: &str) -> Result<Word, ErrorKind> {
        let a = EvalArgs {
            input: Some(Input::Expr(expr.into())),
            no_ir_cache: true,
            no_tracing_cache: true,
            workers: 1,
            ..EvalArgs::new()
        };
        api::evaluate(ev, a).map_err(|e| e.kind)
    }

    #[test]
    fn a_refused_chunk_fails_the_evaluation_and_the_session_goes_on() {
        let limit = Arc::new(AtomicUsize::new(usize::MAX));
        let group = MemoryGroup::new(Limit(limit.clone()));
        let s = Session::new();
        s.heap.join(&group);
        let mut ev = EvalConfig::new().session(s.clone()).build();
        let held = group.bytes();
        assert_eq!(held, s.heap.bytes());
        limit.store(held + (40 << 20), Ordering::Relaxed);
        // each step copies the list and is a call
        let grow = "builtins.foldl' (acc: i: acc ++ [ i ]) [ ] (builtins.genList (x: x) 20000)";
        let deep = format!("builtins.length ({grow})");
        assert_eq!(evaluate(&mut ev, &deep).err(), Some(ErrorKind::Budget));
        let tried = format!("(builtins.tryEval ({deep})).success");
        assert_eq!(evaluate(&mut ev, &tried).err(), Some(ErrorKind::Budget));
        // genList refuses before it allocates
        let before = group.bytes();
        let big = "builtins.length (builtins.genList (x: x) 100000000)";
        assert_eq!(evaluate(&mut ev, big).err(), Some(ErrorKind::Budget));
        assert!(group.bytes() - before <= 32 << 20);
        // once the host allows the size again, the evaluator goes on
        limit.store(usize::MAX, Ordering::Relaxed);
        let w = evaluate(&mut ev, "(x: x + 1) 1").expect("evaluates");
        assert_eq!(json(&mut ev, w), "2");
        drop(ev);
        drop(s);
        assert_eq!(group.bytes(), 0);
    }

    /// Helper fibers have arenas of their own: a refusal in any of them
    /// ends the run with the budget error and does not hang it.
    #[test]
    fn a_refusal_ends_a_parallel_run() {
        let limit = Arc::new(AtomicUsize::new(usize::MAX));
        let group = MemoryGroup::new(Limit(limit.clone()));
        let s = Session::new();
        s.heap.join(&group);
        let ev = EvalConfig::new().session(s.clone()).build();
        limit.store(group.bytes() + (64 << 20), Ordering::Relaxed);
        let grow = "builtins.foldl' (acc: i: acc ++ [ i ]) [ ] (builtins.genList (x: x) 3000)";
        let expr = format!("builtins.deepSeq (builtins.genList (i: {grow}) 64) 0");
        let kind = parallel::try_run(4, ev, |ev| {
            let a = EvalArgs {
                input: Some(Input::Expr(expr)),
                no_ir_cache: true,
                no_tracing_cache: true,
                ..EvalArgs::new()
            };
            api::evaluate(ev, a).err().map(|e| e.kind)
        })
        .expect("runs");
        assert_eq!(kind, Some(ErrorKind::Budget));
    }

    /// A mapping that fails after the host's release exhausts the session:
    /// its evaluation fails at its next call, its next evaluation fails at
    /// once, and the process and a new session go on.
    #[test]
    fn a_session_out_of_memory_fails_and_the_process_goes_on() {
        let _reserve = heap::tests::spending_the_reserve();
        let s = Session::new();
        let mut ev = EvalConfig::new().session(s.clone()).build();
        // each step copies the list and is a call
        let grow = "builtins.length (builtins.foldl' (acc: i: acc ++ [ i ]) [ ] (builtins.genList (x: x) 20000))";
        heap::tests::FAIL_MAPS.with(|f| f.set(2));
        assert_eq!(evaluate(&mut ev, grow).err(), Some(ErrorKind::Exhausted));
        assert_eq!(s.heap.exhausted(), Some(Exhausted::Memory));
        let next = evaluate(&mut ev, "(x: x + 1) 1").err();
        assert_eq!(next, Some(ErrorKind::Exhausted));
        drop(ev);
        drop(s);
        let (mut ev, w) = eval_in(&Session::new(), "(x: x + 1) 1");
        assert_eq!(json(&mut ev, w), "2");
    }

    /// Past `SOFT_SYMS` names the session is exhausted, and its next
    /// evaluation fails at its first call.
    #[test]
    fn a_session_past_the_symbol_limit_fails() {
        let s = Session::new();
        let mut ev = EvalConfig::new().session(s.clone()).build();
        ev.interner.skip_to_for_test(SOFT_SYMS - 3);
        let names = r#"builtins.attrNames (builtins.listToAttrs (map (i: { name = "n${toString i}"; value = i; }) (builtins.genList (x: x) 10)))"#;
        let _ = evaluate(&mut ev, names);
        assert_eq!(s.heap.exhausted(), Some(Exhausted::Symbols));
        let next = evaluate(&mut ev, "(x: x + 1) 1").err();
        assert_eq!(next, Some(ErrorKind::Exhausted));
    }

    #[test]
    fn a_group_returns_to_nothing_after_each_released_session() {
        let group = MemoryGroup::new(Limit(Arc::new(AtomicUsize::new(usize::MAX))));
        for _ in 0..8 {
            let s = Session::new();
            s.heap.join(&group);
            s.heap.set_release(Release::Unmap);
            let (ev, w) = eval_in(&s, EXPR);
            let at = w.bits() as usize & 0xffff_ffff_fff8;
            assert!(group.bytes() >= s.heap.bytes() && s.heap.bytes() > 0);
            drop(ev);
            drop(s);
            assert_eq!(group.bytes(), 0);
            assert!(!is_released(at));
        }
    }

    /// The child process uses a value of a released session; the parent
    /// checks that it died with the message and did not read freed memory.
    #[test]
    fn a_value_used_after_release_is_loud() {
        if env::var_os("IETS_TEST_STALE_CHILD").is_some() {
            let a = Session::new();
            let (ev_a, wa) = eval_in(&a, EXPR);
            drop(ev_a);
            drop(a);
            let (mut ev_b, _) = eval_in(&Session::new(), "1");
            let _ = json(&mut ev_b, wa);
            process::exit(0);
        }
        let out = Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "session::tests::a_value_used_after_release_is_loud",
            ])
            .env("IETS_TEST_STALE_CHILD", "1")
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{err}");
        assert!(
            err.contains("error: a value was used after its session was released"),
            "{err}"
        );
    }
}
