import { idents } from "./name.ts";
const cats: [RegExp, string][] = [
  [/rustc_query_impl|query::plumbing|query::caches|try_execute_query|execute_job|JobOwner|QueryJob|query::job/, "query machinery"],
  [/dep_graph/, "dep graph"],
  [/^_xzm|malloc|_free$|free_|GlobalAlloc|alloc_zeroed|__rust_alloc|__rust_dealloc|realloc/, "allocator"],
  [/memmove|memcpy|memset|bzero/, "memory copy/zero"],
  [/intern_|Interned|InternSet|mk_args|mk_ty|mk_type_list|intern_predicate/, "interning"],
  [/obligation_forest|fulfill|FulfillProcessor|process_obligations|register_obligation/, "obligation processing"],
  [/FxHash|hashbrown|FxBuildHasher|RawTable/, "hash tables"],
];
const tot = new Map<string, number>(); let all = 0;
for (const file of Bun.argv.slice(2)) {
  const lines = (await Bun.file(file).text()).split("\n"); let inMain = false;
  const st: { d: number; n: number; k: number; name: string }[] = [];
  const pop = (u: number) => { while (st.length && st[st.length-1].d >= u) { const f = st.pop()!; const s = Math.max(0, f.n - f.k); if (!s) continue; all += s; const c = cats.find(([re]) => re.test(f.name)); const key = c ? c[1] : "compiler logic"; tot.set(key, (tot.get(key) ?? 0) + s); } };
  for (const l of lines) {
    if (l.startsWith("Total number in stack")) break;
    if (l.includes("Thread_")) { pop(0); inMain = l.includes("main-thread"); continue; }
    if (!inMain) continue;
    const m = l.match(/^([ +!:|]*)(\d+) (.*)$/); if (!m) continue;
    const d = m[1].length, n = +m[2]; const mm = m[3].match(/_R[A-Za-z0-9_]+/);
    const name = mm ? idents(mm[0]).join("::") : m[3].replace(/\s+\(in .*$/, "").trim();
    pop(d); if (st.length) st[st.length-1].k += n; st.push({ d, n, k: 0, name });
  }
  pop(0);
}
const runs = Bun.argv.length - 2;
[...tot].sort((a,b)=>b[1]-a[1]).forEach(([k,v]) => console.log(String(Math.round(v/runs)).padStart(6), (100*v/all).toFixed(1).padStart(5)+"%", k));
