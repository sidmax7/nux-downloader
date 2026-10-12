import { createPinia, setActivePinia } from "pinia";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { pkg, snapshot } from "../testing";

vi.mock("../api", () => ({ api: {} }));
vi.mock("../bindings", () => ({ events: {} }));

import { useEngineStore } from "./engine";

beforeEach(() => setActivePinia(createPinia()));

describe("engine store", () => {
  it("derives per-package speed from successive snapshots", () => {
    const store = useEngineStore();
    store.apply(snapshot({ packages: [pkg({ downloaded: 0 })] }), 1_000_000);
    store.apply(snapshot({ packages: [pkg({ downloaded: 500 })] }), 1_001_000);
    expect(store.speedOf(pkg())).toBe(500);
  });

  it("reads zero for a paused package", () => {
    const store = useEngineStore();
    store.apply(snapshot({ packages: [pkg({ downloaded: 0 })] }), 1_000_000);
    store.apply(snapshot({ packages: [pkg({ downloaded: 500, state: "paused" })] }), 1_001_000);
    expect(store.speedOf(pkg())).toBe(0);
  });

  it("replaces the snapshot whole", () => {
    const store = useEngineStore();
    const next = snapshot({ packages: [pkg()] });
    store.apply(next);
    expect(store.snapshot).toBe(next);
  });

  it("knows whether there is anything to pause or resume", () => {
    const store = useEngineStore();
    store.apply(snapshot({ packages: [pkg({ state: "paused" })] }));
    expect(store.hasRunning).toBe(false);
    expect(store.hasPaused).toBe(true);
    store.apply(snapshot({ packages: [pkg({ state: "queued" })] }));
    expect(store.hasRunning).toBe(true);
  });

  it("counts a link's speed only while it has connections", () => {
    const store = useEngineStore();
    const link = (name: string, active: number, speed: number) => ({
      name,
      id: 1,
      label: name,
      color: null,
      kind: "wifi" as const,
      address: null,
      enabled: true,
      degraded: false,
      active,
      bytes: 0,
      speed,
      limit: null,
    });
    store.apply(snapshot({ links: [link("a", 2, 1000), link("b", 0, 99)] }));
    expect(store.totalSpeed).toBe(1000);
  });
});
