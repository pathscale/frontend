//! Typed owned work submitted as plain nagoya tasks.
//!
//! There is no captured compiler context. Jobs own Arc input and a function pointer, and move
//! their outputs into completions. The caller can claim queued work before awaiting it, including
//! in nested jobs. Completion futures register wakers under the result lock and never spin.

use alloc::sync::Arc;
use alloc::vec::Vec;

/// Run owned work, returning owned results in input order.
///
/// `run` must use only its owned input and locally created context. Diagnostics and other effects
/// belong in its output. `weight` estimates relative work without querying compiler state.
/// Borrowed compiler walks use run_stage on the caller instead.
pub fn run_owned_stage<In: ?Sized, O>(
    input: Arc<In>,
    len: usize,
    weight: fn(&In, usize) -> u32,
    run: fn(&In, usize) -> O,
) -> Vec<O>
where
    In: Send + Sync + 'static,
    O: Send + 'static,
{
    #[cfg(feature = "parallel")]
    if len > 1 && super::is_parallel_here() {
        return parallel::run(input, len, weight, run);
    }
    let _ = weight;
    (0..len).map(|index| run(&input, index)).collect()
}

#[cfg(feature = "parallel")]
mod parallel {
    use super::*;
    use alloc::string::String;
    use core::future::Future;
    use core::pin::Pin;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use core::task::{Context, Poll, Waker};
    use parking_lot::Mutex;

    use crate::rustc_data_structures::sync::{enter_session_width, pool};
    use crate::unwind_janky::{self, Payload};

    struct Panicked {
        payload: Payload,
        message: Option<String>,
    }

    type Outcome<O> = Result<O, Panicked>;

    struct Job<In: ?Sized, O> {
        input: Arc<In>,
        index: usize,
        run: fn(&In, usize) -> O,
    }

    struct Completion<O> {
        outcome: Option<Outcome<O>>,
        waiter: Option<Waker>,
    }

    struct Task<In: ?Sized, O> {
        job: Mutex<Option<Job<In, O>>>,
        completion: Mutex<Completion<O>>,
    }

    impl<In: Send + Sync + ?Sized + 'static, O: Send + 'static> Task<In, O> {
        fn run(&self, width: usize) {
            let Some(job) = self.job.lock().take() else { return };
            let outcome = pool::catch(move || {
                let _mode = enter_session_width(width);
                let value = (job.run)(&job.input, job.index);
                // A completed task holds no input. Queued callbacks can outlive the owner's
                // return without retaining parser state or preventing its Arc commit check.
                drop(job);
                value
            })
            .map_err(|payload| Panicked { payload, message: unwind_janky::take_last_panic() });
            let waiter = {
                let mut completion = self.completion.lock();
                completion.outcome = Some(outcome);
                completion.waiter.take()
            };
            if let Some(waiter) = waiter {
                waiter.wake();
            }
        }
    }

    struct Ready<'a, In: ?Sized, O> {
        task: &'a Task<In, O>,
    }

    impl<In: ?Sized, O> Future for Ready<'_, In, O> {
        type Output = Outcome<O>;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let mut completion = self.task.completion.lock();
            match completion.outcome.take() {
                Some(outcome) => Poll::Ready(outcome),
                None => {
                    completion.waiter = Some(cx.waker().clone());
                    Poll::Pending
                }
            }
        }
    }

    struct Work<In: ?Sized, O> {
        tasks: Vec<Arc<Task<In, O>>>,
        order: Vec<usize>,
        next: AtomicUsize,
        width: usize,
    }

    /// One owned item per executor task. A successor inherits this helper's budget place.
    fn help<In: Send + Sync + ?Sized + 'static, O: Send + 'static>(work: Arc<Work<In, O>>) {
        let next = work.next.fetch_add(1, Ordering::Relaxed);
        let Some(&index) = work.order.get(next) else { return };
        work.tasks[index].run(work.width);
        if next + 1 < work.order.len() {
            pool::submit_continuation(move || help(work));
        }
    }

    pub(super) fn run<In: Send + Sync + ?Sized + 'static, O: Send + 'static>(
        input: Arc<In>,
        len: usize,
        weight: fn(&In, usize) -> u32,
        run: fn(&In, usize) -> O,
    ) -> Vec<O> {
        let workers = pool::workers();
        let width = pool::width().min(workers.saturating_add(1));
        if width < 2 {
            return (0..len).map(|index| run(&input, index)).collect();
        }
        // The owner starts item zero. Helpers prefer larger remaining items, with the original
        // index breaking ties. Scheduling never changes result or diagnostic replay order.
        let mut order: Vec<_> = (1..len).map(|index| (index, weight(&input, index))).collect();
        order.sort_unstable_by(|(a, aw), (b, bw)| bw.cmp(aw).then_with(|| a.cmp(b)));
        let tasks = (0..len)
            .map(|index| {
                Arc::new(Task {
                    job: Mutex::new(Some(Job { input: Arc::clone(&input), index, run })),
                    completion: Mutex::new(Completion { outcome: None, waiter: None }),
                })
            })
            .collect();
        // Only the take-once jobs own the input from here on.
        drop(input);
        let work = Arc::new(Work {
            tasks,
            order: order.into_iter().map(|(index, _)| index).collect(),
            next: AtomicUsize::new(0),
            width,
        });
        for _ in 0..(width - 1).min(len - 1) {
            let work = Arc::clone(&work);
            if !pool::try_submit(move || help(work)) {
                break;
            }
        }
        let mut values = Vec::with_capacity(len);
        let mut first_panic = None;
        for task in &work.tasks {
            // If queued, claim it here. If running elsewhere, await only that running item.
            task.run(width);
            match pool::block_on(Ready { task }) {
                Ok(value) => values.push(value),
                Err(panic) => {
                    if first_panic.is_none() {
                        first_panic = Some(panic);
                    }
                }
            }
        }
        if let Some(panic) = first_panic {
            pool::resume(panic.payload, panic.message);
        }
        values
    }
}
