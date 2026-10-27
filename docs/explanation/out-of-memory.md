# When a session runs out of memory

You run iets inside a program that lives for weeks, with one session per
request. One request's evaluation asks for more memory than the machine
will map. Nix ends the process there. iets ends the session instead: the
evaluation fails with `exhausted`, you release the session, and the other
sessions go on, within the limits in "What still ends the process".

## An allocation cannot fail

The evaluator allocates from a bump arena, and every allocation returns
memory. A builtin that builds a list writes into the list as soon as it has
it, and a thunk that is forced writes its value into a cell the same way.
An allocation that could fail would need an error path at every one of
those sites, in the code that runs most often. So when the kernel refuses a
mapping, the arena must still hand out memory.

## No unwinding

A panic could carry the failure back to the host, but it has nowhere safe
to go. With more than one worker, the failing code runs on a fiber, and a
panic cannot cross the stack switch back to the worker. Other fibers keep
running and hold pointers into the same arena. The thunks the failing fiber
had claimed stay claimed, and an interner shard may be locked. Even with one
worker, the session after an unwind is in a state nothing checks. So iets
doesn't unwind: the failure goes back to the host the way any evaluation
error does, as an `Err` from the next function call.

## Exhausted, not unwound

When a mapping fails, iets first asks the session's `MemoryHost` to free
memory and tries once more. When that fails too, the session is exhausted.
The memory the evaluation needs to reach its next call comes from a reserve
that the process mapped earlier, while memory was there: one arena chunk
and two name blocks, 34 MiB. The arena then lowers its call limit to zero,
the same check a `MemoryHost` refusal uses, so the next function call fails
with `ErrorKind::Exhausted`. The error goes up through the evaluator like
any other, and every thunk on the way is put back as it was. The next
mapping that works fills the reserve again.

A session whose evaluator interns `SOFT_SYMS` names, 2^24 short of the
2^30 a `Sym` can hold, is exhausted the same way. The ids left over carry
the evaluation to the arena's next chunk, where it sees the mark: 2^24 new
names come with far more than one chunk of attribute sets.

An exhausted session stays exhausted. Every evaluation that comes in through
`api`, `api::value` or `Evaluator::enter` fails at its first call, so the
host releases the session and starts a new one. Its memory goes back with
it.

## What still ends the process

The reserve holds a fixed amount, so some failures still stop the process
with `error: out of memory`, as every one did before:

- A single allocation larger than an arena chunk, such as one huge list,
  has no room in the reserve.
- A second session that runs out before a mapping fills the reserve again
  finds it spent. With more than one worker, each worker arena that needs a
  chunk after the failure draws on the same reserve.
- An evaluation that allocates more than the reserve before its next call
  runs out of it.
- Past 2^30 symbols there are no ids left, and the process stops with
  `error: too many symbols`.

Two more are outside iets. Rust's own allocator aborts the process when
`malloc` fails, and the evaluator uses it for vectors and hash tables. With
Linux's default overcommit, a mapping almost never fails: the kernel kills
a process when its pages run out instead. A refused mapping comes from
strict overcommit (`vm.overcommit_memory = 2`), a `RLIMIT_AS` or
`RLIMIT_DATA` limit, or an address space or map count that ran out.

## What a host does

Keep sessions from reaching the machine's limit at all. Put them in a
`MemoryGroup` whose `MemoryHost` refuses well below what the machine can
map. A refusal fails the evaluation with `budget` and leaves the session
usable, and `release()` is where the host drops idle sessions. Release
sessions with `Release::Unmap`, so the address space and the map count stay
flat. Treat `exhausted` as "release this session", and keep the reserve for
the rare case the bound missed.

A host that must survive every failure runs the evaluator in a process of
its own, one for each session or instance, and speaks to it over a pipe.
The helper's exit is then an error the host reads, and the helper also
keeps iets's fault handler and its unsafe code out of the host's address
space. It costs a second binary, a protocol, and a copy of every value that
crosses.
