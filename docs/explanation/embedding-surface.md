# What a host calls outside `api`

`crate::api` is the library facade: a frontend calls it and never reaches
into `cli` (`ARCHITECTURE.md`, "Layering"). A host that embeds the evaluator
in a long-lived program reaches further. The host below is a daemon that
keeps one session for each root it evaluates. For each item it calls, the
table gives what the host needs it for, whether `api` offers it another way,
and what `api` would have to name for the host to stay inside it: the input
to the choice of a stable API.

Nothing in `lib.rs` keeps a host inside `api`: about forty modules are
`pub`, and `tests/layering.rs` holds only `src/cli/` to the facade. Two of
the items come from changes on other branches: `api::value::Kept`, a value
kept across `Eval` borrows, and `error::Error::UnknownRoot`, the refusal of
`api::build::plan` for a root the evaluation did not make.

## Item by item

| What the host calls | What for | In `api` | What `api` would name |
|---|---|---|---|
| `eval::{EvalConfig, Evaluator}` | Build an evaluator in a session of the host's, pure, with no lints and no site search; keep it for the session's life | No. `docs/reference/library.md` documents them under `eval`. | `api::{EvalConfig, Evaluator}`, re-exported as they are |
| `eval::EvalResult`, through `eval::Error` and `ErrorKind` | Tell an evaluation error from a fault of the host by its kind (`offline`, `exhausted`, `budget`) | No: `api::evaluate` returns `Box<eval::Error>`, a type outside `api` | `api::{EvalError, ErrorKind, EvalResult}` |
| `api::report_of`, then `log::Report::text` | Render an error in Nix's words | The call is in `api`; the type it returns is `log::Report` | `api::Report`, with `text()` |
| `session::Session`: `with_env`, `enter`, `heap`, `cancel`, `catch_exit` | One session per root, its environment, its memory, its cancel | No | `api::Session`. `catch_exit` can go from the host's list: only the command line's `session::exit` reads it, and no library call reaches that |
| `env::Env::{isolated, with}` | A session that reads none of the host's environment but the two variables it adds (`NIX_CONFIG`, `IETS_OFFLINE`) | No | `api::Env` |
| `runtime::heap::Release` (and `MemoryGroup`, `MemoryHost`, `Exhausted`) | Give a released session's memory back; bound and watch the sessions' memory | No. `docs/reference/library.md` documents them under `runtime::heap` | `api::memory::{Release, MemoryGroup, MemoryHost, Exhausted}` |
| `lint::Config::off` | An evaluator that reports no lint | No. `EvalConfig::lints` takes a `lint::Config` | `EvalConfig::no_lints()`, or `api::LintConfig` |
| `parallel::fiber::MAIN_STACK` | The stack of the thread the host evaluates on, with one worker: the same room the demand fiber has, so a deep evaluation meets the call-depth error first | No | `api::MAIN_STACK`, or a `std::thread::Builder` with the stack set |
| `store::path::StorePath`: `parse`, `to_full_string`, `base_name` | Name the roots of an upload plan, read a plan's paths | No, though `api::build::plan` takes them | `api::StorePath` |
| `api::build::{plan, Plan, Dispatch, Options, Transport, config}` | Plan the upload of what an evaluation made in memory, then upload it | Yes | Nothing |
| `Plan::drvs` and `remote::plan::Object::store_path` | Root every path of a plan before it lands | `Plan` is in `api`; its `drvs` are `Object`s, which are not | `Plan::paths()`, every source and derivation path |
| `build::render::Mode` | A quiet `Style` for an upload that writes no progress | `Style` is in `api`, the type of its `mode` field is not | `api::build::Mode`, or `Style::quiet()` |
| `provenance::Provenance::off` | `Dispatch::open` takes a `Provenance`, and an upload names no source | No | `api::build::Provenance`, or a `Dispatch::open` that takes none |
| `error::Error`, its `UnknownRoot` variant | Drop a root the evaluation did not make, and plan the rest | No. `docs/reference/library.md` names `error::Error` as the error type | `api::Error` (beside `api::EvalError`), with its variants |
| `api::build::config().store()` | Read the `store` setting to choose the store | The call is in `api`; `config::Config` is not | `api::build::Config`, or `api::build::store_setting()` |
| `remote::worker::{Connection, Transport}`: `Transport::parse`, `Connection::open` | Ask whether a daemon answers the handshake before choosing it | `Transport` is in `api::build`, `Connection` is not | `api::build::answers(&Transport) -> Result<()>`, or `api::build::Connection` |
| `api::value::{Eval, Value, ValueKind, Kept}` | Read values, call functions, keep the root | Yes | Nothing |

## What the list says

Of seventeen items, two are inside `api` already. Five are half inside: the
call is in `api`, and a type it takes or returns is not. Ten are outside it.
Each fix is a re-export or one small function: none needs a new mechanism.

The items fall into three groups:

- **Types the facade's own calls take or return**: `StorePath`, `Report`,
  `eval::Error`, `error::Error`, `config::Config`, `Provenance`, `Mode`,
  `Object`. A host cannot call `api` without them, so `api` names them
  whatever else the choice is.
- **Setup of a session and an evaluator**: `Session`, `Env`, `EvalConfig`,
  `Evaluator`, `Release`, the lint switch, the stack size. They are the
  library's documented entry points; they live in their own modules because
  the command line uses them the same way.
- **A store probe**: `Connection::open`, which the host uses for one
  handshake. A function that answers whether a daemon is there would keep
  the worker protocol's connection out of the surface.

A stable surface is then either `api` with these names added, about twenty
of them, or a list of modules (`api`, `eval`, `session`, `env`,
`runtime::heap`) declared stable as they are. The first keeps the internals
free to move; the second costs nothing now and pins everything those modules
make `pub`, `Evaluator`'s public fields among them.
