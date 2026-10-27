//! The eval target. The input chooses a small expression of a known type
//! (`generate`). iets and Nix 2.34.8 (`eval-oracle`) evaluate it in pure
//! mode and force it deeply. The two must print the same value, as
//! `nix-instantiate --eval --strict` prints it, or both must fail. Error
//! messages are not compared.
//!
//! The expressions are simply typed and no binding refers to itself, so
//! each evaluation ends. A value of the wrong type is put in now and then,
//! to compare the type errors.
//!
//! Oracle frames: request u32 length, then the source. Reply u8 kind (0
//! value, 1 error, 2 error after which the oracle exits), u32 length, then
//! the value or the message.

use crate::oracle::{self, Oracle};
use crate::{Divergence, Target, Verdict, classify, known, no_panic};
use iets::eval::{EvalConfig, Evaluator};
use iets::print;
use libfuzzer_sys::arbitrary::Unstructured;
use std::collections::{BTreeSet, HashSet};
use std::{env, fs};

pub const KINDS: &[&str] = &[
    // Nix gives a value, iets an error
    "nix-only",
    // iets gives a value, Nix an error
    "iets-only",
    // both give a value, and the values differ
    "value",
    "iets-panic",
    // the oracle died: Nix itself crashes on the input
    "nix-crash",
    // iets-only or value, and iets with Nix's `==` rules (`nix_eq`) gives
    // Nix's answer
    "eq-rule",
];

/// The reply kind of an error after which the oracle exits: a stack
/// overflow, where `nix-instantiate` stops too.
const EXITS: u8 = 2;

/// A value, or the error message.
type Answer = Result<String, String>;

pub struct Eval {
    oracle: Oracle,
    known: Vec<known::Entry>,
    generate: fn(&mut Unstructured) -> String,
}

thread_local! {
    /// One for each thread: an evaluator is not `Send`, and its arena is
    /// never freed (runtime/arena.rs), so one per input would not do.
    static EV: std::cell::RefCell<Evaluator> =
        std::cell::RefCell::new(EvalConfig::new().pure(true).build().unwrap());
}

impl Eval {
    pub fn new(known: Vec<known::Entry>) -> Eval {
        Eval::with(known, generate)
    }

    /// The same comparison, of the expressions `generate` makes.
    pub fn with(known: Vec<known::Entry>, generate: fn(&mut Unstructured) -> String) -> Eval {
        Eval {
            oracle: Oracle::from_env("IETS_EVAL_ORACLE", "eval-oracle"),
            known,
            generate,
        }
    }

    fn nix(&mut self, src: &str) -> Result<Answer, String> {
        let mut frame = (src.len() as u32).to_le_bytes().to_vec();
        frame.extend_from_slice(src.as_bytes());
        let (kind, text) = self.oracle.ask(&frame, |out| {
            let kind = oracle::bytes(out, 1)?[0];
            let n = oracle::u32_le(out)? as usize;
            Ok((
                kind,
                String::from_utf8_lossy(&oracle::bytes(out, n)?).into_owned(),
            ))
        })?;
        if kind == EXITS {
            self.oracle.restart();
        }
        Ok(if kind == 0 { Ok(text) } else { Err(text) })
    }

    fn verdict(&mut self, src: &str) -> Verdict {
        let d = self.compare(src);
        classify(&self.known, d, src.as_bytes())
    }

    /// Evaluates `src` on both sides and compares.
    pub fn compare(&mut self, src: &str) -> Option<Divergence> {
        let iets = no_panic(|| EV.with_borrow_mut(|ev| iets(ev, src)));
        let nix = self.nix(src);
        let d = |kind, nix: String, iets: String| Divergence {
            kind,
            nix,
            iets,
            at: src.to_string(),
        };
        match (nix, iets) {
            (Err(e), i) => Some(d("nix-crash", e, i.map(|a| show(&a)).unwrap_or_default())),
            (Ok(n), Err(p)) => Some(d("iets-panic", show(&n), p)),
            (Ok(Ok(n)), Ok(Err(i))) => Some(d("nix-only", n, format!("error: {i}"))),
            (Ok(Err(n)), Ok(Ok(i))) => Some(d("iets-only", format!("error: {n}"), i)),
            (Ok(Ok(n)), Ok(Ok(i))) if n != i => Some(d("value", n, i)),
            _ => None,
        }
    }
}

fn show(a: &Answer) -> String {
    match a {
        Ok(v) => v.clone(),
        Err(m) => format!("error: {m}"),
    }
}

