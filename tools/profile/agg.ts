const t = await Bun.file(Bun.argv[2]).text();
const m = new Map<string, number[]>();
for (const l of t.split("\n")) { const x = l.match(/^(\S+) .*check ([\d.]+) ms/); if (x) { (m.get(x[1]) ?? m.set(x[1], []).get(x[1])!).push(+x[2]); } }
for (const [k, v] of m) { v.sort((a,b)=>a-b); console.log(k, "min", v[0], "median", v[Math.floor(v.length/2)], "all", v.join(" ")); }
