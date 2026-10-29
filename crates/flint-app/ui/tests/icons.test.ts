import { describe, expect, it } from "vitest";
import { IconLoader } from "../src/icons";

function box(key: string, url: string) {
  const host = document.createElement("div");
  host.innerHTML = `<div class="t"><img data-key="${key}" data-src="${url}"></div>`;
  return host;
}
const img = (host: HTMLElement) => host.querySelector("img")!;

describe("icon_loading", () => {
  it("asks for an unknown icon only when new requests are allowed", () => {
    const l = new IconLoader();
    const host = box(".pdf|32", "u/a.pdf");
    l.load(host, false);
    expect(img(host).getAttribute("src")).toBeNull();
    l.load(host, true);
    expect(img(host).getAttribute("src")).toBe("u/a.pdf");
  });

  it("shows a known icon at once, from the address that loaded it", () => {
    const l = new IconLoader();
    const first = box(".pdf|32", "u/a.pdf");
    l.load(first, true);
    img(first).dispatchEvent(new Event("load"));
    expect(img(first).parentElement!.classList.contains("ok")).toBe(true);
    // another pdf, while typing: no new request, the same address
    const second = box(".pdf|32", "u/b.pdf");
    l.load(second, false);
    expect(img(second).getAttribute("src")).toBe("u/a.pdf");
    expect(img(second).hasAttribute("data-src")).toBe(false);
  });

  it("does not remember an icon that failed", () => {
    const l = new IconLoader();
    const first = box(".x|32", "u/a.x");
    l.load(first, true);
    img(first).dispatchEvent(new Event("error"));
    const second = box(".x|32", "u/b.x");
    l.load(second, false);
    expect(img(second).getAttribute("src")).toBeNull();
  });
});
