import type { Lang } from "./i18n";
import { t } from "./i18n";

const loc = (lang: Lang) => (lang === "tr" ? "tr-TR" : "en-US");

/** Bytes as the sketch shows them: "512 B", "48 KB", "1.2 MB". */
export function formatSize(bytes: number, lang: Lang): string {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = bytes;
  let u = 0;
  while (v >= 1024 && u < units.length - 1) {
    v /= 1024;
    u++;
  }
  // bytes and kilobytes whole; larger units keep one decimal under 100
  const digits = u >= 2 && v < 100 ? 1 : 0;
  const n = new Intl.NumberFormat(loc(lang), { maximumFractionDigits: digits }).format(v);
  return `${n} ${units[u]}`;
}

/** A folder's entry count (the app counts up to 10,000). */
export function formatItems(n: number, lang: Lang): string {
  const num = new Intl.NumberFormat(loc(lang)).format(n);
  if (n >= 10000) return t(lang, "itemsMany", num);
  return t(lang, n === 1 ? "item" : "items", num);
}

const cap = (s: string) => s.charAt(0).toLocaleUpperCase() + s.slice(1);

/** "Just now", "2 hours ago", "Yesterday", "3 days ago", "Oct 14", "2024". */
export function formatDate(unixSecs: number, nowMs: number, lang: Lang): string {
  if (!unixSecs) return "";
  const then = new Date(unixSecs * 1000);
  const now = new Date(nowMs);
  const secs = Math.max(0, Math.floor(nowMs / 1000) - unixSecs);
  const rtf = new Intl.RelativeTimeFormat(loc(lang), { numeric: "auto" });
  if (secs < 60) return t(lang, "justNow");
  if (secs < 3600) return rtf.format(-Math.floor(secs / 60), "minute");
  const day = (d: Date) => Date.UTC(d.getFullYear(), d.getMonth(), d.getDate()) / 86400000;
  const days = day(now) - day(then);
  if (days <= 0) return rtf.format(-Math.floor(secs / 3600), "hour");
  if (days === 1) return cap(rtf.format(-1, "day"));
  if (days < 7) return rtf.format(-days, "day");
  if (then.getFullYear() === now.getFullYear()) {
    return new Intl.DateTimeFormat(loc(lang), { month: "short", day: "numeric" }).format(then).replace(".", "");
  }
  return String(then.getFullYear());
}
