// fold.ts SAMPLE_FILE [filter]: inclusive and self sample counts per function from macOS `sample`.
const text = await Bun.file(Bun.argv[2]).text();
const filter = Bun.argv[3] ?? "";
const lines = text.split("\n");
const start = lines.findIndex((l) => l.startsWith("Call graph:"));
const end = lines.findIndex((l, i) => i > start && l.startsWith("Total number in stack"));
import { idents } from "./name.ts";
function name(sym: string): string {
  const m = sym.match(/_R[A-Za-z0-9_]+/);
  if (!m) return sym.replace(/\s+\(in .*$/, "").replace(/\s+\+ .*$/, "").trim().slice(0, 60);
  const parts = idents(m[0])
    .filter((p) => !/^(frontend|std|core|alloc)$/.test(p));
  return parts.slice(-2).join("::");
}
const incl = new Map<string, number>(), self = new Map<string, number>();
const stack: { depth: number; n: number; name: string; kids: number }[] = [];
const flush = (upto: number) => {
  while (stack.length && stack[stack.length - 1].depth >= upto) {
    const f = stack.pop()!;
    self.set(f.name, (self.get(f.name) ?? 0) + Math.max(0, f.n - f.kids));
  }
};
for (const l of lines.slice(start + 1, end)) {
  const m = l.match(/^([ +!:|]*)(\d+) (.*)$/);
  if (!m) continue;
  const depth = m[1].length, n = +m[2], nm = name(m[3]);
  flush(depth);
  if (stack.length) stack[stack.length - 1].kids += n;
  // count inclusive once per distinct name on the current path
  if (!stack.some((f) => f.name === nm)) incl.set(nm, (incl.get(nm) ?? 0) + n);
  stack.push({ depth, n, name: nm, kids: 0 });
}
flush(0);
const show = (title: string, m: Map<string, number>) => {
  console.log(`== ${title}`);
  [...m].filter(([k]) => k.includes(filter)).sort((a, b) => b[1] - a[1]).slice(0, 25)
    .forEach(([k, v]) => console.log(String(v).padStart(7), k));
};
show("self", self);
show("inclusive", incl);
