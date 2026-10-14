Named defect: frontend_threads_record_consumed.

Apply only to a disposable integrator-owned copy. In src/unwind_janky/mod.rs
replace its single line
    LAST_PANIC.with(|| None, Option::take).flatten()
with
    LAST_PANIC.with(|| None, |slot| slot.clone()).flatten()

An integrator-built frontend_threads_unwind_witness must then fail with the
assertion name frontend_threads_record_consumed, because a second consumer
can read the first consumer's message.

Named defect: frontend_threads_fresh_panic.

In src/unwind_janky/mod.rs replace the single condition
    if RESUMING.with(|| false, |resuming| core::mem::replace(resuming, false)) == Some(true) {
with
    if RESUMING.with(|| false, |resuming| core::mem::replace(resuming, true)) == Some(true) {

The real input must fail at frontend_threads_resume_original or
frontend_threads_fresh_panic: the resume flag stops being consumed.

Positive witness: stdin original-request yields exit 0 and stdout exactly
frontend_threads_unwind: original-request, followed by a newline. It drives
the public library API through an installed std catcher and recording hook.
It creates no OS threads and makes no claim about cross-worker isolation.

No plant, compilation, or functional witness was executed in the patch lane.