fn iets(ev: &mut Evaluator, src: &str) -> Answer {
    let v = ev
        .eval_string(src, b"/case")
        .and_then(|v| ev.force_deep(v))
        .map_err(|e| e.msg.clone())?;
    let mut out = Vec::new();
    print::print_ambiguous(ev, v, 1, &mut out, &mut HashSet::default(), 0)
        .map_err(|e| e.msg.clone())?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

impl Target for Eval {
    fn check(&mut self, input: &[u8]) -> Verdict {
        let src = (self.generate)(&mut Unstructured::new(input));
        if let Ok(p) = env::var("IETS_EVAL_SOURCE") {
            let _ = fs::write(p, &src);
        }
        let v = self.verdict(&src);
        if !matches!(v, Verdict::Bug(_)) {
            return v;
        }
        // Each side keeps its symbol table across inputs, and `==` on two
        // sets compares the attributes in symbol order: which of a false
        // and a failing attribute it reaches first follows the inputs
        // before. A bug counts only when it holds with fresh state, as in
        // a fresh `nix-instantiate`. A fresh evaluator leaks its arena, so
        // it is made only when a fresh oracle does not settle the case.
        self.oracle.restart();
        let v = self.verdict(&src);
        if !matches!(v, Verdict::Bug(_)) {
            return v;
        }
        EV.set(EvalConfig::new().pure(true).build().unwrap());
        let v = self.verdict(&src);
        let Verdict::Bug(mut d) = v else { return v };
        if !matches!(d.kind, "iets-only" | "value") {
            return Verdict::Bug(d);
        }
        // the `==` shortcut gives a value where Nix forces and fails, or
        // equals what Nix does not; with Nix's rules the answer is Nix's
        let exact = no_panic(|| {
            EV.with_borrow_mut(|ev| {
                ev.nix_eq = true;
                let a = iets(ev, &src);
                ev.nix_eq = false;
                a
            })
        });
        let agrees = match exact {
            Ok(Ok(i)) => d.kind == "value" && i == d.nix,
            Ok(Err(_)) => d.kind == "iets-only",
            Err(_) => false,
        };
        if agrees {
            d.kind = "eq-rule";
        }
        classify(&self.known, Some(d), src.as_bytes())
    }
}

#[derive(Clone, PartialEq, Debug)]
enum Ty {
    Int,
    Float,
    Bool,
    Str,
    Null,
    List(Box<Ty>),
    /// Fields sorted by name.
    Attrs(Vec<(&'static str, Ty)>),
    Fn(Box<Ty>, Box<Ty>),
}

struct Var {
    name: String,
    ty: Ty,
    /// Bound by `with`: any lexical binding of the name hides it.
    with: bool,
    /// In scope, so it hides outer bindings of the name, but not to be
    /// used: a `let` or `rec` binding whose value is not made yet.
    hidden: bool,
}

/// Field names, which `ORDER` puts in one symbol order on both sides.
const FIELDS: &[&str] = &["", "1", "a", "a b", "b", "c", "p", "q", "x"];
const NAMES: &[&str] = &["x", "y", "z", "f", "g", "s"];

const INTS: &[&str] = &[
    "0",
    "1",
    "2",
    "3",
    "7",
    "42",
    "(-1)",
    "9223372036854775807",
    "(-9223372036854775807 - 1)",
    "4611686018427387904",
    "1000000",
];
const FLOATS: &[&str] = &[
    "0.1",
    "0.2",
    "0.5",
    "1.5",
    "2.0",
    "(-0.0)",
    "1.0e300",
    "1.0e-300",
    "123456.789",
    "3.141592653589793",
    "1.0e16",
    "100000.0",
    "1000000.0",
    ".5",
];
const STRS: &[&str] = &[
    "",
    "a",
    "b",
    "abc",
    "a b",
    "h\u{e9}llo",
    "a\"b",
    "\\",
    "$",
    "${",
    "\n",
    "\t",
    "1.2.3",
    "1.10",
    "1.2pre",
    "/a/b",
    "a/b/",
    "/",
    "hello-2.12.3",
    "foo-bar-1.0pre",
    "0",
    "-",
    "x",
    "''",
    "'",
    "\u{1f600}",
];
const HASHES: &[&str] = &["md5", "sha1", "sha256", "sha512"];
const REGEXES: &[&str] = &[
    "a(b*)c",
    "(.*)",
    "[a-z]+",
    "([0-9]+)\\.([0-9]+)",
    "(a)|b",
    "",
    ".",
    "(x)?",
    "([^-]*)-(.*)",
    "[[:space:]]+",
    "(",
];

/// Names every field first. `==` on two sets compares their attributes in
/// the evaluator's symbol order, which follows the order it first saw the
/// names. Parsed first, the fields have the same order on both sides, for
/// every input (`docs/reference/divergences.md`, "Which attribute's error
/// comes first").
const ORDER: &str = "let _order = { \"\" = 0; \"1\" = 0; a = 0; \"a b\" = 0; b = 0; c = 0; p = 0; q = 0; x = 0; }; in ";

/// The expression `input` chooses; see the module comment.
pub fn generate(u: &mut Unstructured) -> String {
    let mut g = Gen {
        u,
        vars: Vec::new(),
        budget: 120,
        heavy: 2,
    };
    let mut ty = g.ty(2);
    while matches!(ty, Ty::Fn(..)) {
        ty = g.ty(2);
    }
    format!("{ORDER}{}", g.expr(&ty, 6))
}

struct Gen<'a, 'b> {
    u: &'a mut Unstructured<'b>,
    vars: Vec<Var>,
    /// Expressions left to make; at zero only literals and variables.
    budget: u32,
    /// `misc` uses left. Each `toXML` or `split` of the one before can
    /// make a string many times longer, so a chain grows exponentially.
    heavy: u8,
}

impl Gen<'_, '_> {
    fn pick(&mut self, n: usize) -> usize {
        if n <= 1 {
            return 0;
        }
        self.u.choose_index(n).unwrap_or(0)
    }

    fn one<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[self.pick(xs.len())]
    }

    fn odds(&mut self, n: u8) -> bool {
        self.u.ratio(1u8, n).unwrap_or(false)
    }

    fn small(&mut self, max: u8) -> u8 {
        self.u.int_in_range(0..=max).unwrap_or(0)
    }

    fn ty(&mut self, depth: u32) -> Ty {
        let n = if depth == 0 { 5 } else { 10 };
        match self.pick(n) {
            0 | 5 => Ty::Int,
            1 => Ty::Float,
            2 | 6 => Ty::Str,
            3 => Ty::Bool,
            4 => Ty::Null,
            7 => Ty::List(Box::new(self.ty(depth - 1))),
            9 => Ty::Fn(Box::new(self.ty(depth - 1)), Box::new(self.ty(depth - 1))),
            _ => {
                let mut fs: Vec<(&'static str, Ty)> = Vec::new();
                for _ in 0..1 + self.small(2) {
                    let f = self.one(FIELDS);
                    if !fs.iter().any(|(n, _)| *n == f) {
                        let t = self.ty(depth - 1);
                        fs.push((f, t));
                    }
                }
                fs.sort_by(|a, b| a.0.cmp(b.0));
                Ty::Attrs(fs)
            }
        }
    }

    fn sortable(&mut self) -> Ty {
        [
            Ty::Int,
            Ty::Float,
            Ty::Str,
            Ty::List(Box::new(Ty::Int)),
            Ty::List(Box::new(Ty::Str)),
        ][self.pick(5)]
        .clone()
    }

    /// A value of no fixed type, from builtins whose result is awkward to
    /// type; it goes to toJSON, toXML or typeOf.
    fn misc(&mut self, d: u32) -> String {
        let s = Ty::Str;
        match self.pick(10) {
            0 | 1 => {
                let f = self.one(&["builtins.match", "builtins.split"]);
                format!(
                    "({f} \"{}\" {})",
                    escape(self.one(REGEXES)),
                    self.expr(&s, d)
                )
            }
            2 => {
                let t = self.ty(1);
                let f = self.one(&["builtins.partition", "builtins.groupBy"]);
                let r = if f == "builtins.groupBy" {
                    s.clone()
                } else {
                    Ty::Bool
                };
                format!(
                    "({f} {} {})",
                    self.expr(&Ty::Fn(Box::new(t.clone()), Box::new(r)), d),
                    self.expr(&Ty::List(Box::new(t)), d)
                )
            }
            3 => {
                let t = self.ty(1);
                format!(
                    "(builtins.zipAttrsWith (n: vs: vs) {})",
                    self.expr(&Ty::List(Box::new(t)), d)
                )
            }
            4 => format!(
                "(builtins.genericClosure {{ startSet = [ {{ key = {}; }} ]; operator = i: if i.key < {} then [ {{ key = i.key + 1; }} {{ key = i.key; }} ] else [ ]; }})",
                self.small(3),
                self.small(5)
            ),
            5 => {
                let t = self.ty(1);
                let n = self.one(FIELDS);
                format!(
                    "(builtins.unsafeGetAttrPos \"{}\" {})",
                    escape(n),
                    self.expr(&t, d)
                )
            }
            6 => {
                let a = self.ty(1);
                let r = self.ty(1);
                format!(
                    "(builtins.functionArgs {})",
                    self.expr(&Ty::Fn(Box::new(a), Box::new(r)), d)
                )
            }
            7 => {
                let a = self.ty(1);
                let b = self.ty(1);
                format!(
                    "(builtins.intersectAttrs {} {})",
                    self.expr(&a, d),
                    self.expr(&b, d)
                )
            }
            _ => {
                let t = self.ty(2);
                self.expr(&t, d)
            }
        }
    }

    fn fresh(&mut self) -> String {
        self.one(NAMES).to_string()
    }

    fn bind(&mut self, name: &str, ty: &Ty) {
        self.vars.push(Var {
            name: name.into(),
            ty: ty.clone(),
            with: false,
            hidden: false,
        });
    }

    fn bind_hidden(&mut self, name: &str, ty: &Ty) {
        self.bind(name, ty);
        self.vars.last_mut().unwrap().hidden = true;
    }

    /// Makes the `k`th binding from the end usable.
    fn unhide(&mut self, k: usize) {
        let i = self.vars.len() - k;
        self.vars[i].hidden = false;
    }

    /// Makes the last `n` bindings usable.
    fn reveal(&mut self, n: usize) {
        let len = self.vars.len();
        for v in &mut self.vars[len - n..] {
            v.hidden = false;
        }
    }

    fn unbind(&mut self, n: usize) {
        self.vars.truncate(self.vars.len() - n);
    }

    /// The variables of type `ty` that the name resolves to: the innermost
    /// lexical binding wins, then the innermost `with`.
    fn visible(&self, ty: &Ty) -> Vec<String> {
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for (i, v) in self.vars.iter().enumerate().rev() {
            let resolved = if v.with {
                !self.vars.iter().any(|w| !w.with && w.name == v.name)
                    && !self.vars[i + 1..]
                        .iter()
                        .any(|w| w.with && w.name == v.name)
            } else {
                seen.insert(v.name.clone())
            };
            if resolved && !v.hidden && v.ty == *ty && !out.contains(&v.name) {
                out.push(v.name.clone());
            }
        }
        out
    }

    fn expr(&mut self, ty: &Ty, depth: u32) -> String {
        if self.budget == 0 || depth == 0 || self.u.is_empty() {
            let vs = self.visible(ty);
            if !vs.is_empty() && self.odds(2) {
                return vs[self.pick(vs.len())].clone();
            }
            return self.leaf(ty);
        }
        self.budget -= 1;
        if self.odds(48) {
            let other = self.ty(1);
            return self.leaf(&other);
        }
        let d = depth - 1;
        // half general forms, half the forms of the type
        match self.pick(24) {
            0 => {
                let vs = self.visible(ty);
                if vs.is_empty() {
                    self.leaf(ty)
                } else {
                    vs[self.pick(vs.len())].clone()
                }
            }
            1 => self.let_in(ty, d),
            2 => format!(
                "(if {} then {} else {})",
                self.expr(&Ty::Bool, d),
                self.expr(ty, d),
                self.expr(ty, d)
            ),
            3 => {
                let a = self.ty(1);
                let f = self.expr(&Ty::Fn(Box::new(a.clone()), Box::new(ty.clone())), d);
                format!("({f} {})", self.expr(&a, d))
            }
            4 => self.select(ty, d),
            5 => self.with(ty, d),
            6 => {
                let t = self.ty(1);
                let seq = self.one(&["builtins.seq", "builtins.deepSeq"]);
                format!("({seq} {} {})", self.expr(&t, d), self.expr(ty, d))
            }
            7 => self.lazy(ty, d),
            8 => {
                // mostly true, and `&&` must not force its right side
                let c = self.expr(&Ty::Bool, d);
                let c = if self.odds(4) {
                    c
                } else {
                    format!("(!(false && {c}))")
                };
                format!("(assert {c}; {})", self.expr(ty, d))
            }
            9 => {
                let r = self.fresh();
                let inner = self.expr(ty, d);
                let alt = self.expr(ty, d);
                format!(
                    "(let {r} = builtins.tryEval {inner}; in if {r}.success then {r}.value else {alt})"
                )
            }
            10 => {
                let l = self.expr(&Ty::List(Box::new(ty.clone())), d);
                let e = self.expr(ty, d);
                match self.pick(8) {
                    0 => format!("(builtins.head {l})"),
                    1 => format!("(builtins.elemAt {l} {})", self.one(INTS)),
                    2..4 => format!("(builtins.head ([ {e} ] ++ {l}))"),
                    _ => {
                        let f = self.expr(ty, d);
                        format!("(builtins.elemAt [ {e} {f} ] {})", self.small(1))
                    }
                }
            }
            11 => {
                let t = self.ty(1);
                let f = Ty::Fn(
                    Box::new(ty.clone()),
                    Box::new(Ty::Fn(Box::new(t.clone()), Box::new(ty.clone()))),
                );
                // a short literal list: an accumulator that doubles each
                // step must not see the 40 items of a splitVersion
                let f = self.expr(&f, d);
                let z = self.expr(ty, d);
                let xs: Vec<String> = (0..self.small(3)).map(|_| self.expr(&t, d)).collect();
                format!("(builtins.foldl' {f} {z} [ {} ])", xs.join(" "))
            }
            _ => self.typed(ty, d),
        }
    }

    fn let_in(&mut self, ty: &Ty, d: u32) -> String {
        // `let` is recursive: every name is in scope in every value, so a
        // value uses only the bindings before it, never itself
        let mut names: Vec<String> = Vec::new();
        for _ in 0..1 + self.small(1) {
            let n = self.fresh();
            // `let x = 1; x = 2;` is an error on both sides; keep it rare
            if !names.contains(&n) || self.odds(16) {
                names.push(n);
            }
        }
        let tys: Vec<Ty> = names.iter().map(|_| self.ty(1)).collect();
        for (n, t) in names.iter().zip(&tys) {
            self.bind_hidden(n, t);
        }
        let mut binds = String::new();
        for (i, (n, t)) in names.iter().zip(&tys).enumerate() {
            let e = self.expr(t, d);
            binds += &format!("{n} = {e}; ");
            let k = names.len() - i;
            self.unhide(k);
        }
        let body = self.expr(ty, d);
        self.unbind(names.len());
        format!("(let {binds}in {body})")
    }

    fn select(&mut self, ty: &Ty, d: u32) -> String {
        let f = self.one(FIELDS);
        let mut fs = match self.ty(1) {
            Ty::Attrs(fs) => fs,
            _ => vec![],
        };
        fs.retain(|(n, _)| *n != f);
        let has = !self.odds(4);
        if has {
            fs.push((f, ty.clone()));
            fs.sort_by(|a, b| a.0.cmp(b.0));
        }
        let a = self.expr(&Ty::Attrs(fs), d);
        let name = attr_name(f);
        if has && self.odds(2) {
            format!("({a}).{name}")
        } else {
            format!("(({a}).{name} or {})", self.expr(ty, d))
        }
    }

    fn with(&mut self, ty: &Ty, d: u32) -> String {
        let Ty::Attrs(fs) = self.ty(1) else {
            return self.expr(ty, d);
        };
        let a = self.expr(&Ty::Attrs(fs.clone()), d);
        let mut n = 0;
        for (name, t) in &fs {
            if is_ident(name) {
                self.vars.push(Var {
                    name: name.to_string(),
                    ty: t.clone(),
                    with: true,
                    hidden: false,
                });
                n += 1;
            }
        }
        let body = self.expr(ty, d);
        self.unbind(n);
        format!("(with {a}; {body})")
    }

    /// `ty`, next to an error that is never forced.
    fn lazy(&mut self, ty: &Ty, d: u32) -> String {
        let e = self.expr(ty, d);
        match self.pick(5) {
            0 => format!("(let unused = throw \"no\"; in {e})"),
            1 => format!("((unused: {e}) (throw \"no\"))"),
            2 => format!("(builtins.elemAt [ (throw \"no\") {e} ] 1)"),
            3 => format!("({{ a = throw \"no\"; b = {e}; }}.b)"),
            _ => format!("(builtins.head [ {e} (abort \"no\") ])"),
        }
    }

    fn typed(&mut self, ty: &Ty, d: u32) -> String {
        match ty {
            Ty::Int => self.int(d),
            Ty::Float => self.float(d),
            Ty::Bool => self.bool(d),
            Ty::Str => self.str(d),
            Ty::Null => self.leaf(ty),
            Ty::List(t) => self.list(t, d),
            Ty::Attrs(fs) => self.attrs(fs, d),
            Ty::Fn(a, r) => self.func(a, r, d),
        }
    }

    fn int(&mut self, d: u32) -> String {
        let i = Ty::Int;
        match self.pick(12) {
            0 => {
                let op = self.one(&["+", "-", "*", "/"]);
                format!("({} {op} {})", self.expr(&i, d), self.expr(&i, d))
            }
            1 => {
                let f = self.one(&[
                    "builtins.add",
                    "builtins.sub",
                    "builtins.mul",
                    "builtins.div",
                    "builtins.bitAnd",
                    "builtins.bitOr",
                    "builtins.bitXor",
                ]);
                format!("({f} {} {})", self.expr(&i, d), self.expr(&i, d))
            }
            2 => format!("(-{})", self.expr(&i, d)),
            3 => {
                let t = self.ty(1);
                format!("(builtins.length {})", self.expr(&Ty::List(Box::new(t)), d))
            }
            4 => format!("(builtins.stringLength {})", self.expr(&Ty::Str, d)),
            5 => {
                let f = self.one(&["builtins.floor", "builtins.ceil"]);
                format!("({f} {})", self.expr(&Ty::Float, d))
            }
            6 => format!(
                "(builtins.compareVersions {} {})",
                self.expr(&Ty::Str, d),
                self.expr(&Ty::Str, d)
            ),
            7 => format!(
                "(builtins.fromJSON \"{}\")",
                self.one(INTS).trim_matches(['(', ')'])
            ),
            _ => self.leaf(&i),
        }
    }

    fn float(&mut self, d: u32) -> String {
        let f = Ty::Float;
        match self.pick(6) {
            0 => {
                let op = self.one(&["+", "-", "*", "/"]);
                let a = self.expr(&f, d);
                let b = if self.odds(2) {
                    self.expr(&Ty::Int, d)
                } else {
                    self.expr(&f, d)
                };
                if self.odds(2) {
                    format!("({a} {op} {b})")
                } else {
                    format!("({b} {op} {a})")
                }
            }
            1 => format!("(-{})", self.expr(&f, d)),
            2 => {
                let j = self.one(&[
                    "1.5",
                    "1e3",
                    "-0.0",
                    "1.5e-7",
                    "12345678901234567890",
                    "0.1",
                ]);
                format!("(builtins.fromJSON \"{j}\")")
            }
            _ => self.leaf(&f),
        }
    }

    fn bool(&mut self, d: u32) -> String {
        let b = Ty::Bool;
        match self.pick(12) {
            0 => {
                let t = self.sortable();
                let op = self.one(&["<", "<=", ">", ">="]);
                format!("({} {op} {})", self.expr(&t, d), self.expr(&t, d))
            }
            1 => {
                let t = self.ty(2);
                let op = self.one(&["==", "!="]);
                format!("({} {op} {})", self.expr(&t, d), self.expr(&t, d))
            }
            2 => {
                let op = self.one(&["&&", "||", "->"]);
                format!("({} {op} {})", self.expr(&b, d), self.expr(&b, d))
            }
            3 => format!("(!{})", self.expr(&b, d)),
            4 => {
                let t = self.ty(1);
                let a = self.expr(&t, d);
                let f = self.one(FIELDS);
                format!("(({a}) ? {})", attr_name(f))
            }
            5 => {
                let t = self.ty(2);
                let is = self.one(&[
                    "isInt",
                    "isFloat",
                    "isBool",
                    "isString",
                    "isNull",
                    "isList",
                    "isAttrs",
                    "isFunction",
                    "isPath",
                ]);
                format!("(builtins.{is} {})", self.expr(&t, d))
            }
            6 => {
                let t = self.ty(1);
                format!(
                    "(builtins.elem {} {})",
                    self.expr(&t, d),
                    self.expr(&Ty::List(Box::new(t.clone())), d)
                )
            }
            7 => {
                let t = self.ty(1);
                let q = self.one(&["builtins.all", "builtins.any"]);
                format!(
                    "({q} {} {})",
                    self.expr(&Ty::Fn(Box::new(t.clone()), Box::new(b.clone())), d),
                    self.expr(&Ty::List(Box::new(t)), d)
                )
            }
            8 => {
                let t = self.ty(2);
                format!("(builtins.tryEval {}).success", self.expr(&t, d))
            }
            9 => {
                let t = self.ty(1);
                format!(
                    "(builtins.hasAttr \"{}\" {})",
                    escape(self.one(FIELDS)),
                    self.expr(&t, d)
                )
            }
            _ => self.leaf(&b),
        }
    }

    fn str(&mut self, d: u32) -> String {
        let s = Ty::Str;
        match self.pick(18) {
            0 => format!("({} + {})", self.expr(&s, d), self.expr(&s, d)),
            1 => {
                let a = self.expr(&s, d);
                let b = self.expr(&s, d);
                let (l, m, r) = (self.text(), self.text(), self.text());
                if self.odds(3) {
                    format!("''{}${{{a}}}{}${{{b}}}{}''", ind(&l), ind(&m), ind(&r))
                } else {
                    format!(
                        "\"{}${{{a}}}{}${{{b}}}{}\"",
                        escape(&l),
                        escape(&m),
                        escape(&r)
                    )
                }
            }
            2 => {
                let ts = [
                    Ty::Int,
                    Ty::Float,
                    Ty::Bool,
                    Ty::Null,
                    Ty::Str,
                    Ty::List(Box::new(Ty::Int)),
                    Ty::List(Box::new(Ty::Str)),
                ];
                let t = ts[self.pick(ts.len())].clone();
                format!("(toString {})", self.expr(&t, d))
            }
            3 => format!(
                "(builtins.substring {} {} {})",
                self.expr(&Ty::Int, d),
                self.expr(&Ty::Int, d),
                self.expr(&s, d)
            ),
            4 => {
                let l = Ty::List(Box::new(s.clone()));
                format!(
                    "(builtins.replaceStrings {} {} {})",
                    self.expr(&l, d),
                    self.expr(&l, d),
                    self.expr(&s, d)
                )
            }
            5 => format!(
                "(builtins.concatStringsSep {} {})",
                self.expr(&s, d),
                self.expr(&Ty::List(Box::new(s.clone())), d)
            ),
            6 => {
                let t = self.ty(2);
                format!("(builtins.typeOf {})", self.expr(&t, d))
            }
            7 => {
                let t = self.ty(2);
                format!("(builtins.toJSON {})", self.expr(&t, d))
            }
            8 => format!(
                "(builtins.hashString \"{}\" {})",
                self.one(HASHES),
                self.expr(&s, d)
            ),
            9 => {
                let t = self.ty(2);
                format!(
                    "(builtins.toJSON (builtins.fromJSON (builtins.toJSON {})))",
                    self.expr(&t, d)
                )
            }
            10..12 if self.heavy > 0 => {
                self.heavy -= 1;
                let m = self.misc(d);
                let f = self.one(&["builtins.toJSON", "builtins.toXML", "builtins.typeOf"]);
                format!("({f} {m})")
            }
            12 => {
                let f = self.one(&["builtins.baseNameOf", "builtins.dirOf"]);
                format!("({f} {})", self.expr(&s, d))
            }
            13 => {
                let f = self.one(&["name", "version"]);
                format!("(builtins.parseDrvName {}).{f}", self.expr(&s, d))
            }
            _ => self.leaf(&s),
        }
    }

    fn list(&mut self, t: &Ty, d: u32) -> String {
        let l = Ty::List(Box::new(t.clone()));
        match self.pick(14) {
            0 => format!("({} ++ {})", self.expr(&l, d), self.expr(&l, d)),
            1 => {
                let a = self.ty(1);
                format!(
                    "(map {} {})",
                    self.expr(&Ty::Fn(Box::new(a.clone()), Box::new(t.clone())), d),
                    self.expr(&Ty::List(Box::new(a)), d)
                )
            }
            2 => format!(
                "(builtins.filter {} {})",
                self.expr(&Ty::Fn(Box::new(t.clone()), Box::new(Ty::Bool)), d),
                self.expr(&l, d)
            ),
            3 => format!("(builtins.tail {})", self.expr(&l, d)),
            4 => format!(
                "(builtins.genList {} {})",
                self.expr(&Ty::Fn(Box::new(Ty::Int), Box::new(t.clone())), d),
                self.small(4)
            ),
            5 => format!(
                "(builtins.concatLists {})",
                self.expr(&Ty::List(Box::new(l.clone())), d)
            ),
            6 => {
                let a = self.ty(1);
                format!(
                    "(builtins.concatMap {} {})",
                    self.expr(&Ty::Fn(Box::new(a.clone()), Box::new(l.clone())), d),
                    self.expr(&Ty::List(Box::new(a)), d)
                )
            }
            7 if matches!(t, Ty::Int | Ty::Float | Ty::Str) => {
                let op = self.one(&["<", ">", "<=", ">="]);
                format!("(builtins.sort (p: q: p {op} q) {})", self.expr(&l, d))
            }
            8 => {
                let f = self.one(FIELDS);
                let fs = vec![(f, t.clone())];
                format!(
                    "(builtins.catAttrs \"{}\" {})",
                    escape(f),
                    self.expr(&Ty::List(Box::new(Ty::Attrs(fs))), d)
                )
            }
            9 if *t == Ty::Str => {
                if self.odds(2) {
                    format!("(builtins.splitVersion {})", self.expr(t, d))
                } else {
                    let a = self.ty(1);
                    format!("(builtins.attrNames {})", self.expr(&a, d))
                }
            }
            10 => {
                let fs: Vec<_> = (0..self.small(2))
                    .map(|i| (FIELDS[i as usize], t.clone()))
                    .collect();
                format!("(builtins.attrValues {})", self.expr(&Ty::Attrs(fs), d))
            }
            _ => {
                let n = self.small(3);
                let xs: Vec<String> = (0..n).map(|_| self.expr(t, d)).collect();
                format!("[ {} ]", xs.join(" "))
            }
        }
    }

    fn attrs(&mut self, fs: &[(&'static str, Ty)], d: u32) -> String {
        match self.pick(8) {
            0 => {
                // fields from the left, the right or both: the right wins
                let (mut l, mut r) = (Vec::new(), Vec::new());
                for (n, t) in fs {
                    match self.pick(3) {
                        0 => l.push((*n, t.clone())),
                        1 => r.push((*n, t.clone())),
                        _ => {
                            let other = self.ty(1);
                            l.push((*n, other));
                            r.push((*n, t.clone()));
                        }
                    }
                }
                format!(
                    "({} // {})",
                    self.expr(&Ty::Attrs(l), d),
                    self.expr(&Ty::Attrs(r), d)
                )
            }
            1 => {
                let items: Vec<String> = fs
                    .iter()
                    .map(|(n, t)| {
                        format!(
                            "{{ name = \"{}\"; value = {}; }}",
                            escape(n),
                            self.expr(t, d)
                        )
                    })
                    .collect();
                format!("(builtins.listToAttrs [ {} ])", items.join(" "))
            }
            2 => {
                let extra = self.one(FIELDS);
                let mut all = fs.to_vec();
                if !all.iter().any(|(n, _)| *n == extra) {
                    let t = self.ty(1);
                    all.push((extra, t));
                    all.sort_by(|a, b| a.0.cmp(b.0));
                    format!(
                        "(builtins.removeAttrs {} [ \"{}\" ])",
                        self.expr(&Ty::Attrs(all), d),
                        escape(extra)
                    )
                } else {
                    self.attrs_lit(fs, d)
                }
            }
            3 if fs.windows(2).all(|w| w[0].1 == w[1].1) && !fs.is_empty() => {
                let t = fs[0].1.clone();
                let a = self.ty(1);
                let src: Vec<_> = fs.iter().map(|(n, _)| (*n, a.clone())).collect();
                let (n, v) = (self.fresh(), self.fresh());
                self.bind(&n, &Ty::Str);
                self.bind(&v, &a);
                let body = self.expr(&t, d);
                self.unbind(2);
                format!(
                    "(builtins.mapAttrs ({n}: {v}: {body}) {})",
                    self.expr(&Ty::Attrs(src), d)
                )
            }
            4 => self.rec(fs, d),
            5 => {
                // `inherit` from a `let` and from a set
                let lets: Vec<bool> = fs
                    .iter()
                    .map(|(f, _)| is_ident(f) && self.odds(2))
                    .collect();
                let names: Vec<_> = fs
                    .iter()
                    .zip(&lets)
                    .filter(|(_, l)| **l)
                    .map(|(f, _)| f.clone())
                    .collect();
                for (f, t) in &names {
                    self.bind_hidden(f, t);
                }
                let mut binds = String::new();
                for (i, (f, t)) in names.iter().enumerate() {
                    binds += &format!("{f} = {}; ", self.expr(t, d));
                    let k = names.len() - i;
                    self.unhide(k);
                }
                let mut body = Vec::new();
                for ((f, t), l) in fs.iter().zip(&lets) {
                    if *l {
                        body.push(format!("inherit {f};"));
                    } else if is_ident(f) && self.odds(2) {
                        let src = self.expr(&Ty::Attrs(vec![(*f, t.clone())]), d);
                        body.push(format!("inherit ({src}) {f};"));
                    } else {
                        body.push(format!("{} = {};", attr_name(f), self.expr(t, d)));
                    }
                }
                self.unbind(names.len());
                if binds.is_empty() {
                    format!("{{ {} }}", body.join(" "))
                } else {
                    format!("(let {binds}in {{ {} }})", body.join(" "))
                }
            }
            _ => self.attrs_lit(fs, d),
        }
    }

    fn attrs_lit(&mut self, fs: &[(&'static str, Ty)], d: u32) -> String {
        let mut out = String::from("{ ");
        for (n, t) in fs {
            match t {
                // `a.b = 1; a.c = 2;` builds the same set
                Ty::Attrs(inner) if !inner.is_empty() && self.odds(3) => {
                    for (m, u) in inner {
                        let v = self.expr(u, d);
                        out += &format!("{}.{} = {v}; ", attr_name(n), attr_name(m));
                    }
                }
                _ => {
                    let v = self.expr(t, d);
                    if self.odds(8) {
                        out += &format!("${{\"{}\"}} = {v}; ", escape(n));
                    } else {
                        out += &format!("{} = {v}; ", attr_name(n));
                    }
                }
            }
        }
        out + "}"
    }

    /// `rec { }`: a field sees the fields before it.
    fn rec(&mut self, fs: &[(&'static str, Ty)], d: u32) -> String {
        let idents: Vec<_> = fs.iter().filter(|(f, _)| is_ident(f)).collect();
        for (f, t) in &idents {
            self.bind_hidden(f, t);
        }
        let mut out = String::from("rec { ");
        let mut done = 0;
        for (f, t) in fs {
            let v = self.expr(t, d);
            out += &format!("{} = {v}; ", attr_name(f));
            if is_ident(f) {
                let k = idents.len() - done;
                self.unhide(k);
                done += 1;
            }
        }
        self.unbind(idents.len());
        out + "}"
    }

    fn func(&mut self, a: &Ty, r: &Ty, d: u32) -> String {
        match (a, self.pick(4)) {
            (Ty::Attrs(fs), 0) if fs.iter().all(|(n, _)| is_ident(n)) && !fs.is_empty() => {
                // a pattern: each field required or with a default
                // the defaults see the arguments; none uses one
                let at = self.odds(2).then(|| self.fresh());
                for (n, t) in fs {
                    self.bind_hidden(n, t);
                }
                if let Some(x) = &at {
                    self.bind_hidden(x, a);
                }
                let mut pat = Vec::new();
                for (n, t) in fs {
                    if self.odds(3) {
                        let def = self.expr(t, d);
                        pat.push(format!("{n} ? {def}"));
                    } else {
                        pat.push(n.to_string());
                    }
                }
                self.reveal(fs.len() + at.is_some() as usize);
                let body = self.expr(r, d);
                self.unbind(fs.len() + at.is_some() as usize);
                let dots = if self.odds(2) { ", ..." } else { "" };
                match at {
                    Some(x) => format!("({{ {}{dots} }} @ {x}: {body})", pat.join(", ")),
                    None => format!("({{ {}{dots} }}: {body})", pat.join(", ")),
                }
            }
            (Ty::Int, 1) if *r == Ty::Int => {
                let f = self.one(&["builtins.add", "builtins.mul", "builtins.sub"]);
                format!("({f} {})", self.expr(&Ty::Int, d))
            }
            _ => {
                let x = self.fresh();
                self.bind(&x, a);
                let body = self.expr(r, d);
                self.unbind(1);
                format!("({x}: {body})")
            }
        }
    }

    fn text(&mut self) -> String {
        if self.odds(2) {
            String::new()
        } else {
            self.one(STRS).to_string()
        }
    }

    fn leaf(&mut self, ty: &Ty) -> String {
        match ty {
            Ty::Int => self.one(INTS).to_string(),
            Ty::Float => self.one(FLOATS).to_string(),
            Ty::Bool => self.one(&["true", "false"]).to_string(),
            Ty::Null => "null".into(),
            Ty::Str => {
                let s = self.text();
                if self.odds(4) {
                    format!("''{}''", ind(&s))
                } else {
                    format!("\"{}\"", escape(&s))
                }
            }
            Ty::List(t) => {
                if self.odds(2) {
                    "[ ]".into()
                } else {
                    format!("[ {} ]", self.leaf(t))
                }
            }
            Ty::Attrs(fs) => {
                let mut out = String::from("{ ");
                for (n, t) in fs {
                    let v = self.leaf(t);
                    out += &format!("{} = {v}; ", attr_name(n));
                }
                out + "}"
            }
            Ty::Fn(_, r) => format!("(_: {})", self.leaf(r)),
        }
    }
}

fn is_ident(s: &str) -> bool {
    let mut c = s.chars();
    matches!(c.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && c.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '\'' | '-'))
}

fn attr_name(s: &str) -> String {
    if is_ident(s) {
        s.into()
    } else {
        format!("\"{}\"", escape(s))
    }
}

/// The text of `s` in a `"..."` string.
fn escape(s: &str) -> String {
    let mut o = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => o += "\\\"",
            '\\' => o += "\\\\",
            '\n' => o += "\\n",
            '\t' => o += "\\t",
            '$' if chars.peek() == Some(&'{') => o += "\\$",
            c => o.push(c),
        }
    }
    o
}

/// The text of `s` in a `''...''` string.
fn ind(s: &str) -> String {
    s.replace("''", "'''").replace("${", "''${")
}
