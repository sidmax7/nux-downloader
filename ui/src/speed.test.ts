import { describe, expect, it } from "vitest";
import { emptyTracker, trackSpeeds } from "./speed";

const at = (seconds: number) => 1_000_000 + seconds * 1000;

describe("trackSpeeds", () => {
  it("reads zero on the first sample", () => {
    const t = trackSpeeds(emptyTracker(), [{ id: 1, bytes: 100, active: true }], at(0));
    expect(t.speeds.get(1)).toBe(0);
  });

  it("measures bytes per second between samples", () => {
    const first = trackSpeeds(emptyTracker(), [{ id: 1, bytes: 0, active: true }], at(0));
    const second = trackSpeeds(first, [{ id: 1, bytes: 1000, active: true }], at(1));
    expect(second.speeds.get(1)).toBe(1000);
  });

  it("smooths toward a new rate", () => {
    let t = trackSpeeds(emptyTracker(), [{ id: 1, bytes: 0, active: true }], at(0));
    t = trackSpeeds(t, [{ id: 1, bytes: 1000, active: true }], at(1));
    t = trackSpeeds(t, [{ id: 1, bytes: 1000, active: true }], at(2));
    const speed = t.speeds.get(1) ?? 0;
    expect(speed).toBeGreaterThan(0);
    expect(speed).toBeLessThan(1000);
  });

  it("reads zero at once for a paused item", () => {
    let t = trackSpeeds(emptyTracker(), [{ id: 1, bytes: 0, active: true }], at(0));
    t = trackSpeeds(t, [{ id: 1, bytes: 1000, active: true }], at(1));
    t = trackSpeeds(t, [{ id: 1, bytes: 1000, active: false }], at(2));
    expect(t.speeds.get(1)).toBe(0);
  });

  it("never reports a negative speed when a counter restarts", () => {
    let t = trackSpeeds(emptyTracker(), [{ id: 1, bytes: 5000, active: true }], at(0));
    t = trackSpeeds(t, [{ id: 1, bytes: 0, active: true }], at(1));
    expect(t.speeds.get(1)).toBe(0);
  });

  it("forgets items that left", () => {
    let t = trackSpeeds(emptyTracker(), [{ id: 1, bytes: 0, active: true }], at(0));
    t = trackSpeeds(t, [], at(1));
    expect(t.speeds.size).toBe(0);
    expect(t.bytes.size).toBe(0);
  });
});
