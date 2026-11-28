//! Host lists reset ahead of time, each on a host allocator of its own, recycled once the GPU is done with them.
//!
//! **Why.** On the card a host `ID3D12GraphicsCommandList::Reset` costs 53-66 µs inside the driver when the title
//! records (under 10 µs in `listbench`), and the title's recording threads paid it at every list Reset and at
//! every cut at a memory wait: ~48 lists a frame in gameplay, several ms. Here the recording thread takes a list
//! that is already open and empty (a lock and a pop), and the host Resets run on a thread of their own.
//!
//! **Lifetime of an entry** (a [`HostList`] with its own allocator, [`HostList::own`]):
//!
//! ```text
//! ready (open, empty) --take--> recording --Close--> submitted (queue, progress value)
//!      ^                                                   |  the console list's Reset, cut or Release
//!      |                                                   v
//!      +--- allocator Reset, list Reset <-- the recycler -- retired, until the queue's progress fence passes
//! ```
//!
//! An allocator is reset only when every list recorded into it has finished on the GPU: an entry's allocator
//! holds that one list alone, and the submission marks the queue and progress value of its last execution
//! ([`HostList::executed_on`], [`HostList::executed_at`]); [`enable`] is given the test of that value.
//!
//! A list without a mark is reset at once **only if the title never closed it** since its last reset
//! ([`HostList::sealed`]): an open list cannot have been executed. A sealed list without a mark may have been
//! executed by a path that does not mark (the runtime's submission marks every list it is given, executed or
//! skipped; anything else, such as a host list taken through `segments` and executed directly, does not), so it is
//! **held**: kept alive, never reset, counted in [`HELD`]. A leak, never a reset under the GPU.
//!
//! The pool is off until [`enable`] (the run asks for it with `REBUILD_LIST_POOL=1`); off, lists are reset on the
//! recording thread onto the console allocator's host allocators as before.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

use windows::Win32::Graphics::Direct3D12::*;

use super::list::HostList;
use super::Backend;

/// `done(queue, value)`: the queue's progress fence has passed `value` (the submission's mark of a list's last
/// execution).
pub type Done = fn(usize, u64) -> bool;

struct State {
    /// open, empty lists by type (0 direct, 1 bundle, 2 compute, 3 copy)
    ready: [Vec<HostList>; 4],
    /// lists the title is done with, maybe still on the GPU
    retired: Vec<HostList>,
    /// sealed lists retired without a mark: maybe executed by a path that does not mark, so never reset
    held: Vec<HostList>,
}

struct Pool {
    state: Mutex<State>,
    wake: Condvar,
    done: Done,
}

// SAFETY: D3D12 lists and allocators are free-threaded; each entry is used by one thread at a time (the pool's
// lock hands it over).
unsafe impl Send for State {}

static POOL: OnceLock<Pool> = OnceLock::new();

/// Lists taken ready, made new because none was ready, recycled by the recycler, dropped (a host Reset refused),
/// and the recycler's TSC ticks in host Resets (allocator and list).
pub static TAKEN: AtomicU64 = AtomicU64::new(0);
pub static MADE: AtomicU64 = AtomicU64::new(0);
pub static RECYCLED: AtomicU64 = AtomicU64::new(0);
pub static DROPPED: AtomicU64 = AtomicU64::new(0);
/// Sealed lists retired without an execution mark: held for the run, never reset.
pub static HELD: AtomicU64 = AtomicU64::new(0);
pub static RESET_TICKS: AtomicU64 = AtomicU64::new(0);

/// Turn the pool on (once) and start its recycler thread, when `REBUILD_LIST_POOL=1`. Off by default until a card
/// run confirms it: the first card run with it (and titlemem's range invalidation) ended in a GPU reset that was not
/// isolated; WARP with the debug layer was clean (`agents/listcost.md`).
pub fn enable(done: Done) -> bool {
    if !std::env::var("REBUILD_LIST_POOL").is_ok_and(|v| v.trim() == "1") {
        return false;
    }
    let mut started = false;
    POOL.get_or_init(|| {
        started = true;
        Pool::new(done)
    });
    if started {
        let _ = std::thread::Builder::new().name("xlist-recycler".into()).spawn(|| {
            if let Some(p) = POOL.get() {
                p.recycle_forever();
            }
        });
    }
    true
}

/// The pool is on.
#[inline]
pub fn on() -> bool {
    POOL.get().is_some()
}

/// An open, empty host list of type `kind` with an allocator of its own: a ready one, else a new one.
pub(crate) fn take(kind: u32, backend: &dyn Backend) -> windows::core::Result<HostList> {
    match POOL.get() {
        Some(p) => p.take(kind, backend),
        None => make(kind, backend),
    }
}

/// The title is done with `h` (its console list was reset, cut or released): back to the pool once the GPU is.
pub(crate) fn retire(h: HostList) {
    if let Some(p) = POOL.get() {
        p.retire(h);
    }
}

