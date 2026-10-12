// Speeds are not part of the engine's snapshot (only links have one), so
// the UI derives them from how far each item moved between snapshots.

export interface Sample {
  id: number;
  bytes: number;
  /** Whether it is transferring; a paused item reads 0 at once. */
  active: boolean;
}

export interface Tracker {
  at: number;
  bytes: Map<number, number>;
  speeds: Map<number, number>;
}

/** How much of the newest reading to blend in (the rest is history). */
const SMOOTHING = 0.35;

export function emptyTracker(): Tracker {
  return { at: 0, bytes: new Map(), speeds: new Map() };
}

/**
 * The next tracker state. `now` is in milliseconds. Items that are gone
 * drop out; a counter that went backwards (a restarted file) reads 0.
 */
export function trackSpeeds(prev: Tracker, samples: Sample[], now: number): Tracker {
  const seconds = (now - prev.at) / 1000;
  const bytes = new Map<number, number>();
  const speeds = new Map<number, number>();
  for (const sample of samples) {
    bytes.set(sample.id, sample.bytes);
    const before = prev.bytes.get(sample.id);
    if (!sample.active || before === undefined || seconds <= 0 || prev.at === 0) {
      speeds.set(sample.id, 0);
      continue;
    }
    const instant = Math.max(0, sample.bytes - before) / seconds;
    const old = prev.speeds.get(sample.id) ?? 0;
    speeds.set(sample.id, old === 0 ? instant : old + (instant - old) * SMOOTHING);
  }
  return { at: now, bytes, speeds };
}
