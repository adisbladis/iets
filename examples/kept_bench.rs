//! Timing harness for values kept across calls
//! (docs/explanation/evaluation.md, "Values kept across calls").
//!
//! `cargo run --release --example kept_bench -- <kept|again|fresh> <nixpkgs> <attr>...`
//!
//! Reads `outPath` of each attribute of an applied nixpkgs, one borrow per
//! read. `kept` applies nixpkgs once and keeps it, `again` applies it again
//! in the same evaluator for each read, and `fresh` in a new session for
//! each read. Prints one `mode reads failed wall_ms session_mb` line so
//! `bench/kept.nu` can parse it; `session_mb` is the largest session.

use std::sync::Arc;
use std::time::Instant;
use std::{env, process};

use iets::api::value::{Eval, Value};
use iets::api::{EvalConfig, Evaluator, Session};

fn die(msg: &str) -> ! {
    eprintln!("kept_bench: {msg}");
    process::exit(2);
}

fn evaluator(s: &Arc<Session>) -> Evaluator {
    EvalConfig::new()
        .session(s.clone())
        .keep_site(false)
        .build()
        .unwrap_or_else(|err| die(&err.to_string()))
}

fn apply<'e>(e: &mut Eval<'e>, src: &str) -> Value<'e> {
    let v = e.eval_expr(src, b"/").unwrap_or_else(|err| die(&err.msg));
    e.force(v).unwrap_or_else(|err| die(&err.msg))
}

/// `pkgs.<attr>.outPath`, or `None` when it fails or is no string.
fn out_path<'e>(e: &mut Eval<'e>, pkgs: Value<'e>, attr: &str) -> Option<&'e [u8]> {
    let drv = e.attr(pkgs, attr.as_bytes()).ok()??;
    let drv = e.force(drv).ok()?;
    let path = e.attr(drv, b"outPath").ok()??;
    e.force(path).ok()?.as_bytes()
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() < 3 {
        die("usage: kept_bench <kept|again|fresh> <nixpkgs> <attr>...");
    }
    let (mode, attrs) = (args[0].as_str(), &args[2..]);
    let src = format!("import {} {{ system = \"x86_64-linux\"; }}", args[1]);

    let t = Instant::now();
    let mut ok = 0;
    let mut bytes = 0;
    match mode {
        "kept" => {
            let s = Session::new();
            let mut ev = evaluator(&s);
            let root = {
                let mut e = Eval::new(&mut ev);
                let pkgs = apply(&mut e, &src);
                e.keep(pkgs)
            };
            for a in attrs {
                let mut e = Eval::new(&mut ev);
                let pkgs = e.get(&root).expect("kept by this evaluator");
                ok += out_path(&mut e, pkgs, a).is_some() as usize;
            }
            bytes = s.heap.bytes();
        }
        "again" => {
            let s = Session::new();
            let mut ev = evaluator(&s);
            for a in attrs {
                let mut e = Eval::new(&mut ev);
                let pkgs = apply(&mut e, &src);
                ok += out_path(&mut e, pkgs, a).is_some() as usize;
            }
            bytes = s.heap.bytes();
        }
        "fresh" => {
            for a in attrs {
                let s = Session::new();
                let mut ev = evaluator(&s);
                let mut e = Eval::new(&mut ev);
                let pkgs = apply(&mut e, &src);
                ok += out_path(&mut e, pkgs, a).is_some() as usize;
                bytes = bytes.max(s.heap.bytes());
            }
        }
        _ => die("mode is kept, again or fresh"),
    }
    println!(
        "{mode} {} {} {} {}",
        attrs.len(),
        attrs.len() - ok,
        t.elapsed().as_millis(),
        bytes >> 20
    );
}
