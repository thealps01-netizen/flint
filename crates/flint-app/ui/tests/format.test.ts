import { describe, expect, it } from "vitest";
import { formatDate, formatItems, formatSize } from "../src/format";

const NOW = Date.UTC(2026, 9, 29, 14, 32) ; // 29 Oct 2026, 14:32 UTC
const ago = (s: number) => Math.floor(NOW / 1000) - s;

describe("formatSize", () => {
  it("uses the sketch's units", () => {
    expect(formatSize(512, "en")).toBe("512 B");
    expect(formatSize(48 * 1024, "en")).toBe("48 KB");
    expect(formatSize(6.3 * 1024, "en")).toBe("6 KB");
    expect(formatSize(1.2 * 1024 * 1024, "en")).toBe("1.2 MB");
    expect(formatSize(310 * 1024 * 1024, "en")).toBe("310 MB");
    expect(formatSize(3.5 * 1024 ** 3, "en")).toBe("3.5 GB");
    expect(formatSize(1.2 * 1024 * 1024, "tr")).toBe("1,2 MB");
  });
  it("counts folder items", () => {
    expect(formatItems(214, "en")).toBe("214 items");
    expect(formatItems(1, "en")).toBe("1 item");
    expect(formatItems(10000, "en")).toBe("10,000+ items");
    expect(formatItems(214, "tr")).toBe("214 öğe");
  });
});

describe("formatDate", () => {
  it("is relative when recent, a date when older (EN)", () => {
    expect(formatDate(ago(20), NOW, "en")).toBe("Just now");
    expect(formatDate(ago(5 * 60), NOW, "en")).toBe("5 minutes ago");
    expect(formatDate(ago(2 * 3600), NOW, "en")).toBe("2 hours ago");
    expect(formatDate(ago(20 * 3600), NOW, "en")).toBe("Yesterday");
    expect(formatDate(ago(3 * 86400), NOW, "en")).toBe("3 days ago");
    expect(formatDate(Date.UTC(2026, 9, 14, 12) / 1000, NOW, "en")).toBe("Oct 14");
    expect(formatDate(Date.UTC(2024, 4, 2, 12) / 1000, NOW, "en")).toBe("2024");
    expect(formatDate(0, NOW, "en")).toBe("");
  });
  it("speaks Turkish (TR)", () => {
    expect(formatDate(ago(2 * 3600), NOW, "tr")).toBe("2 saat önce");
    expect(formatDate(ago(20 * 3600), NOW, "tr")).toBe("Dün");
    expect(formatDate(Date.UTC(2026, 9, 14, 12) / 1000, NOW, "tr")).toBe("14 Eki");
  });
});
