/** Numbers each query; an answer is shown only if newer than the one on screen. */
export class Sequencer {
  private sent: number;
  private shown: number;

  /**
   * `clockMs` (Date.now() in the app) starts the numbers at clockMs × 1000:
   * the app keeps the newest number for its whole run, so a reloaded page
   * must number above the page before it (up to 1000 queries per ms).
   */
  constructor(clockMs = 0) {
    this.sent = clockMs * 1000;
    this.shown = this.sent;
  }

  next(): number {
    return ++this.sent;
  }

  accept(seq: number): boolean {
    if (seq <= this.shown) return false;
    this.shown = seq;
    return true;
  }

  /** The field was emptied: every answer still in flight is stale. */
  clear() {
    this.shown = this.sent;
  }
}

/** Moves the selection by `delta` rows, stopping at the ends; -1 when empty. */
export function move(sel: number, delta: number, count: number): number {
  if (count <= 0) return -1;
  return Math.max(0, Math.min(count - 1, sel + delta));
}
