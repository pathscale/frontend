# speed-fanout

A caller-side driver for frontend's reading-only entry points on several threads. frontend
itself spawns nothing and holds no pool; this crate is the caller that owns the threads.

- `Driver::new(n)` starts `n` workers, each an `eko::thread::Builder` thread with a 16 MB stack,
  each running one worker of a ps-st3 fanout `Pool` (which spawns nothing). `src/host.rs` is the
  pool's `Host`, over `eko`'s Mutex and Condvar.
- `parses_as_many`, `site_at_many`, `diagnose_many` and `analyze_many` take slices and return
  answers in input order, the same answers the single calls give. Parse-only work runs under
  shared session globals recycled every `RECYCLE_EVERY` calls; `analyze_source` runs with none.
- Every input runs inside `unwind_janky::catch`, so a panic is that input's error and the worker
  carries on. Dropping the driver joins every worker.

Not published. Outside frontend's workspace, and a workspace of its own.

## Run

From this directory:

```sh
cargo test --release
cargo run --release --example scaling -- 1
```

`scaling` is bounded by work: every `../src/**/*.rs` plus fixed fragments and 200 small
sources, a fixed number of passes. It prints each kind at 1, 2, 4, 8 and 12 workers beside a
serial loop and that loop's repeat (the noise floor), and stops if any arm answers differently
from the serial one. The machine is shared: compare arms within one run only.
