//! The memory of one session. Every arena chunk and every interned symbol
//! name is mapped here, and all of it goes when the last handle drops
//! (docs/reference/library.md, "Sessions and memory").
//!
//! By default a released range is not given back to the address space: it
//! becomes an inaccessible reservation, so a `Word` kept past its session
//! faults on first use instead of reading whatever the next session put
//! there. A host that keeps no `Word` past its session can have the ranges
//! unmapped instead (`Release::Unmap`).
//!
//! A heap can count its bytes into a `MemoryGroup`, whose `MemoryHost` is
//! asked before each new mapping whether the group may grow.

use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::{mem, process, ptr, slice};

use crate::parallel::fiber;
use crate::sys::vm;

/// Name bytes come from blocks of this size; a longer name gets its own.
const NAME_BLOCK: usize = 1 << 20;

/// What a released session does with its memory.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Release {
    /// Keep the ranges reserved and inaccessible, so a `Word` used after
    /// its session stops the process with a message.
    #[default]
    Revoke,
    /// Unmap the ranges. A `Word` used after its session then reads
    /// whatever is mapped there later.
    Unmap,
}

/// Decides whether the sessions of a `MemoryGroup` may map more memory.
pub trait MemoryHost: Send + Sync {
    /// Whether the group may hold `total` bytes. Asked before each arena
    /// chunk or name block a session of the group maps, on the allocating
    /// thread with no lock held. On `false` the chunk is still mapped, and
    /// the evaluation fails at its next call with `ErrorKind::Budget`.
    fn allow(&self, total: usize) -> bool;

    /// Called when a mapping fails, before iets tries once more. If that
    /// fails too, the process exits with `out of memory`.
    fn release(&self) {}
}

/// Sessions whose bytes count against one `MemoryHost` (`Heap::join`).
pub struct MemoryGroup {
    bytes: AtomicUsize,
    host: Box<dyn MemoryHost>,
}

impl MemoryGroup {
    pub fn new(host: impl MemoryHost + 'static) -> Arc<MemoryGroup> {
        Arc::new(MemoryGroup {
            bytes: AtomicUsize::new(0),
            host: Box::new(host),
        })
    }

    /// Bytes the sessions of the group hold.
    pub fn bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }
}

#[derive(Default)]
pub struct Heap {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    chunks: Vec<(usize, usize)>,
    /// Rust values handed out as `&'static`, dropped before the chunks go.
    kept: Vec<Box<dyn Send>>,
    /// Bump state of the current name block.
    next: usize,
    end: usize,
    /// Bytes mapped: arena chunks and name blocks.
    bytes: usize,
    release: Release,
    group: Option<Arc<MemoryGroup>>,
}

impl Heap {
    /// Takes ownership of a mapping that `grow` counted: it is released
    /// with the heap.
    pub(crate) fn adopt(&self, base: usize, len: usize) {
        self.inner.lock().unwrap().chunks.push((base, len));
    }

    /// Counts `len` bytes about to be mapped and asks the group's host
    /// whether the group may hold them. True outside a group.
    pub(crate) fn grow(&self, len: usize) -> bool {
        let (group, total) = {
            let mut g = self.inner.lock().unwrap();
            g.bytes += len;
            let Some(group) = g.group.clone() else {
                return true;
            };
            let total = group.bytes.fetch_add(len, Ordering::Relaxed) + len;
            (group, total)
        };
        group.host.allow(total)
    }

    /// Asks what `grow` asks for `extra` more bytes, without counting them.
    pub(crate) fn may_grow(&self, extra: usize) -> bool {
        let group = self.inner.lock().unwrap().group.clone();
        group.is_none_or(|g| g.host.allow(g.bytes().saturating_add(extra)))
    }

    /// Maps `len` bytes, retrying once after `MemoryHost::release`.
    pub(crate) fn map(&self, len: usize) -> usize {
        if let Some(p) = try_map(len) {
            return p;
        }
        let group = self.inner.lock().unwrap().group.clone();
        if let Some(g) = group {
            g.host.release();
        }
        let Some(p) = try_map(len) else {
            // no unwinding: other fibers hold arena pointers, and Nix ends
            // the same way on a heap it cannot grow
            errln!("error: out of memory");
            process::exit(1);
        };
        p
    }

