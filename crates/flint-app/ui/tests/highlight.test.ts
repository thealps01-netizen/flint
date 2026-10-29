import { describe, expect, it } from "vitest";
import { segments } from "../src/highlight";

const marked = (name: string, r: [number, number][]) =>
  segments(name, r).filter((s) => s.hit).map((s) => s.text);

describe("highlight_utf16_ranges", () => {
  it("marks exactly the matched characters", () => {
    expect(marked("Bütçe_2026.xlsx", [[0, 5]])).toEqual(["Bütçe"]);
    expect(segments("Bütçe_2026.xlsx", [[0, 5]]).map((s) => s.text).join("")).toBe("Bütçe_2026.xlsx");
  });
  it("counts an emoji as two UTF-16 units, like the engine", () => {
    // "📊 rapor.txt": the emoji is units 0-1, the space 2, "rapor" 3-8
    expect(marked("📊 rapor.txt", [[3, 8]])).toEqual(["rapor"]);
  });
  it("keeps a combining mark with its letter", () => {
    // "Bu\u0308tc\u0327e": a decomposed "Bütçe"
    const name = "Bu\u0308tc\u0327e";
    // a range that ends between a letter and its mark grows to include it
    expect(marked(name, [[0, 2]])).toEqual(["Bu\u0308"]);
  });
  it("sorts, merges and clamps ranges", () => {
    expect(marked("rapor-taslak", [[6, 12], [0, 3], [2, 5], [40, 50]])).toEqual(["rapor", "taslak"]);
    expect(marked("abc", [])).toEqual([]);
  });
});
