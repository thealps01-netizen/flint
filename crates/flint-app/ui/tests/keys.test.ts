import { describe, expect, it } from "vitest";
import { actionFor } from "../src/keys";

const k = (key: string, mods: Partial<{ ctrlKey: boolean; shiftKey: boolean; altKey: boolean }> = {}) => ({
  key,
  ctrlKey: false,
  shiftKey: false,
  altKey: false,
  metaKey: false,
  ...mods,
});

describe("keys_map", () => {
  it("maps the launcher keys", () => {
    expect(actionFor(k("Enter"), false)).toBe("open");
    expect(actionFor(k("Enter", { ctrlKey: true }), false)).toBe("reveal");
    expect(actionFor(k("c", { ctrlKey: true }), false)).toBe("copy");
    expect(actionFor(k("C", { ctrlKey: true, shiftKey: true }), false)).toBe(null);
    expect(actionFor(k("Escape"), false)).toBe("hide");
    expect(actionFor(k("ArrowDown"), false)).toBe("down");
    expect(actionFor(k("ArrowUp"), false)).toBe("up");
    expect(actionFor(k("PageDown"), false)).toBe("pageDown");
    expect(actionFor(k(",", { ctrlKey: true }), false)).toBe("settings");
    expect(actionFor(k("a"), false)).toBe(null);
  });
  it("leaves Ctrl+C to the text field when text is selected in it", () => {
    expect(actionFor(k("c", { ctrlKey: true }), true)).toBe(null);
  });
  it("swallows the browser's reload and print keys", () => {
    expect(actionFor(k("F5"), false)).toBe("block");
    expect(actionFor(k("F5", { ctrlKey: true }), false)).toBe("block");
    expect(actionFor(k("r", { ctrlKey: true }), false)).toBe("block");
    expect(actionFor(k("R", { ctrlKey: true, shiftKey: true }), false)).toBe("block");
    expect(actionFor(k("p", { ctrlKey: true }), false)).toBe("block");
    expect(actionFor(k("BrowserRefresh"), false)).toBe("block");
  });
});
