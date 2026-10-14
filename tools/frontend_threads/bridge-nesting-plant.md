Named defect: frontend_threads_nested_refusal.

Apply only to a disposable integrator-owned copy after part2. In
src/rustc_proc_macro/bridge/server.rs replace the single line
    if ACTIVE_CLIENT.get() {
with
    if false {
inside ActiveClientGuard::enter. This admits a nested client and clears the
outer symbol interner. The linked example must fail at the assertion named
frontend_threads_nested_refusal (reported by the real proc-macro panic path),
and cannot print frontend_threads_bridge: outer_input.

Positive witness: supply outer_input on stdin to the integrator-built
frontend_threads_bridge_witness executable. Require exit 0 and stdout exactly
frontend_threads_bridge: outer_input, followed by a newline. Repeat with
--legacy; require exit 0 and frontend_threads_legacy_refusal: ok.

Guard-cleanup defect: remove ACTIVE_CLIENT.set(false) from ActiveClientGuard::drop.
The same real input must fail by frontend_threads_panic_error_name or
frontend_threads_after_panic because the next client is refused as nested.

These are supplied witnesses and exact plants, not executed evidence. The
read-only patch lane does not build, mutate a copy, or run either executable.
