// parents.ts SAMPLE_FILE NAME [levels]: caller chains of frames whose name contains NAME.
import { idents } from "./name.ts";
const [file, want, lv] = [Bun.argv[2], Bun.argv[3], +(Bun.argv[4] ?? 8)];
const lines = (await Bun.file(file).text()).split("\n");
function name(sym: string): string {
  const m = sym.match(/_R[A-Za-z0-9_]+/);
  if (!m) return sym.replace(/\s+\(in .*$/, "").replace(/\s+\+ .*$/, "").trim().slice(0, 50);
  return idents(m[0]).filter((p) => !/^(frontend|std|core|alloc)$/.test(p)).slice(-2).join("::");
}
const chains = new Map<string, number>();
const stack: { depth: number; name: string }[] = [];
for (const l of lines) {
  const m = l.match(/^([ +!:|]*)(\d+) (.*)$/);
  if (!m) continue;
  const depth = m[1].length, n = +m[2], nm = name(m[3]);
  while (stack.length && stack[stack.length - 1].depth >= depth) stack.pop();
  if (nm.includes(want)) {
    const chain = stack.slice(-lv).map((f) => f.name).filter((x) => !x.startsWith("<dedup")).join(" <- ".length ? " > " : "");
    chains.set(chain, (chains.get(chain) ?? 0) + n);
  }
  stack.push({ depth, name: nm });
}
[...chains].sort((a, b) => b[1] - a[1]).slice(0, 12).forEach(([k, v]) => console.log(String(v).padStart(6), k));
