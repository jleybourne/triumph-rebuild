//! A single-producer, single-consumer ring of samples. Neither side ever waits: a full ring takes
//! fewer samples than offered, an empty ring gives fewer than asked.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

pub struct Ring {
    slots: Box<[UnsafeCell<f32>]>,
    mask: usize,
    /// Samples ever written and read; the difference is what the ring holds.
    written: AtomicUsize,
    read: AtomicUsize,
}

// SAFETY: one writer and one reader, each touching only slots the counters give it.
unsafe impl Sync for Ring {}

impl Ring {
    /// A writer and a reader over at least `capacity` samples.
    pub fn new(capacity: usize) -> (Writer, Reader) {
        let len = capacity.max(2).next_power_of_two();
        let ring = Arc::new(Ring {
            slots: (0..len).map(|_| UnsafeCell::new(0.0)).collect(),
            mask: len - 1,
            written: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
        });
        (Writer(ring.clone()), Reader(ring))
    }

    fn held(&self) -> usize {
        self.written.load(Ordering::Acquire) - self.read.load(Ordering::Acquire)
    }
}

/// The producing side. Not `Clone`: there is one.
pub struct Writer(Arc<Ring>);

impl Writer {
    /// Copies as many samples as fit; returns how many.
    pub fn push(&mut self, samples: &[f32]) -> usize {
        let r = &*self.0;
        let w = r.written.load(Ordering::Relaxed);
        let room = r.slots.len() - (w - r.read.load(Ordering::Acquire));
        let n = room.min(samples.len());
        for (i, &s) in samples[..n].iter().enumerate() {
            // SAFETY: slots between written and read + len belong to the writer.
            unsafe { *r.slots[(w + i) & r.mask].get() = s };
        }
        r.written.store(w + n, Ordering::Release);
        n
    }

    pub fn room(&self) -> usize {
        self.0.slots.len() - self.0.held()
    }
}

/// The consuming side.
pub struct Reader(Arc<Ring>);

impl Reader {
    /// Fills `out` from the ring; returns how many samples were there. The rest is left as it was.
    pub fn pop(&mut self, out: &mut [f32]) -> usize {
        let r = &*self.0;
        let rd = r.read.load(Ordering::Relaxed);
        let n = (r.written.load(Ordering::Acquire) - rd).min(out.len());
        for (i, o) in out[..n].iter_mut().enumerate() {
            // SAFETY: slots between read and written belong to the reader.
            *o = unsafe { *r.slots[(rd + i) & r.mask].get() };
        }
        r.read.store(rd + n, Ordering::Release);
        n
    }

    pub fn held(&self) -> usize {
        self.0.held()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_order_across_threads() {
        let (mut w, mut r) = Ring::new(1000);
        let total = 1_000_000usize;
        let producer = std::thread::spawn(move || {
            let mut next = 0usize;
            while next < total {
                let chunk: Vec<f32> = (next..(next + 333).min(total)).map(|v| v as f32).collect();
                next += w.push(&chunk);
            }
        });
        let mut expect = 0usize;
        let mut buf = [0f32; 257];
        while expect < total {
            let n = r.pop(&mut buf);
            for &v in &buf[..n] {
                assert_eq!(v, expect as f32);
                expect += 1;
            }
        }
        producer.join().unwrap();
        assert_eq!(r.held(), 0);
    }
}
