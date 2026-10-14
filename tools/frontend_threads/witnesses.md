These are functional witnesses, not unit tests or source-pattern verdicts.
The patch lane has not compiled or executed them. Integrators supply the
executables and lint driver using their own permitted build workflow.

Bridge: examples/frontend_threads_bridge_witness.rs accepts Rust tokens on
stdin. The runner requires exact unchanged output, restoration of the outer
symbol and bridge, a named nesting refusal, recovery after a client panic,
and a named refusal of the retained legacy CrossThread library field.

    bun tools/frontend_threads/run-witness.ts bridge /absolute/path/bridge-witness

Unwind: examples/frontend_threads_unwind_witness.rs accepts a message on
stdin, drives the installed catcher and recording hook, and checks record
consumption, the original message after resume and the next recorded panic.

    bun tools/frontend_threads/run-witness.ts unwind /absolute/path/unwind-witness

Use bridge-nesting-plant.md and unwind-plants.md to prepare one defect at a
time in an integrator-owned disposable copy. Build its linked witness outside
this lane. The runner accepts those executables in nested-defect,
cleanup-defect, consume-defect and resume-defect modes. A defect is detected
only when the actual entry point exits unsuccessfully with the specified
assertion name. A launch failure or unrelated compiler/runtime failure is
not evidence of detection.

Clippy: lint-witness.ts passes six real Rust inputs to the actual driver.
The probe functions are never executed. Each must fail its configured lint
with the exact owner reason; all compiler outputs are directed to /dev/null.
This invokes compilation through Clippy and is for the integrator only. The
read-only lane must not run it.

    bun tools/frontend_threads/lint-witness.ts /absolute/path/clippy-driver /absolute/repo

Named lint defect: frontend_threads_thread_ban_spawn. In an integrator-owned
copy, remove only the std::thread::spawn entry from clippy.toml. The same lint
witness must fail by that name because this first input now passes the lint.
Equivalent individual-entry defects are named builder_spawn, scope,
barrier, sender and condvar by the same runner.

No new concurrency harness is supplied for the excluded sync::stages or
eko::thread::scope sites. Serial unwind coverage does not establish the
per-thread isolation that remains necessary for embedding programs.
