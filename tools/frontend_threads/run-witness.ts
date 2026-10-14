type Outcome = { code: number; stdout: string; stderr: string };

async function run(binary: string, args: string[], input: string): Promise<Outcome> {
  const process = Bun.spawn([binary, ...args], {
    stdin: new TextEncoder().encode(input), stdout: "pipe", stderr: "pipe",
  });
  const [stdout, stderr, code] = await Promise.all([
    new Response(process.stdout).text(), new Response(process.stderr).text(), process.exited,
  ]);
  return { code, stdout, stderr };
}

function require(condition: boolean, name: string, outcome: Outcome): void {
  if (!condition) {
    throw new Error(`${name}: exit=${outcome.code}\nstdout=${outcome.stdout}\nstderr=${outcome.stderr}`);
  }
}

const [mode, binary] = Bun.argv.slice(2);
if (!binary || Bun.argv.length !== 4) {
  throw new Error("usage: bun run-witness.ts bridge|unwind|nested-defect|cleanup-defect|consume-defect|resume-defect EXECUTABLE");
}

switch (mode) {
  case "bridge": {
    const ordinary = await run(binary, [], "outer_input\n");
    require(ordinary.code === 0 && ordinary.stdout === "frontend_threads_bridge: outer_input\n",
      "frontend_threads_bridge_output", ordinary);
    const legacy = await run(binary, ["--legacy"], "outer_input\n");
    require(legacy.code === 0 && legacy.stdout === "frontend_threads_legacy_refusal: ok\n",
      "frontend_threads_legacy_output", legacy);
    console.log("frontend_threads_bridge_output: ok");
    break;
  }
  case "unwind": {
    const outcome = await run(binary, [], "original-request\n");
    require(outcome.code === 0 && outcome.stdout === "frontend_threads_unwind: original-request\n",
      "frontend_threads_unwind_output", outcome);
    console.log("frontend_threads_unwind_output: ok");
    break;
  }
  case "nested-defect":
  case "cleanup-defect":
  case "consume-defect":
  case "resume-defect": {
    const bridge = mode === "nested-defect" || mode === "cleanup-defect";
    const outcome = await run(binary, [], bridge ? "outer_input\n" : "original-request\n");
    const names: Record<string, string[]> = {
      "nested-defect": ["frontend_threads_nested_refusal"],
      "cleanup-defect": ["frontend_threads_panic_error_name", "frontend_threads_after_panic"],
      "consume-defect": ["frontend_threads_record_consumed"],
      "resume-defect": ["frontend_threads_resume_original", "frontend_threads_fresh_panic"],
    };
    require(outcome.code !== 0 && names[mode].some(name => outcome.stderr.includes(name)),
      `frontend_threads_${mode}_detected`, outcome);
    console.log(`frontend_threads_${mode}_detected: ok`);
    break;
  }
  default:
    throw new Error(`unknown witness mode: ${mode}`);
}
