export function idents(sym: string): string[] {
  const out: string[] = [];
  let i = 0;
  while (i < sym.length) {
    if (/[1-9]/.test(sym[i])) {
      let j = i;
      while (j < sym.length && /[0-9]/.test(sym[j])) j++;
      const n = +sym.slice(i, j);
      const id = sym.slice(j, j + n);
      if (id.length === n && /^[A-Za-z_][A-Za-z0-9_]*$/.test(id)) { out.push(id); i = j + n; continue; }
    }
    i++;
  }
  return out;
}