    /// Bytes mapped for this session so far: arena chunks and name blocks.
    pub fn bytes(&self) -> usize {
        self.inner.lock().unwrap().bytes
    }

    /// Sets what happens to the memory when the heap is released.
    pub fn set_release(&self, r: Release) {
        self.inner.lock().unwrap().release = r;
    }

    /// Moves this heap's bytes into `group`, out of any group before it.
    pub fn join(&self, group: &Arc<MemoryGroup>) {
        let mut g = self.inner.lock().unwrap();
        if let Some(old) = g.group.take() {
            old.bytes.fetch_sub(g.bytes, Ordering::Relaxed);
        }
        group.bytes.fetch_add(g.bytes, Ordering::Relaxed);
        g.group = Some(group.clone());
    }

    /// `len` bytes at alignment 8 that live as long as the session.
    pub(crate) fn alloc(&self, len: usize) -> *mut u8 {
        let len = len.max(1).next_multiple_of(8);
        {
            let mut g = self.inner.lock().unwrap();
            if g.end - g.next >= len {
                let p = g.next;
                g.next += len;
                return p as *mut u8;
            }
        }
        // unlocked, as the host may call back into the heap; when two
        // threads race here, one block's unused tail is lost
        let size = NAME_BLOCK.max(len.next_multiple_of(vm::page_size()));
        // a name block is small; the arena reports refusals
        self.grow(size);
        let p = self.map(size);
        let mut g = self.inner.lock().unwrap();
        g.chunks.push((p, size));
        if size > NAME_BLOCK {
            return p as *mut u8;
        }
        g.next = p + len;
        g.end = p + size;
        p as *mut u8
    }

    /// `v`, owned by the session and dropped when it is released.
    pub(crate) fn keep<T: Send + Sync + 'static>(&self, v: T) -> &'static T {
        let b = Box::new(v);
        let r: *const T = &*b;
        self.inner.lock().unwrap().kept.push(b);
        // SAFETY: the box does not move its contents and lives until release.
        unsafe { &*r }
    }

    /// `b` copied into this session's memory.
    pub(crate) fn alloc_bytes(&self, b: &[u8]) -> &'static [u8] {
        let p = self.alloc(b.len());
        // SAFETY: fresh allocation of at least `b.len()` bytes, valid until
        // the heap drops, which outlives every reference by the session rule.
        unsafe {
            ptr::copy_nonoverlapping(b.as_ptr(), p, b.len());
            slice::from_raw_parts(p, b.len())
        }
    }
}

/// An anonymous mapping of `len` bytes, or `None` when there is no memory.
fn try_map(len: usize) -> Option<usize> {
    #[cfg(test)]
    if tests::FAIL_NEXT_MAP.with(|f| f.replace(false)) {
        return None;
    }
    vm::map(len, false).ok().map(|p| p as usize)
}

impl Drop for Heap {
    fn drop(&mut self) {
        let inner = self.inner.get_mut().unwrap();
        drop(mem::take(&mut inner.kept));
        if let Some(g) = inner.group.take() {
            g.bytes.fetch_sub(inner.bytes, Ordering::Relaxed);
        }
        let chunks = mem::take(&mut inner.chunks);
        if chunks.is_empty() {
            return;
        }
        if inner.release == Release::Unmap {
            for (base, len) in chunks {
                #[cfg(test)]
                tests::UNMAPPED.with(|u| u.set(u.get() + len));
                // SAFETY: the range is a mapping this heap owns, and the host
                // that chose `Unmap` keeps no reference into it.
                unsafe { vm::unmap(base as *mut u8, len) };
            }
            return;
        }
        fiber::install_fault_handler();
        for (base, len) in chunks {
            tombstone(base, len);
        }
    }
}

/// Gives the pages of `[base, base + len)` back and keeps the range
/// reserved and inaccessible. NORESERVE and PROT_NONE cost no commit
/// charge, and adjacent tombstones merge into one mapping.
fn tombstone(base: usize, len: usize) {
    // SAFETY: the range is a mapping this heap owns; nothing may touch it
    // after release, and what does faults, which is the point.
    if !unsafe { vm::revoke(base as *mut u8, len) } {
        // the pages stay, the range stays ours: a leak, never a reuse
        return;
    }
    let n = Box::into_raw(Box::new(Released {
        base,
        len,
        next: ptr::null_mut(),
    }));
    let mut head = RELEASED.load(Ordering::Relaxed);
    loop {
        // SAFETY: `n` is ours until the exchange publishes it.
        unsafe { (*n).next = head };
        match RELEASED.compare_exchange_weak(head, n, Ordering::Release, Ordering::Relaxed) {
            Ok(_) => break,
            Err(h) => head = h,
        }
    }
}

