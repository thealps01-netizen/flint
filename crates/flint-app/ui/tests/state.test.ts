import { describe, expect, it } from "vitest";
import { Sequencer, move } from "../src/state";

describe("stale_results_are_dropped", () => {
  it("renders an answer only if it is newer than the one on screen", () => {
    const s = new Sequencer();
    const a = s.next();
    const b = s.next();
    expect(s.accept(b)).toBe(true);
    expect(s.accept(a)).toBe(false); // the older answer arrived late
    const c = s.next();
    expect(s.accept(c)).toBe(true);
    expect(s.accept(c)).toBe(false); // the same answer twice
  });

  it("keeps numbering above an earlier page after a reload", () => {
    // the app remembers the newest number for the whole process; a
    // reloaded page must not start below it, or every answer is dropped
    const before = new Sequencer(1_000_000);
    let last = 0;
    for (let i = 0; i < 5000; i++) last = before.next(); // over several seconds
    const after = new Sequencer(1_000_000 + 6); // reloaded 6 ms after the start
    expect(after.next()).toBeGreaterThan(last);
    expect(Number.isSafeInteger(new Sequencer(Date.now()).next())).toBe(true);
  });

  it("an emptied field makes every older answer stale", () => {
    const s = new Sequencer();
    const typed = s.next(); // still in flight
    s.clear(); // the user emptied the field
    expect(s.accept(typed)).toBe(false);
  });
});

describe("selection", () => {
  it("stops at the ends (no wrap)", () => {
    expect(move(0, -1, 5)).toBe(0);
    expect(move(4, 1, 5)).toBe(4);
    expect(move(1, 1, 5)).toBe(2);
    expect(move(0, 1, 0)).toBe(-1);
    expect(move(3, 10, 5)).toBe(4); // Page Down
  });
});