fn make(kind: u32, backend: &dyn Backend) -> windows::core::Result<HostList> {
    let a = backend.allocator(kind)?;
    let l = backend.list(kind, &a)?;
    MADE.fetch_add(1, Ordering::Relaxed);
    Ok(HostList::owning(l, a))
}

/// One line for the run report.
pub fn report() -> String {
    let r = |c: &AtomicU64| c.load(Ordering::Relaxed);
    let ms = super::cost::ms(r(&RESET_TICKS));
    let n = r(&RECYCLED).max(1);
    let (ready, retired) = POOL.get().map_or((0, 0), Pool::sizes);
    format!(
        "list pool {}: {} host lists taken ready, {} made; {} recycled ({ms:.1} ms of host Resets on the recycler, {:.1} µs each), {} dropped, {} held (closed, never marked executed); now {ready} ready, {retired} waiting for the GPU",
        if on() { "on" } else { "off" },
        r(&TAKEN),
        r(&MADE),
        r(&RECYCLED),
        ms * 1000.0 / n as f64,
        r(&DROPPED),
        r(&HELD)
    )
}

impl Pool {
    fn new(done: Done) -> Pool {
        Pool { state: Mutex::new(State { ready: Default::default(), retired: Vec::new(), held: Vec::new() }), wake: Condvar::new(), done }
    }

    fn take(&self, kind: u32, backend: &dyn Backend) -> windows::core::Result<HostList> {
        let h = self.state.lock().unwrap_or_else(|e| e.into_inner()).ready[kind as usize & 3].pop();
        match h {
            Some(h) => {
                TAKEN.fetch_add(1, Ordering::Relaxed);
                Ok(h)
            }
            None => make(kind, backend),
        }
    }

    /// A list without an allocator of its own is dropped.
    fn retire(&self, h: HostList) {
        if h.own.is_none() {
            return;
        }
        self.state.lock().unwrap_or_else(|e| e.into_inner()).retired.push(h);
        self.wake.notify_one();
    }

    fn sizes(&self) -> (usize, usize) {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (s.ready.iter().map(Vec::len).sum(), s.retired.len())
    }

    /// The recycler: retired lists whose last execution the GPU has finished get their allocator and themselves
    /// reset and go back to ready. It polls the fences every 0.5 ms while any list waits for the GPU, and sleeps
    /// otherwise.
    fn recycle_forever(&self) {
        loop {
            {
                let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                while s.retired.is_empty() {
                    s = self.wake.wait(s).unwrap_or_else(|e| e.into_inner());
                }
            }
            if self.recycle() > 0 {
                let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                let _ = self.wake.wait_timeout(s, Duration::from_micros(500));
            }
        }
    }

    /// One pass: every retired list the GPU is done with is renewed (outside the lock) and made ready. Returns how
    /// many still wait for the GPU.
    fn recycle(&self) -> usize {
        let free: Vec<HostList> = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let mut free = Vec::new();
            let mut i = 0;
            while i < s.retired.len() {
                let h = &s.retired[i];
                let at = h.executed_at.load(Ordering::Acquire);
                if at == 0 && h.sealed.load(Ordering::Acquire) {
                    // closed by the title and never marked: an unknown path may have it on the GPU
                    let h = s.retired.swap_remove(i);
                    s.held.push(h);
                    if HELD.fetch_add(1, Ordering::Relaxed) < 4 {
                        eprintln!("xlist     list pool: a closed host list was retired with no execution mark; held, never reset");
                    }
                } else if at == 0 || (self.done)(h.executed_on.load(Ordering::Relaxed), at) {
                    free.push(s.retired.swap_remove(i));
                } else {
                    i += 1;
                }
            }
            free
        };
        let mut back = Vec::with_capacity(free.len());
        for h in free {
            let t0 = super::cost::now();
            let ok = renew(&h);
            RESET_TICKS.fetch_add(super::cost::now().wrapping_sub(t0), Ordering::Relaxed);
            if ok {
                RECYCLED.fetch_add(1, Ordering::Relaxed);
                back.push(h);
            } else {
                DROPPED.fetch_add(1, Ordering::Relaxed);
            }
        }
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        for h in back {
            let k = h.kind as usize & 3;
            s.ready[k].push(h);
        }
        s.retired.len()
    }
}

/// Reset `h`'s allocator and `h` onto it: open and empty again, its marks cleared.
fn renew(h: &HostList) -> bool {
    let Some(a) = h.own.as_ref() else { return false };
    // a list released while open is closed first (an allocator with an open list refuses its Reset)
    if unsafe { a.Reset() }.is_err() && (unsafe { h.list.Close() }.is_err() || unsafe { a.Reset() }.is_err()) {
        return false;
    }
    if unsafe { h.list.Reset(a, None::<&ID3D12PipelineState>) }.is_err() {
        return false;
    }
    h.executed_on.store(0, Ordering::Relaxed);
    h.executed_at.store(0, Ordering::Relaxed);
    h.bad.store(false, Ordering::Relaxed);
    h.sealed.store(false, Ordering::Relaxed);
    h.stamp.store(super::stamps::NONE, Ordering::Relaxed);
    true
}

