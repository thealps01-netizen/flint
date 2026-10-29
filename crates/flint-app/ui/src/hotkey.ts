/** A key press in the hotkey field. */
interface Press {
  code: string;
  ctrlKey: boolean;
  altKey: boolean;
  shiftKey: boolean;
  metaKey: boolean;
}

const NAMED = new Set(["Space", "Enter", "Tab", "Backquote", "Period", "Comma", "Slash", "Semicolon"]);
const MODIFIER = /^(Control|Alt|Shift|Meta|OS)(Left|Right)?$/;

/** The key's name as the app's hotkey parser accepts it, or "" if none. */
function keyName(code: string): string {
  if (/^Key[A-Z]$/.test(code)) return code.slice(3);
  if (/^Digit[0-9]$/.test(code)) return code.slice(5);
  if (/^F([1-9]|1[0-9]|2[0-4])$/.test(code)) return code;
  return NAMED.has(code) ? code : "";
}

/**
 * The hotkey a press makes ("Ctrl+Alt+K", modifiers in the app's order),
 * `null` while only modifiers are held, "" for a press that cannot be a
 * global hotkey (no modifier, Shift alone, or an unsupported key).
 */
export function hotkeyName(e: Press): string | null {
  if (MODIFIER.test(e.code)) return null;
  const key = keyName(e.code);
  const mods = [e.ctrlKey && "Ctrl", e.metaKey && "Win", e.altKey && "Alt", e.shiftKey && "Shift"].filter(Boolean);
  const shiftOnly = mods.length === 1 && e.shiftKey;
  if (!key || mods.length === 0 || shiftOnly) return "";
  return [...mods, key].join("+");
}
