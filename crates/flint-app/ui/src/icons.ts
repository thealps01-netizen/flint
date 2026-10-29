/**
 * Loads row icons without flooding the app while the user types. Each
 * `img[data-src]` carries `data-key` (what its icon depends on, from the
 * app, plus the size): a key that loaded before is shown at once from the
 * address that loaded it, so the page's image cache answers and nothing is
 * asked; a new key is asked for only when `allowNew` (after typing pauses).
 * A loaded icon marks its box `.ok`, which hides the neutral glyph.
 */
export class IconLoader {
  private known = new Map<string, string>();

  load(root: ParentNode, allowNew: boolean) {
    for (const img of root.querySelectorAll<HTMLImageElement>("img[data-src]")) {
      const key = img.dataset.key ?? "";
      const seen = this.known.get(key);
      const box = img.parentElement;
      if (seen) {
        img.removeAttribute("data-src");
        img.onload = () => box?.classList.add("ok");
        img.src = seen;
        if (img.complete && img.naturalWidth > 0) box?.classList.add("ok");
        continue;
      }
      if (!allowNew) continue;
      const url = img.dataset.src!;
      img.removeAttribute("data-src");
      img.onload = () => {
        box?.classList.add("ok");
        this.known.set(key, url);
      };
      img.src = url;
    }
  }
}