#[cfg(test)]
mod tests {
    use super::super::fake::{self, Fake};
    use super::*;
    use std::sync::atomic::AtomicBool;
    use windows::core::Interface;

    static GPU_DONE: AtomicBool = AtomicBool::new(false);
    fn done(queue: usize, value: u64) -> bool {
        assert_eq!((queue, value), (0x1234, 7));
        GPU_DONE.load(Ordering::Relaxed)
    }

    /// ID3D12CommandAllocator::Reset and ID3D12GraphicsCommandList::Close / Reset, by vtable slot.
    const ALLOCATOR_RESET: usize = 8;
    const LIST_CLOSE: usize = 9;
    const LIST_RESET: usize = 10;

    #[test]
    fn a_list_comes_back_ready_only_after_the_gpu_is_done_with_it() {
        let backend = fake::backend();
        let p = Pool::new(done);
        // none ready: made, with an allocator of its own
        let h = p.take(0, &*backend).unwrap();
        assert!(h.own.is_some());
        let (list, alloc) = (h.list.as_raw(), h.own.as_ref().unwrap().as_raw());
        // executed on queue 0x1234 as its submission 7, then retired: it waits for the GPU
        h.executed_on.store(0x1234, Ordering::Relaxed);
        h.executed_at.store(7, Ordering::Relaxed);
        h.bad.store(true, Ordering::Relaxed);
        p.retire(h);
        assert_eq!(p.recycle(), 1, "still on the GPU");
        assert_eq!(Fake::of(alloc).count(ALLOCATOR_RESET), 0, "its allocator untouched while the GPU may read it");
        assert_eq!(p.sizes(), (0, 1));
        // the GPU passes 7: allocator reset, then the list onto it; ready, marks cleared
        GPU_DONE.store(true, Ordering::Relaxed);
        assert_eq!(p.recycle(), 0);
        assert_eq!(Fake::of(alloc).count(ALLOCATOR_RESET), 1);
        assert_eq!(Fake::of(list).count(LIST_RESET), 1);
        assert_eq!(Fake::of(list).arg(LIST_RESET, 0), alloc as u64, "reset onto its own allocator");
        assert_eq!(p.sizes(), (1, 0));
        let lists = backend.lists.load(Ordering::Relaxed);
        let again = p.take(0, &*backend).unwrap();
        assert_eq!(again.list.as_raw(), list, "the same list, taken ready");
        assert_eq!(backend.lists.load(Ordering::Relaxed), lists, "nothing made");
        assert_eq!((again.executed_at.load(Ordering::Relaxed), again.bad.load(Ordering::Relaxed)), (0, false));
        // a list never executed is renewed at once; types keep their own shelves
        p.retire(again);
        assert_eq!(p.recycle(), 0);
        assert_eq!(p.sizes(), (1, 0));
        let copy = p.take(3, &*backend).unwrap();
        assert_ne!(copy.list.as_raw(), list, "a copy list is not taken from the direct shelf");
        assert_eq!(Fake::of(list).count(LIST_CLOSE), 0);
        // a list without an allocator of its own is not kept
        p.retire(HostList::new_for_test(copy.list.clone()));
        assert_eq!(p.sizes(), (1, 0));
    }

    fn never(_: usize, _: u64) -> bool {
        panic!("no list here carries a mark")
    }

    /// Suspect (a) of `agents/listcost.md`: a list the title closed but no submission marked may be on the GPU
    /// through a path that does not mark, so it is held, never reset; one never closed is reset at once.
    #[test]
    fn a_closed_list_without_an_execution_mark_is_never_reset() {
        let backend = fake::backend();
        let p = Pool::new(never);
        let sealed = p.take(0, &*backend).unwrap();
        let (list, alloc) = (sealed.list.as_raw(), sealed.own.as_ref().unwrap().as_raw());
        sealed.sealed.store(true, Ordering::Release);
        let held = HELD.load(Ordering::Relaxed);
        p.retire(sealed);
        assert_eq!(p.recycle(), 0, "not waiting on a fence it has no value for");
        assert_eq!(p.sizes(), (0, 0), "neither ready nor retired");
        assert_eq!(p.state.lock().unwrap().held.len(), 1, "held");
        assert!(HELD.load(Ordering::Relaxed) > held);
        assert_eq!(Fake::of(alloc).count(ALLOCATOR_RESET), 0, "its allocator never reset");
        assert_eq!(Fake::of(list).count(LIST_RESET), 0, "the list never reset");
        // a list the title never closed cannot have been executed: renewed at once
        let open = p.take(0, &*backend).unwrap();
        let alloc2 = open.own.as_ref().unwrap().as_raw();
        p.retire(open);
        assert_eq!(p.recycle(), 0);
        assert_eq!(p.sizes(), (1, 0));
        assert_eq!(Fake::of(alloc2).count(ALLOCATOR_RESET), 1);
        // renewing clears the seal with the marks
        let again = p.take(0, &*backend).unwrap();
        assert!(!again.sealed.load(Ordering::Relaxed));
    }
}
