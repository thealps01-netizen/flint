export type Action = "open" | "reveal" | "copy" | "hide" | "up" | "down" | "pageUp" | "pageDown" | "settings" | "block";

interface Key {
  key: string;
  ctrlKey: boolean;
  shiftKey: boolean;
  altKey: boolean;
  metaKey: boolean;
}

/** The launcher action for a key press; `null` leaves it to the text field. */
export function actionFor(e: Key, textSelected: boolean): Action | null {
  const plain = !e.ctrlKey && !e.altKey && !e.metaKey && !e.shiftKey;
  const ctrlOnly = e.ctrlKey && !e.altKey && !e.metaKey && !e.shiftKey;
  switch (e.key) {
    case "Enter":
      return plain ? "open" : ctrlOnly ? "reveal" : null;
    case "Escape":
      return "hide";
    case "ArrowDown":
      return plain ? "down" : null;
    case "ArrowUp":
      return plain ? "up" : null;
    case "PageDown":
      return plain ? "pageDown" : null;
    case "PageUp":
      return plain ? "pageUp" : null;
    case ",":
      return ctrlOnly ? "settings" : null;
    // the WebView's own reload and print: a reload would restart the page
    // mid-run, so they do nothing here
    case "F5":
    case "BrowserRefresh":
      return "block";
  }
  const ch = e.key.toLowerCase();
  if (e.ctrlKey && !e.altKey && !e.metaKey && (ch === "r" || ch === "p")) return "block";
  if (ctrlOnly && ch === "c" && !textSelected) return "copy";
  return null;
}
