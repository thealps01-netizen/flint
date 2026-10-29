import { formatDate, formatItems, formatSize } from "./format";
import { segments } from "./highlight";
import { t, type Lang } from "./i18n";

/** One search result as the app sends it (protocol `WireHit`). */
export interface Hit {
  path: string;
  name: string;
  is_dir: boolean;
  typo: boolean;
  highlights: [number, number][];
  last_change: number;
  score: number;
}

export interface Details {
  size: number | null;
  modified: number | null;
  typeName: string;
  items: number | null;
}

const ESC: Record<string, string> = { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" };
export const esc = (s: string) => s.replace(/[&<>"']/g, (c) => ESC[c]);

/** The folder part of a full path ("C:\Users\x\a.txt" → "C:\Users\x"). */
export function folderOf(path: string): string {
  const i = path.lastIndexOf("\\");
  if (i < 0) return "";
  return i === 2 && path[1] === ":" ? path.slice(0, 3) : path.slice(0, i);
}

export function nameHtml(h: Hit): string {
  return segments(h.name, h.highlights)
    .map((s) => (s.hit ? `<b>${esc(s.text)}</b>` : esc(s.text)))
    .join("");
}

export function iconUrl(path: string, isDir: boolean, px: number): string {
  // WebView2 maps a custom scheme to http(s)://<scheme>.localhost
  const scheme = location.protocol === "https:" ? "https" : "http";
  return `${scheme}://flinticon.localhost/${encodeURIComponent(path)}?dir=${isDir ? 1 : 0}&px=${px}`;
}

const GLYPH_FILE =
  '<svg class="i g" viewBox="0 0 24 24" aria-hidden="true"><path d="M7 3h7l5 5v11a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2zM14 3v5h5"/></svg>';
const GLYPH_DIR =
  '<svg class="i g" viewBox="0 0 24 24" aria-hidden="true"><path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/></svg>';
const OFTEN =
  '<svg class="i" viewBox="0 0 24 24" data-often role="img" aria-label="{0}"><path d="M3.5 12a8.5 8.5 0 1 0 2.6-6.1L3.5 8.5M3.5 4v4.5H8M12 7.5V12l3 2"/></svg>';

/**
 * The icon box: a neutral glyph that the real shell icon covers once it
 * loads (the box is the same size either way, so rows never shift).
 */
export function iconBox(h: Hit, px: number, key: string = h.path): string {
  return `<div class="t">${h.is_dir ? GLYPH_DIR : GLYPH_FILE}<img alt="" width="${px}" height="${px}" loading="lazy" decoding="async" data-key="${esc(`${key}|${px}`)}" data-src="${esc(iconUrl(h.path, h.is_dir, px))}"></div>`;
}

/** A result row. Size and date fill in later from `details`. */
export function rowHtml(h: Hit, i: number, selected: boolean, often: boolean, lang: Lang, iconKey?: string): string {
  const date = formatDate(h.last_change, Date.now(), lang);
  return (
    `<div class="r${selected ? " s" : ""}" role="option" id="opt-${i}" aria-selected="${selected}" data-i="${i}">` +
    iconBox(h, 32, iconKey) +
    `<div class="m"><div class="n"><span>${nameHtml(h)}</span>${often ? OFTEN.replace("{0}", esc(t(lang, "openedOften"))) : ""}</div>` +
    `<div class="p">${esc(folderOf(h.path))}</div></div>` +
    `<div class="d"><span data-size></span><span class="dd" data-date>${esc(date)}</span></div></div>`
  );
}

/** Size column text for a row from its details. */
export function sizeText(d: Details, lang: Lang): string {
  if (d.items !== null) return formatItems(d.items, lang);
  return d.size === null ? "" : formatSize(d.size, lang);
}

/** A path for a wrapping block: each folder name (with its "\") stays
 *  whole, and lines break only between them. */
const wrapPath = (p: string) =>
  p
    .split("\\")
    .map((s, i, all) => `<span class="ps">${esc(i < all.length - 1 ? `${s}\\` : s)}</span>`)
    .join("<wbr>");

export function detailsHtml(h: Hit, d: Details | null, often: boolean, lang: Lang, iconKey?: string): string {
  const rows: [string, string][] = [];
  if (d) {
    if (d.typeName) rows.push([t(lang, "type"), d.typeName]);
    if (d.items !== null) rows.push([t(lang, "contents"), formatItems(d.items, lang)]);
    else if (d.size !== null) rows.push([t(lang, "size"), formatSize(d.size, lang)]);
    if (d.modified) rows.push([t(lang, "modified"), formatDate(d.modified, Date.now(), lang)]);
  }
  if (often) rows.push([t(lang, "usage"), t(lang, "openedOften")]);
  return (
    `<div class="dh">${iconBox(h, 48, iconKey)}<div><div class="dn">${nameHtml(h)}</div><div class="dp">${wrapPath(folderOf(h.path))}</div></div></div>` +
    `<dl>${rows.map(([k, v]) => `<dt>${esc(k)}</dt><dd>${esc(v)}</dd>`).join("")}</dl>`
  );
}
