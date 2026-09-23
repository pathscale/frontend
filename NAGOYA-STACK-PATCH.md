# nagoya: worker stack size

## The problem

frontend's parallel stages run compiler items (type checking, borrow checking, MIR building) on
nagoya's pool threads. rustc recurses deeply and is written for 16 MiB stacks: upstream starts
its threads at `DEFAULT_STACK_SIZE` (16 MiB, `src/rustc_interface/util.rs`), ekostd's threads
use 16 MiB, and this tree has no stack-growing fallback (no `stacker`, no
`ensure_sufficient_stack`), so a deep item on a small stack overflows and the process dies.

nagoya's `Runtime::with_tuning` (`src/runtime.rs`, the `std::thread::Builder` at line 88 of
`cac152e`) starts every worker with `std`'s default stack. `Runtime::new` and the shared
`runtime::background()` go through it. There is no way to ask for a bigger one.

## The minimal patch (to pathscale/nagoya, not applied)

A stack-size argument on the one constructor that starts threads, with the existing
constructors passing `None` so nothing that exists today changes:

```diff
--- a/src/runtime.rs
+++ b/src/runtime.rs
@@ impl Runtime {
     #[must_use]
     pub fn with_tuning(workers: usize, tuning: Tuning, label: &str) -> Self {
+        Self::with_stack(workers, tuning, label, None)
+    }
+
+    /// [`Runtime::with_tuning`], with each worker thread started on a stack of
+    /// `stack_bytes`, or on `std`'s default when `None`.
+    ///
+    /// For work that recurses deeply, such as a compiler's passes, which a
+    /// default-size thread stack cannot hold.
+    ///
+    /// # Panics
+    ///
+    /// As [`Runtime::with_tuning`].
+    #[must_use]
+    pub fn with_stack(
+        workers: usize,
+        tuning: Tuning,
+        label: &str,
+        stack_bytes: Option<usize>,
+    ) -> Self {
         let workers = workers.max(1);
@@
         for id in 0..workers {
             let pool = pool.clone();
             let runner = pool.runner(id);
-            std::thread::Builder::new()
-                .name(alloc::format!("{label}-{id}"))
+            let mut builder = std::thread::Builder::new().name(alloc::format!("{label}-{id}"));
+            if let Some(bytes) = stack_bytes {
+                builder = builder.stack_size(bytes);
+            }
+            builder
                 .spawn(move || {
```

With it, the embedding program (not frontend, which starts no threads) builds the pool and hands
it over, once, before the first parallel session:

```rust
let runtime = nagoya::runtime::Runtime::with_stack(
    workers,
    nagoya::Tuning::default(),
    "frontend",
    Some(16 * 1024 * 1024),
);
frontend::rustc_data_structures::sync::set_parallel_executor(runtime.executor().clone_handle())
    .ok();
// Keep `runtime` alive for the life of the process (dropping it does not stop the threads,
// but it is the handle a caller would use to read `spurious_wakes`).
```

## Does frontend work without it

Yes, with either of these, and neither needs a nagoya change:

1. **`RUST_MIN_STACK=16777216` in the environment** before the first parallel session.
   `std::thread::Builder::spawn` without `stack_size` reads that variable for its default, so
   `background()`'s workers start with 16 MiB. (frontend's own `init_stack_size` reads the same
   variable and still rejects a malformed value, as before.)
2. **A pool the program builds itself**: an `st3::fanout::Pool` whose threads the program starts
   with `std::thread::Builder::stack_size(16 << 20)` and runs with
   `nagoya::Executor::run_worker`, handed over with `sync::set_parallel_executor`.

Without either, parallel sessions run their items on default-size stacks (2 MiB for a `std`
thread on the platforms this is built for), and an item that recurses deeply enough overflows.
Serial sessions are unaffected: they run everything on the caller's own thread.
