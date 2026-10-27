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

## The stacks are reserved

The evaluator runs on big stacks: 4 GiB for the demand path and 256 MiB for
each helper fiber. Each one is a reservation (`MAP_NORESERVE`) with a guard
page at its low end, so under the default overcommit a page costs memory
only once the stack reaches it. The command line switches its evaluating
thread onto such a stack, and `api::on_main_stack` does the same for a host.

A thread's own stack is no reservation. The kernel charges all of it, and
its default heuristic refuses one mapping larger than memory and swap
together. So a thread built with a 4 GiB stack does not start on a machine
with less, where the reserved stack does.

Strict overcommit charges a reservation as it charges any mapping, and an
`RLIMIT_AS` or `RLIMIT_DATA` limit counts it. A stack the kernel refuses is
an error, not a crash: `iets` prints `iets: cannot reserve 4096 MiB of fiber
stacks: Cannot allocate memory (os error 12)` and exits 1, and
`api::on_main_stack` runs nothing and returns the `StackError`. A run with
more than one worker reserves another 4 GiB for its demand fiber and 256 MiB
for each helper. A helper fiber the run cannot reserve is left out, and the
run goes on with the fibers it has.

## The symbol tables are reserved

An evaluator keeps three tables indexed by symbol id: where each name is,
the first 8 bytes of each name, and the string a builtin made of it. Each
is one reservation of `MAX_SYMS` (2^30) words, 8 GiB. An id reads its entry
with one load and no lock, and a table never moves while fibers read it.
Under the default overcommit only the pages the ids reach cost memory: the
NixOS workloads in `bench/workloads` intern about 96,000 (nixos-server) and
99,000 (nixos-plasma) symbols, under 1 MiB of each table.

Strict overcommit charges the whole reservation, as it does a stack's, so
an evaluator needs about 28 GiB of the commit limit at one worker: 24 GiB
of tables and the 4 GiB main stack. A table the kernel refuses is an error,
not a crash. `EvalConfig::build` maps the three tables before anything
else. On a refusal it unmaps the ones it mapped and returns an
`api::TableError` that names the table. The session holds nothing of the
attempt and is not exhausted, so an evaluator built in it later works.
`iets` prints `iets: cannot reserve 8192 MiB for the table of symbol names:
Cannot allocate memory (os error 12)` and exits 1.

The tables are not made smaller, and they do not grow as ids come. A
smaller `MAX_SYMS` is one constant, but the point where a session is
exhausted and the process stops with `too many symbols` comes down with it.
At 2^26 ids the three tables would charge 1.5 GiB, and nixos-plasma would
still use a 680th of them, but where a long session must stop is a choice
of its own. A table that grows would reserve its address space with no
access and give the interner pages as it hands out ids. Strict overcommit
would then charge only those pages, but an `RLIMIT_AS` limit still counts
the reservation, and `intern` cannot fail. A growth the kernel refuses
would need ids already in hand to carry the evaluation to its next call, as
`SOFT_SYMS` does, and the strings table, which the evaluator fills outside
the interner, would have to grow with the other two.

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
