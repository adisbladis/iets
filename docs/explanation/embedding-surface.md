# What an embedding host calls

`crate::api` is the library's surface: a frontend calls it and never
reaches into `cli` (`ARCHITECTURE.md`, "Layering"), and a host that embeds
the evaluator in a long-lived program needs nothing else. What `api` names
is what the library promises. The modules behind it stay `pub` for the
`iets` binary, the examples, the tests and the fuzz targets, and are free
to move. The host below is a daemon that keeps one session for each root
it evaluates. `tests/embedding.rs` makes its calls through `iets::api`
alone, and holds its own text to that.

## Item by item

Each row is something the host does, and the names it does it with.

| What for | In `api` |
|---|---|
| Build an evaluator in a session of the host's, pure, and keep it for the session's life | `api::{EvalConfig, Evaluator}` |
| Tell an evaluation error from a fault of the host by its kind (`offline`, `exhausted`, `budget`) | `api::{EvalError, ErrorKind, EvalResult}` |
| Render an error in Nix's words | `api::report_of`, which gives an `api::Report`; `Report::text` |
| One session per root, its environment, its memory, its cancel | `api::Session`: `with_env`, `enter`, `heap`, `cancel`; `api::Cancel` to keep the cancel where the request ends |
| A session that reads none of the host's environment but the variables it adds (`NIX_CONFIG`, `IETS_OFFLINE`) | `api::Env::{isolated, with}` |
| Give a released session's memory back; bound and watch the sessions' memory | `api::memory::{Heap, Release, MemoryGroup, MemoryHost, Exhausted}` |
| An evaluator that reports no lint | `EvalConfig::no_lints` |
| The stack the host evaluates on, the room the demand fiber has, so a deep evaluation meets the call-depth error first, reserved so it starts on a small machine | `api::on_main_stack`, `api::StackError`, `api::MAIN_STACK` |
| Name the roots of an upload plan | `api::StorePath`: `parse`, `to_full_string`, `base_name` |
| Plan the upload of what an evaluation made in memory, then upload it | `api::build::{plan, Plan, Dispatch, Options, Transport}` |
| Root every path of a plan before it lands | `Plan::paths` |
| An upload that writes no progress | `api::build::{Style, Mode}`, with `quiet` set |
| An upload that names no source | `api::build::Provenance::off` |
| Drop a root the evaluation did not make, and plan the rest | `api::Error::UnknownRoot` |
| Read the `store` setting to choose the store | `api::build::config`, which gives an `api::build::Config`; `Config::store` |
| Ask whether a store answers before choosing it | `api::build::answers` |
| Read values, call functions, keep the root | `api::value::{Eval, Value, ValueKind, Kept}` |

`Session::catch_exit` is not on the list. Only the command line's
`session::exit` reads it, and no library call reaches that.

## How each is named

Most entries are re-exports: the type already has the shape a host should
see, so `api` names it where it lives. `EvalError` is `eval::Error`, under a
name that sits beside `api::Error`, the error outside the evaluator.

A small function stands in where the type behind it would pin internals:

- `EvalConfig::no_lints`, in place of `lint::Config`, whose fields are the
  lint registry's levels and its tables per input.
- `Plan::paths`, in place of `Plan::drvs`, whose `remote::plan::Object`s are
  the uploader's records.
- `api::build::answers`, in place of the worker protocol's `Connection` and
  the nix-grpc-store client. It opens the store as an upload would, and
  closes it again.
- `api::on_main_stack`, in place of `parallel::fiber`'s stacks and switch.
  It reserves the stack the command line evaluates on, so a host does not
  map one of its own.

`Evaluator` and `Session` are re-exported with no wrapper. A wrapper of
`Evaluator` would make every `api` call that takes an evaluator take the
wrapper instead, and `api::value::Eval` already is the safe handle on its
values. What the re-export would pin is their public fields, so the fields
a host has no use for are `pub(crate)`. A `Session` shows its `log`, `heap`
and `cancel`. An `Evaluator` shows `drv`, the derivations a run made, which
`examples/drv_info.rs` lists, and the fuzz target's hidden `nix_eq`.

## What is left outside

A type outside `api` can still reach a host, as the type of a field or of
an argument of something `api` names. None of these is on the host's path,
and naming one in `api` would pin the module it belongs to:

- `runtime::Word`, which `api::evaluate` returns and `Evaluator`'s own
  methods take. A host reads values through `api::value`, where no `Word`
  shows.
- `lint::Config`, `IrCache` and `trace::run::Cache`: the lint levels, the
  compiled-file cache and the tracing cache that `EvalArgs` and `api::run`
  set up from the arguments of a command line.
- `Places`, `Dispatcher` and `DrvInfo`, which `api::build::places`,
  `Dispatch::with` and `plan_with` give and take for the command line.
- The types of `Session::settings` and `Session::log`, whose methods a host
  calls without naming them.
- The fields of `Plan`, `Provenance` and `Report`, whose types are the
  uploader's and the renderer's.

The entry points that `docs/reference/library.md` documents for other
frontends stay in their modules: `parallel::try_run`, which `api::run::Run`
wraps; `print::print_json`, for a frontend that prints a `Word`; and
`session::bind`, where a host calls `Session::enter` on its thread.
