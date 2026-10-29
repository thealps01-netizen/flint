import { describe, expect, it } from "vitest";
import { detailsHtml, folderOf, rowHtml } from "../src/render";

const hit = (name: string) => ({
  path: "C:\\Users\\x\\" + name,
  name,
  is_dir: false,
  typo: false,
  highlights: [[0, 3]] as [number, number][],
  last_change: 0,
  score: 1,
});

describe("long_name_row_layout", () => {
  it("keeps one row structure whatever the name length", () => {
    const host = document.createElement("div");
    host.innerHTML = rowHtml(hit("a".repeat(400) + ".txt"), 0, false, false, "en");
    const row = host.firstElementChild as HTMLElement;
    expect(row.classList.contains("r")).toBe(true);
    expect(row.getAttribute("role")).toBe("option");
    expect(row.id).toBe("opt-0");
    // name and path are single lines that ellipsize (CSS class .n / .p)
    expect(row.querySelector(".n")).not.toBeNull();
    expect(row.querySelector(".p")!.textContent).toBe("C:\\Users\\x");
    // the icon box is reserved before the icon loads
    expect(row.querySelector(".t img")?.getAttribute("width")).toBe("32");
  });
  it("escapes names", () => {
    const host = document.createElement("div");
    host.innerHTML = detailsHtml(hit("<b>x</b>.txt"), null, false, "en");
    expect(host.querySelector(".dn b b")).toBeNull();
    // the folder wraps only after its separators: each folder name stays
    // whole (no break at a hyphen inside "ABAP-Performans")
    const dp = host.querySelector(".dp")!;
    expect([...dp.querySelectorAll(".ps")].map((s) => s.textContent)).toEqual(["C:\\", "Users\\", "x"]);
    expect(dp.querySelectorAll("wbr").length).toBe(2);
    expect(dp.textContent).toBe("C:\\Users\\x");
  });
  it("escapes names in rows", () => {
    const host = document.createElement("div");
    host.innerHTML = rowHtml(hit("<img src=x onerror=alert(1)>.txt"), 1, true, true, "en");
    expect(host.querySelector(".n img")).toBeNull();
    expect(host.firstElementChild!.getAttribute("aria-selected")).toBe("true");
    expect(host.querySelector("[data-often]")).not.toBeNull();
  });
});

describe("folderOf", () => {
  it("keeps the root of a drive", () => {
    expect(folderOf("C:\\a.txt")).toBe("C:\\");
    expect(folderOf("C:\\Users\\x\\a.txt")).toBe("C:\\Users\\x");
    expect(folderOf("a.txt")).toBe("");
  });
});

describe("icon_keys", () => {
  it("rows and the panel carry the icon key with their size", () => {
    const host = document.createElement("div");
    host.innerHTML = rowHtml(hit("a.pdf"), 0, false, false, "en", ".pdf");
    expect(host.querySelector("img")!.dataset.key).toBe(".pdf|32");
    host.innerHTML = detailsHtml(hit("a.pdf"), null, false, "en", ".pdf");
    expect(host.querySelector("img")!.dataset.key).toBe(".pdf|48");
  });
});
