//! Serial compiler walks over borrowed input.
//!
//! Arena slices, TyCtxt and compiler diagnostic contexts stay on the caller. These compatibility
//! entry points never submit work, capture TLS or extend a lifetime. Owned parallel work uses
//! `sync::run_owned_stage`, which requires typed Arc input and ordinary Send/Sync bounds.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::future::Future;
use core::marker::PhantomData;
use core::pin::Pin;
use core::task::{Context, Poll};

use crate::rustc_data_structures::sync::{DynSend, DynSync};

/// Results of a completed serial walk. Values are never copied for hand-off.
pub struct Slots<O> {
    values: Vec<O>,
}

impl<O> Slots<O> {
    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&O> {
        Some(&self.values[index])
    }

    pub fn is_settled(&self, index: usize) -> bool {
        let _ = &self.values[index];
        true
    }

    pub fn ready(&self, index: usize) -> ReadySlot<'_, O> {
        ReadySlot { slots: self, index }
    }

    pub fn wait(&self, index: usize) -> Option<&O> {
        self.get(index)
    }

    pub fn wait_all(&self) {}

    pub fn into_values(self) -> Vec<O> {
        self.values
    }
}

/// A completed slot, retained for callers that await the serial compatibility API.
pub struct ReadySlot<'a, O> {
    slots: &'a Slots<O>,
    index: usize,
}

impl<'a, O> Future for ReadySlot<'a, O> {
    type Output = Option<&'a O>;

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(self.slots.get(self.index))
    }
}

/// A caller-thread sequence of compiler walks. No work escapes this thread.
pub struct StageScope<'scope, 'env: 'scope> {
    _scope: PhantomData<&'scope mut &'scope ()>,
    _env: PhantomData<&'env mut &'env ()>,
    _owner_thread: PhantomData<*const ()>,
}

/// Execute the serial sequence, propagating diagnostics and panics at their original call sites.
pub fn stages<'env, R>(body: impl for<'scope> FnOnce(&'scope StageScope<'scope, 'env>) -> R) -> R {
    body(&StageScope { _scope: PhantomData, _env: PhantomData, _owner_thread: PhantomData })
}

/// Execute borrowed compiler work in input order on the caller.
pub fn run_stage<'env, In, O, F>(input: In, len: usize, f: F) -> Vec<O>
where
    In: DynSync + DynSend + 'env,
    O: DynSync + DynSend + 'env,
    F: Fn(&In, usize) -> O + DynSync + DynSend + 'env,
{
    (0..len).map(|index| f(&input, index)).collect()
}

/// Serial compatibility for weighted compiler walks. Weights are not evaluated.
pub fn run_stage_weighted<'env, In, O, W, F>(input: In, len: usize, weight: W, f: F) -> Vec<O>
where
    In: DynSync + DynSend + 'env,
    O: DynSync + DynSend + 'env,
    W: Fn(&In, usize) -> u32,
    F: Fn(&In, usize) -> O + DynSync + DynSend + 'env,
{
    let _ = weight;
    run_stage(input, len, f)
}

/// What a pass costs per byte of the source an item covers, in nanoseconds: the factor that
/// turns a byte count into a stage weight (`cost::weight`).
///
/// Measured on this crate's own `src/` (1,424 files, 28.9 MB of source) at width one with
/// `time_passes`: a pass's time over the bytes it read. These are estimates for deciding whether
/// work pays for a helper and for cutting it evenly, nothing else: a wrong one changes when items
/// run, never what they answer. A pass nobody measured takes the rate of the measured pass it
/// resembles, and says so; where that is a guess, the lower rate, which errs towards serial.
pub mod cost {
    /// Walks of an item's HIR that check attributes, stability, privacy, liveness or lints and
    /// infer nothing. `misc_checking_1` (the attribute walk, the unstable-API walk and a few
    /// crate lookups) is 54 ms, about 2 ns per byte for its two walks: 1 each.
    pub const WALK: u32 = 1;

    /// Type checking and the passes as heavy: `type_check_crate` 109 ms and
    /// `coherence_checking` 104 ms, about 4 ns per byte. Borrow checking, well-formedness and the
    /// facts' body walk (which type checks) were not measured on their own and take this rate;
    /// so does lowering, which lies between this and parsing (45 ns per byte), taking the lower.
    pub const TYPECK: u32 = 4;

    /// Late name resolution: `late_resolve_crate` 2,110 ms, about 73 ns per byte.
    pub const RESOLVE: u32 = 73;

    /// `bytes` of source at `ns_per_byte`, as a stage weight. Saturates.
    #[inline]
    pub fn weight(bytes: u32, ns_per_byte: u32) -> u32 {
        bytes.saturating_mul(ns_per_byte)
    }
}

impl<'scope, 'env> StageScope<'scope, 'env> {
    /// Complete a walk before returning its owned slots.
    pub fn stage<In, O, F>(&'scope self, input: In, len: usize, f: F) -> Arc<Slots<O>>
    where
        In: DynSync + DynSend + 'scope,
        O: DynSync + DynSend + 'scope,
        F: Fn(&In, usize) -> O + DynSync + DynSend + 'scope,
    {
        Arc::new(Slots { values: run_stage(input, len, f) })
    }

    /// Complete a weighted walk serially, without evaluating its scheduling estimate.
    pub fn stage_weighted<In, O, W, F>(
        &'scope self,
        input: In,
        len: usize,
        weight: W,
        f: F,
    ) -> Arc<Slots<O>>
    where
        In: DynSync + DynSend + 'scope,
        O: DynSync + DynSend + 'scope,
        W: Fn(&In, usize) -> u32,
        F: Fn(&In, usize) -> O + DynSync + DynSend + 'scope,
    {
        let _ = weight;
        self.stage(input, len, f)
    }

    /// Defer this serial walk until release, preserving work done between the two walks.
    pub fn stage_then<In, O, W, F>(
        &'scope self,
        upstream: &Arc<Slots<bool>>,
        input: In,
        len: usize,
        weight: W,
        f: F,
    ) -> Chain<'scope, In, O, F>
    where
        In: DynSync + DynSend + 'scope,
        O: DynSync + DynSend + 'scope,
        W: Fn(&In, usize) -> u32,
        F: Fn(&In, usize) -> O + DynSync + DynSend + 'scope,
    {
        assert_eq!(upstream.len(), len, "a chained stage has one item per upstream item");
        let _ = weight;
        Chain { input, len, f, _output: PhantomData, _scope: PhantomData }
    }

    /// Earlier serial walks have already emitted their diagnostics and completed.
    pub fn conclude_through<O>(&'scope self, _: &Arc<Slots<O>>) {}
}

/// A serial walk deferred to its original release point.
#[must_use = "a chained stage runs only when it is released"]
pub struct Chain<'scope, In, O, F> {
    input: In,
    len: usize,
    f: F,
    _output: PhantomData<O>,
    _scope: PhantomData<&'scope ()>,
}

impl<In, O, F> Chain<'_, In, O, F>
where
    F: Fn(&In, usize) -> O,
{
    /// Run every item in input order and return its completed slots.
    pub fn release(self) -> Arc<Slots<O>> {
        Arc::new(Slots {
            values: (0..self.len).map(|index| (self.f)(&self.input, index)).collect(),
        })
    }
}
