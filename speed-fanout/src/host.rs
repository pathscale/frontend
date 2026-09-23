//! The fanout pool's `Host`, over `eko`'s pthread Mutex and Condvar.
//!
//! ps-st3's own hosts sit behind its `host` and `atomic-host` features, which bring a futex
//! crate. This one needs nothing but `eko`, which frontend already links.
//!
//! **The permit is the whole point.** `Host` requires that an `unpark` arriving before the
//! matching `park` makes that `park` return at once: the pool publishes work and then unparks,
//! so a dropped early wake is a worker asleep beside a job. A bare condition variable forgets a
//! signal nobody was waiting for, so each worker has a flag beside its condvar, set by `unpark`
//! and consumed by `park`, both under the one mutex.
//!
//! The wakes here are per job chunk, not per task poll, so a mutex per wake costs nothing
//! measurable against a parse.

use std::time::Instant;

use eko::thread::{Condvar, Mutex};
use st3::fanout::Host;

/// One worker's park slot, padded to its own cache lines so two workers' wakes do not share one.
#[repr(align(128))]
struct Slot {
    /// A wake that has not been consumed yet.
    permit: Mutex<bool>,
    wake: Condvar,
}

/// Parking for a fixed number of workers, one slot each, and a clock from construction.
pub struct EkoHost {
    slots: Vec<Slot>,
    origin: Instant,
}

impl EkoHost {
    /// One slot per worker, no permits outstanding, and a clock whose origin is now.
    pub fn new(workers: usize) -> Self {
        let slots = (0..workers)
            .map(|_| Slot { permit: Mutex::new(false), wake: Condvar::new() })
            .collect();
        Self { slots, origin: Instant::now() }
    }
}

impl core::fmt::Debug for EkoHost {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EkoHost").field("workers", &self.slots.len()).finish_non_exhaustive()
    }
}

impl Host for EkoHost {
    fn park(&self, worker: usize) {
        let slot = &self.slots[worker];
        let mut permit = slot.permit.lock();
        // Loop, because a condvar may wake with nothing to show for it.
        while !*permit {
            permit = slot.wake.wait(permit);
        }
        *permit = false;
    }

    fn unpark(&self, worker: usize) {
        let slot = &self.slots[worker];
        let mut permit = slot.permit.lock();
        *permit = true;
        // Signalled with the lock held, so the waiter cannot miss it between its check of the
        // flag and its wait.
        slot.wake.notify_one();
        drop(permit);
    }

    fn now_ns(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
}
