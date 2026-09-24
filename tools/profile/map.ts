// map.ts A B: per-pass seconds summed over a run, side by side.
const read = async (f: string) => {
  const m = new Map<string, number>();
  for (const line of (await Bun.file(f).text()).split("\n")) {
    if (!line.startsWith("time: {")) continue;
    let e; try { e = JSON.parse(line.slice(6)); } catch { continue; }
    m.set(e.pass, (m.get(e.pass) ?? 0) + e.time);
  }
  return m;
};
const [a, b] = [await read(Bun.argv[2]), await read(Bun.argv[3])];
const keys = [...new Set([...a.keys(), ...b.keys()])].sort((x, y) => (a.get(y) ?? 0) - (a.get(x) ?? 0));
for (const k of keys.slice(0, 40)) {
  const x = (a.get(k) ?? 0) * 1000, y = (b.get(k) ?? 0) * 1000;
  if (x < 1 && y < 1) continue;
  console.log(`${x.toFixed(0).padStart(7)} ${y.toFixed(0).padStart(7)} ${(x / (y || 1)).toFixed(2).padStart(6)}x  ${k}`);
}
