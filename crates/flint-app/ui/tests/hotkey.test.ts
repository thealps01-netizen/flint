import { describe, expect, it } from "vitest";
import { hotkeyName } from "../src/hotkey";

const k = (code: string, mods: Partial<Record<"ctrlKey" | "altKey" | "shiftKey" | "metaKey", boolean>> = {}) => ({
  code,
  ctrlKey: false,
  altKey: false,
  shiftKey: false,
  metaKey: false,
  ...mods,
});

describe("hotkey_capture", () => {
  it("names a combination the way the app parses it", () => {
    expect(hotkeyName(k("Space", { altKey: true }))).toBe("Alt+Space");
    expect(hotkeyName(k("KeyK", { ctrlKey: true, altKey: true }))).toBe("Ctrl+Alt+K");
    expect(hotkeyName(k("Digit1", { metaKey: true, shiftKey: true }))).toBe("Win+Shift+1");
    expect(hotkeyName(k("F5", { ctrlKey: true }))).toBe("Ctrl+F5");
    expect(hotkeyName(k("Backquote", { ctrlKey: true }))).toBe("Ctrl+Backquote");
  });
  it("waits while only modifiers are held", () => {
    expect(hotkeyName(k("AltLeft", { altKey: true }))).toBeNull();
    expect(hotkeyName(k("ControlLeft", { ctrlKey: true }))).toBeNull();
  });
  it("rejects what cannot be a global hotkey", () => {
    expect(hotkeyName(k("KeyK"))).toBe(""); // no modifier
    expect(hotkeyName(k("KeyK", { shiftKey: true }))).toBe(""); // would take a typed letter
    expect(hotkeyName(k("ArrowUp", { ctrlKey: true }))).toBe(""); // a key the app does not accept
  });
});
