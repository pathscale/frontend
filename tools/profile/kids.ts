// kids.ts SAMPLE_FILE NAME_SUBSTRING: direct callees of every frame whose name contains NAME, summed.
import { idents } from "./name.ts";
const [file, want] = [Bun.argv[2], Bun.argv[3]];
const lines = (await Bun.file(file).text()).split("\n");
function name(sym: string): string {
  const m = sym.match(/_R[A-Za-z0-9_]+/);
  if (!m) return sym.replace(/\s+\(in .*$/, "").replace(/\s+\+ .*$/, "").trim().slice(0, 60);
  return idents(m[0]).filter((p) => !/^(frontend|std|core|alloc)$/.test(p)).slice(-2).join("::");
}
const kids = new Map<string, number>();
let total = 0;
const stack: { depth: number; name: string; n: number }[] = [];
for (const l of lines) {
  const m = l.match(/^([ +!:|]*)(\d+) (.*)$/);
  if (!m) continue;
  const depth = m[1].length, n = +m[2], nm = name(m[3]);
  while (stack.length && stack[stack.length - 1].depth >= depth) stack.pop();
  const parent = stack[stack.length - 1];
  const inside = stack.some((f, i) => f.name.includes(want) && i < stack.length - 1);
  if (parent && parent.name.includes(want) && !inside) { kids.set(nm, (kids.get(nm) ?? 0) + n); }
  if (nm.includes(want) && !stack.some((f) => f.name.includes(want))) total += n;
  stack.push({ depth, name: nm, n });
}
console.log(`${want}: ${total}`);
[...kids].sort((a, b) => b[1] - a[1]).slice(0, 20).forEach(([k, v]) => console.log(String(v).padStart(7), k));
