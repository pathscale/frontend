const [driver, repo] = Bun.argv.slice(2);
if (!driver || !repo || Bun.argv.length !== 4) {
  throw new Error("usage: bun lint-witness.ts CLIPPY_DRIVER ABSOLUTE_REPO");
}
if (!repo.startsWith("/")) throw new Error("repository path must be absolute");

const reason = "owner ruling: typed Arc<T> hand-off, concurrency only via nagoya/ps-st3";
const probes = [
  { name: "spawn", lint: "disallowed_methods", source: "pub fn banned() { let _ = std::thread::spawn(|| ()); }" },
  { name: "builder_spawn", lint: "disallowed_methods", source: "pub fn banned() { let _ = std::thread::Builder::new().spawn(|| ()); }" },
  { name: "scope", lint: "disallowed_methods", source: "pub fn banned() { std::thread::scope(|_| ()); }" },
  { name: "barrier", lint: "disallowed_types", source: "pub fn banned(_: std::sync::Barrier) {}" },
  { name: "sender", lint: "disallowed_types", source: "pub fn banned(_: std::sync::mpsc::Sender<()>) {}" },
  { name: "condvar", lint: "disallowed_types", source: "pub fn banned(_: std::sync::Condvar) {}" },
];

for (const probe of probes) {
  // Standalone Rust inputs reach the real lint driver. They are never run and
  // do not enter frontend's source tree or start any OS threads.
  const input = `#![deny(clippy::disallowed_methods, clippy::disallowed_types)]\n${probe.source}\n`;
  const process = Bun.spawn([
    driver, "--edition=2024", "--crate-name", `frontend_threads_lint_${probe.name}`,
    "--crate-type=lib", "--emit=metadata", "-o", "/dev/null", "-",
  ], {
    cwd: repo,
    env: { ...Bun.env, CLIPPY_CONF_DIR: repo, CARGO_INCREMENTAL: "0" },
    stdin: new TextEncoder().encode(input), stdout: "pipe", stderr: "pipe",
  });
  const [stdout, stderr, code] = await Promise.all([
    new Response(process.stdout).text(), new Response(process.stderr).text(), process.exited,
  ]);
  const diagnostics = stdout + stderr;
  if (code === 0 || !diagnostics.includes(reason) || !diagnostics.includes(probe.lint)) {
    throw new Error(`frontend_threads_thread_ban_${probe.name}: exit=${code}\n${diagnostics}`);
  }
  console.log(`frontend_threads_thread_ban_${probe.name}: ok`);
}
