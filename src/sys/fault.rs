//! Turning a fault on a known address into a message and an exit.

use std::cell::RefCell;
use std::sync::{Once, OnceLock};
use std::{mem, ptr};

pub use super::Fault;

static CLASSIFY: OnceLock<fn(usize) -> Option<Fault>> = OnceLock::new();

/// The handlers `set_handler` replaced, in the order of `SIGNALS`. A fault
/// iets does not own goes to them.
static PREVIOUS: [OnceLock<Previous>; 2] = [OnceLock::new(), OnceLock::new()];

const SIGNALS: [i32; 2] = [libc::SIGSEGV, libc::SIGBUS];

struct Previous(libc::sigaction);

// SAFETY: a `sigaction` is plain data.
unsafe impl Send for Previous {}
unsafe impl Sync for Previous {}

/// Replaces the runtime's fault handler. `classify` gets the faulting
/// address and runs inside the handler, so it must not lock or allocate.
/// A fault it declines goes to the previous handler, or gets the default
/// disposition when there was none.
pub fn install(classify: fn(usize) -> Option<Fault>) {
    static ONCE: Once = Once::new();
    let _ = CLASSIFY.set(classify);
    ONCE.call_once(set_handler);
}

fn set_handler() {
    // SAFETY: the documented use of `sigaction`, with a handler that only
    // writes to fd 2 and ends the process, or calls the handler it replaced.
    unsafe {
        for (i, sig) in SIGNALS.into_iter().enumerate() {
            let mut sa: libc::sigaction = mem::zeroed();
            sa.sa_sigaction = on_fault as *const () as usize;
            sa.sa_flags = libc::SA_ONSTACK | libc::SA_SIGINFO;
            libc::sigemptyset(&mut sa.sa_mask);
            let mut old: libc::sigaction = mem::zeroed();
            if libc::sigaction(sig, &sa, &mut old) == 0 {
                let _ = PREVIOUS[i].set(Previous(old));
            }
        }
    }
}

extern "C" fn on_fault(sig: i32, info: *mut libc::siginfo_t, ctx: *mut libc::c_void) {
    // SAFETY: the kernel passes a valid siginfo for SIGSEGV/SIGBUS.
    let addr = unsafe { (*info).si_addr() } as usize;
    if let Some(f) = CLASSIFY.get().and_then(|c| c(addr)) {
        // SAFETY: write, _exit and abort are async-signal-safe.
        unsafe {
            libc::write(2, f.message.as_ptr() as *const _, f.message.len());
            if f.abort {
                libc::abort();
            }
            libc::_exit(1);
        }
    }
    let previous = SIGNALS
        .iter()
        .position(|s| *s == sig)
        .and_then(|i| PREVIOUS[i].get())
        .map(|p| &p.0);
    match previous {
        Some(p) if p.sa_sigaction != libc::SIG_DFL && p.sa_sigaction != libc::SIG_IGN => {
            // SAFETY: `sigaction` reported this handler, with the signature
            // its `SA_SIGINFO` flag selects; it is called as the kernel would.
            unsafe {
                if p.sa_flags & libc::SA_SIGINFO != 0 {
                    let h: extern "C" fn(i32, *mut libc::siginfo_t, *mut libc::c_void) =
                        mem::transmute(p.sa_sigaction);
                    h(sig, info, ctx);
                } else {
                    let h: extern "C" fn(i32) = mem::transmute(p.sa_sigaction);
                    h(sig);
                }
            }
        }
        // nobody else's either: the default disposition crashes as usual
        // SAFETY: restoring the default handler and re-raising is the
        // standard way to report a fault we do not own.
        _ => unsafe {
            libc::signal(sig, libc::SIG_DFL);
        },
    }
}

/// This thread's alternate signal stack. The destructor turns `sigaltstack`
/// off before the memory goes.
struct AltStack {
    _mem: Box<[u8]>,
}

impl Drop for AltStack {
    fn drop(&mut self) {
        let ss = libc::stack_t {
            ss_sp: ptr::null_mut(),
            ss_flags: libc::SS_DISABLE,
            ss_size: 0,
        };
        // SAFETY: disables this thread's alternate stack; no handler can
        // run on it after this returns.
        unsafe { libc::sigaltstack(&ss, ptr::null_mut()) };
    }
}

thread_local! {
    static ALT_STACK: RefCell<Option<AltStack>> = const { RefCell::new(None) };
}

/// Gives this thread a stack to run the handler on, so a fault caused by an
/// exhausted stack can still be reported. It is separate from every fiber
/// stack, so it stays valid across a fiber switch.
pub fn alt_stack() {
    const ALT: usize = 64 << 10;
    ALT_STACK.with(|a| {
        let mut a = a.borrow_mut();
        if a.is_some() {
            return;
        }
        let mut mem = vec![0u8; ALT].into_boxed_slice();
        let ss = libc::stack_t {
            ss_sp: mem.as_mut_ptr() as *mut _,
            ss_flags: 0,
            ss_size: ALT,
        };
        // SAFETY: the alternate stack lives in this thread's `ALT_STACK`
        // until the thread exits.
        unsafe { libc::sigaltstack(&ss, ptr::null_mut()) };
        *a = Some(AltStack { _mem: mem });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::process::{self, Command};

    extern "C" fn host_handler(_: i32, _: *mut libc::siginfo_t, _: *mut libc::c_void) {
        let m = b"the host's handler ran\n";
        // SAFETY: write and _exit are async-signal-safe.
        unsafe {
            libc::write(2, m.as_ptr() as *const _, m.len());
            libc::_exit(42);
        }
    }

    /// The child installs a handler as a host would, then iets's, and
    /// faults on an address iets does not own. It exits with 42 only if
    /// iets passes the fault on.
    #[test]
    fn a_fault_iets_does_not_own_reaches_the_handler_set_before() {
        if env::var_os("IETS_TEST_FAULT_CHILD").is_some() {
            // SAFETY: the documented use of `sigaction`.
            unsafe {
                let mut sa: libc::sigaction = mem::zeroed();
                sa.sa_sigaction = host_handler as *const () as usize;
                sa.sa_flags = libc::SA_SIGINFO;
                libc::sigemptyset(&mut sa.sa_mask);
                libc::sigaction(libc::SIGSEGV, &sa, ptr::null_mut());
            }
            install(|_| None);
            // SAFETY: the read faults on purpose, and the handler exits.
            unsafe { ptr::read_volatile(ptr::without_provenance::<u64>(8)) };
            process::exit(0);
        }
        let out = Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "sys::fault::tests::a_fault_iets_does_not_own_reaches_the_handler_set_before",
            ])
            .env("IETS_TEST_FAULT_CHILD", "1")
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(42), "{err}");
        assert!(err.contains("the host's handler ran"), "{err}");
    }
}