/// Append-only, so the fault handler walks it without a lock.
struct Released {
    base: usize,
    len: usize,
    next: *mut Released,
}

static RELEASED: AtomicPtr<Released> = AtomicPtr::new(ptr::null_mut());

/// Whether `addr` is in the memory of a released session. Async-signal-safe.
pub(crate) fn is_released(addr: usize) -> bool {
    let mut p = RELEASED.load(Ordering::Acquire);
    while !p.is_null() {
        // SAFETY: nodes are leaked and never change after publication.
        let r = unsafe { &*p };
        if addr >= r.base && addr - r.base < r.len {
            return true;
        }
        p = r.next;
    }
    false
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::Cell;

    thread_local! {
        /// Makes this thread's next `try_map` fail.
        pub(crate) static FAIL_NEXT_MAP: Cell<bool> = const { Cell::new(false) };
        /// Bytes this thread's dropped heaps unmapped.
        pub(crate) static UNMAPPED: Cell<usize> = const { Cell::new(0) };
    }

    #[test]
    fn release_tombstones_every_chunk() {
        let h = Heap::default();
        let a = h.alloc_bytes(b"outPath");
        let big = h.alloc(3 << 20);
        let at = a.as_ptr() as usize;
        assert_eq!(a, b"outPath");
        assert_eq!(h.bytes(), NAME_BLOCK + (3 << 20));
        assert!(!is_released(at));
        drop(h);
        assert!(is_released(at));
        assert!(is_released(big as usize + (2 << 20)));
    }

    #[test]
    fn an_unmapped_release_leaves_no_reservation() {
        let h = Heap::default();
        h.set_release(Release::Unmap);
        let at = h.alloc_bytes(b"outPath").as_ptr() as usize;
        let before = UNMAPPED.with(|u| u.get());
        drop(h);
        assert!(!is_released(at));
        assert_eq!(UNMAPPED.with(|u| u.get()) - before, NAME_BLOCK);
    }

    /// Allows up to `limit` bytes and counts the calls to `release`.
    pub(crate) struct Limit {
        pub(crate) limit: usize,
        pub(crate) released: Arc<AtomicUsize>,
    }

    impl MemoryHost for Limit {
        fn allow(&self, total: usize) -> bool {
            total <= self.limit
        }

        fn release(&self) {
            self.released.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn limit(limit: usize) -> (Arc<MemoryGroup>, Arc<AtomicUsize>) {
        let released = Arc::new(AtomicUsize::new(0));
        let g = MemoryGroup::new(Limit {
            limit,
            released: released.clone(),
        });
        (g, released)
    }

    #[test]
    fn a_group_counts_its_heaps_and_its_host_decides() {
        let (g, _) = limit(3 * NAME_BLOCK);
        let a = Heap::default();
        a.alloc(8);
        a.join(&g);
        assert_eq!(g.bytes(), NAME_BLOCK);
        let b = Heap::default();
        b.join(&g);
        assert!(b.grow(NAME_BLOCK));
        assert!(!b.grow(NAME_BLOCK + 1));
        assert!(!b.may_grow(0));
        assert_eq!(g.bytes(), 3 * NAME_BLOCK + 1);
        drop(b);
        assert_eq!(g.bytes(), NAME_BLOCK);
        assert!(a.may_grow(NAME_BLOCK));
        drop(a);
        assert_eq!(g.bytes(), 0);
    }

    #[test]
    fn a_failed_mapping_asks_the_host_to_release_and_maps_again() {
        let (g, released) = limit(usize::MAX);
        let h = Heap::default();
        h.join(&g);
        FAIL_NEXT_MAP.with(|f| f.set(true));
        let p = h.alloc_bytes(b"outPath");
        assert_eq!(p, b"outPath");
        assert_eq!(released.load(Ordering::Relaxed), 1);
    }
}
