// phase.ts SAMPLE... : self samples of the main thread charged to the innermost pass on the stack.
import { idents } from "./name.ts";
const specific: [string, string][] = [
  ["rustc_borrowck", "borrow check"], ["check_liveness", "liveness"], ["rustc_mir_build", "MIR build"],
  ["rustc_mir_transform", "MIR passes"], ["rustc_mir_dataflow", "MIR passes"], ["rustc_hir_typeck", "type check bodies"],
  ["wfcheck", "well-formedness"], ["coherence", "coherence"], ["rustc_lint", "lints"], ["rustc_privacy", "privacy"],
  ["rustc_passes", "misc passes"], ["rustc_pattern_analysis", "match checking"], ["rustc_ast_lowering", "lowering"],
  ["late_resolve_crate", "late resolution"], ["rustc_resolve", "resolution"], ["rustc_expand", "expansion"],
  ["rustc_builtin_macros", "expansion"], ["rustc_parse", "parse"], ["rustc_lexer", "parse"],
  ["rustc_ast_passes", "AST checks"], ["rustc_hir_analysis", "hir analysis (signatures, collect)"],
  ["rustc_ty_utils", "hir analysis (signatures, collect)"],
];
const general: [string, string][] = [
  ["drop_in_place", "teardown"], ["new_lint_store", "session setup"], ["create_global_ctxt", "session setup"],
  ["run_compiler", "session setup/other"],
];
const totals = new Map<string, number>(); let all = 0;
const byFn = new Map<string, Map<string, number>>();
for (const file of Bun.argv.slice(2)) {
  const lines = (await Bun.file(file).text()).split("\n");
  let inMain = false;
  const stack: { depth: number; n: number; kids: number; parts: string }[] = [];
  const charge = (f: { n: number; kids: number }, path: string[]) => {
    const self = Math.max(0, f.n - f.kids); if (!self) return;
    let phase = "";
    for (const table of [specific, general]) {
      for (let i = path.length - 1; i >= 0 && !phase; i--) {
        const hit = table.find(([pat]) => path[i].includes(pat)); if (hit) phase = hit[1];
      }
      if (phase) break;
    }
    phase ||= "other";
    totals.set(phase, (totals.get(phase) ?? 0) + self); all += self;
    const leaf = path[path.length - 1].split("::").slice(-3).join("::");
    const per = byFn.get(phase) ?? byFn.set(phase, new Map()).get(phase)!;
    per.set(leaf, (per.get(leaf) ?? 0) + self);
  };
  const pop = (upto: number) => { while (stack.length && stack[stack.length-1].depth >= upto) { const f = stack.pop()!; charge(f, [...stack.map(s => s.parts), f.parts]); } };
  for (const l of lines) {
    if (l.startsWith("Total number in stack")) break;
    if (l.includes("Thread_")) { pop(0); inMain = l.includes("main-thread"); continue; }
    if (!inMain) continue;
    const m = l.match(/^([ +!:|]*)(\d+) (.*)$/); if (!m) continue;
    const depth = m[1].length, n = +m[2];
    const sym = m[3]; const mm = sym.match(/_R[A-Za-z0-9_]+/);
    const parts = mm ? idents(mm[0]).join("::") : sym.replace(/\s+\(in .*$/, "");
    pop(depth);
    if (stack.length) stack[stack.length-1].kids += n;
    stack.push({ depth, n, kids: 0, parts });
  }
  pop(0);
}
const runs = Bun.argv.length - 2;
console.log(`main thread, ${runs} runs, ${(all/runs).toFixed(0)} samples per run (1 ms each)`);
[...totals].sort((a,b)=>b[1]-a[1]).forEach(([k,v]) => console.log(String(Math.round(v/runs)).padStart(6), (100*v/all).toFixed(1).padStart(5)+"%", k));

for (const want of (Bun.env.PHASES ?? "").split(";").filter(Boolean)) {
  const per = byFn.get(want); if (!per) continue;
  const tot = [...per.values()].reduce((a, b) => a + b, 0);
  console.log("\n== " + want + " (" + Math.round(tot / runs) + " per run), own time by function");
  [...per].sort((a,b)=>b[1]-a[1]).slice(0, +(Bun.env.TOP ?? 18)).forEach(([k,v]) => console.log(String(Math.round(v/runs)).padStart(6), (100*v/tot).toFixed(1).padStart(5)+"%", k));
}
