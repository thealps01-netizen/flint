export interface Segment {
  text: string;
  hit: boolean;
}

/** A combining mark (an accent written as its own character). */
const MARK = /\p{M}/u;
const isLowSurrogate = (c: number) => c >= 0xdc00 && c <= 0xdfff;

/**
 * Splits `name` into plain and matched parts. Ranges are half-open UTF-16
 * offsets (the engine's units, the same as JavaScript strings); they are
 * sorted, merged, clamped, and widened so a letter keeps its combining
 * marks and a surrogate pair is never cut.
 */
export function segments(name: string, ranges: [number, number][]): Segment[] {
  const len = name.length;
  const fix = (i: number) => {
    let j = Math.max(0, Math.min(len, i));
    while (j > 0 && j < len && (isLowSurrogate(name.charCodeAt(j)) || MARK.test(name[j]))) j++;
    return j;
  };
  const rs = ranges
    .map(([a, b]) => [fix(a), fix(b)] as [number, number])
    .filter(([a, b]) => b > a)
    .sort((x, y) => x[0] - y[0]);
  const merged: [number, number][] = [];
  for (const r of rs) {
    const last = merged[merged.length - 1];
    if (last && r[0] <= last[1]) last[1] = Math.max(last[1], r[1]);
    else merged.push([r[0], r[1]]);
  }
  const out: Segment[] = [];
  let at = 0;
  for (const [a, b] of merged) {
    if (a > at) out.push({ text: name.slice(at, a), hit: false });
    out.push({ text: name.slice(a, b), hit: true });
    at = b;
  }
  if (at < len) out.push({ text: name.slice(at), hit: false });
  return out;
}
