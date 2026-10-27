//! The parse target: accept or refuse, the position of a refusal, and the
//! desugared tree of an acceptance.
//!
//! Oracle frames (`parse-oracle`): request u32 length, then the source.
//! Reply u8 kind (0 parsed, 1 error), u32 line, u32 column, u32 length,
//! then the tree or the message.

use crate::oracle::{self, Oracle};
use crate::{Divergence, Target, Verdict, canon, classify, known, no_panic, sx, sx::Sx};
use iets::syntax::{self, ast::FileId, symbol::Interner};

pub const KINDS: &[&str] = &[
    // Nix parses it, iets refuses it
    "nix-only",
    // iets parses it, Nix refuses it
    "iets-only",
    // both refuse it, at different places
    "position",
    // both parse it, to different trees
    "tree",
    "iets-panic",
    // the oracle died: Nix itself crashes on the input
    "nix-crash",
];

pub enum Side {
    Parsed(Sx),
    Error { line: u32, col: u32, msg: String },
}

pub struct Parse {
    oracle: Oracle,
    interner: Interner,
    known: Vec<known::Entry>,
}

impl Parse {
    pub fn new(known: Vec<known::Entry>) -> Parse {
        Parse {
            oracle: Oracle::from_env("IETS_PARSE_ORACLE", "parse-oracle"),
            interner: Interner::new().unwrap(),
            known,
        }
    }

    pub fn iets(&mut self, src: &[u8]) -> Result<Side, String> {
        let interner = &mut self.interner;
        no_panic(|| match syntax::parse(src, FileId(0), interner) {
            Ok(ast) => Side::Parsed(sx::normalise(canon::tree(&ast, interner))),
            Err(e) => {
                let (line, col) = syntax::line_col(src, e.span.start);
                Side::Error {
                    line,
                    col,
                    msg: e.msg,
                }
            }
        })
    }

    pub fn nix(&mut self, src: &[u8]) -> Result<Side, String> {
        let mut frame = (src.len() as u32).to_le_bytes().to_vec();
        frame.extend_from_slice(src);
        let (kind, line, col, body) = self.oracle.ask(&frame, |out| {
            let kind = oracle::bytes(out, 1)?[0];
            let (line, col) = (oracle::u32_le(out)?, oracle::u32_le(out)?);
            let n = oracle::u32_le(out)? as usize;
            Ok((kind, line, col, oracle::bytes(out, n)?))
        })?;
        if kind == 0 {
            return sx::parse(&body)
                .map(|t| Side::Parsed(sx::normalise(t)))
                .map_err(|e| format!("unreadable oracle tree: {e}"));
        }
        Ok(Side::Error {
            line,
            col,
            msg: String::from_utf8_lossy(&body).into_owned(),
        })
    }

    fn compare(&mut self, src: &[u8]) -> Option<Divergence> {
        let d = |kind, nix, iets, at| {
            Some(Divergence {
                kind,
                nix,
                iets,
                at,
            })
        };
        let nix = match self.nix(src) {
            Ok(n) => n,
            Err(why) => return d("nix-crash", why, String::new(), String::new()),
        };
        let iets = match self.iets(src) {
            Ok(i) => i,
            Err(msg) => return d("iets-panic", show(&nix), msg, String::new()),
        };
        let (kind, at) = match (&nix, &iets) {
            (Side::Parsed(a), Side::Parsed(b)) => match sx::first_difference(a, b) {
                None => return None,
                Some((at, ..)) => ("tree", at),
            },
            (
                Side::Error {
                    line: l1, col: c1, ..
                },
                Side::Error {
                    line: l2, col: c2, ..
                },
            ) => {
                if *l1 == 0 || (l1, c1) == (l2, c2) {
                    return None;
                }
                ("position", String::new())
            }
            (Side::Parsed(_), Side::Error { .. }) => ("nix-only", String::new()),
            (Side::Error { .. }, Side::Parsed(_)) => ("iets-only", String::new()),
        };
        d(kind, show(&nix), show(&iets), at)
    }
}

impl Target for Parse {
    fn check(&mut self, src: &[u8]) -> Verdict {
        let d = self.compare(src);
        let v = classify(&self.known, d, src);
        if !matches!(v, Verdict::Bug(_)) {
            return v;
        }
        // Both sides keep a symbol table across inputs, and which of two
        // clashing names an error reports follows symbol order. A file is
        // parsed in a fresh process, so a bug counts only when it holds
        // with fresh tables. The restart costs milliseconds; only a bug
        // pays it.
        self.oracle.restart();
        self.interner = Interner::new().unwrap();
        let d = self.compare(src);
        classify(&self.known, d, src)
    }
}

pub fn show(s: &Side) -> String {
    match s {
        Side::Parsed(t) => sx::print(t),
        Side::Error { line, col, msg } => format!("error at {line}:{col}: {msg}"),
    }
}
