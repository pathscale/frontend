// subself.ts SAMPLE... -- ROOT: self samples of frames under ROOT (by name substring), summed over files.
import { idents } from "./name.ts";
const args = Bun.argv.slice(2); const root = args.pop()!;
function name(sym: string): string {
  const m = sym.match(/_R[A-Za-z0-9_]+/);
  if (!m) return sym.replace(/\s+\(in .*$/, "").replace(/\s+\+ .*$/, "").trim().slice(0, 60);
  return idents(m[0]).filter((p) => !/^(frontend|std|core|alloc)$/.test(p)).slice(-3).join("::");
}
const self = new Map<string, number>(); let total = 0;
for (const file of args) {
  const lines = (await Bun.file(file).text()).split("\n");
  const stack: { depth: number; n: number; name: string; kids: number; inRoot: boolean }[] = [];
  const pop = (upto: number) => { while (stack.length && stack[stack.length-1].depth >= upto) { const f = stack.pop()!; if (f.inRoot) { const s = Math.max(0, f.n - f.kids); self.set(f.name, (self.get(f.name) ?? 0) + s); } } };
  for (const l of lines) {
    const m = l.match(/^([ +!:|]*)(\d+) (.*)$/); if (!m) continue;
    const depth = m[1].length, n = +m[2], nm = name(m[3]);
    pop(depth);
    const parentIn = stack.length ? stack[stack.length-1].inRoot : false;
    if (stack.length) stack[stack.length-1].kids += n;
    const isRoot = !parentIn && nm.includes(root);
    if (isRoot) total += n;
    stack.push({ depth, n, name: nm, kids: 0, inRoot: parentIn || isRoot });
  }
  pop(0);
}
console.log(`${root}: ${total} samples`);
[...self].sort((a,b)=>b[1]-a[1]).slice(0, +(Bun.env.TOP ?? 30)).forEach(([k,v]) => console.log(String(v).padStart(6), (100*v/total).toFixed(1).padStart(5)+'%', k));
