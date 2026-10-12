import { createPinia, setActivePinia } from "pinia";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { pkg, snapshot } from "../testing";

vi.mock("../api", () => ({ api: {} }));
vi.mock("../bindings", () => ({ events: {} }));

import { useEngineStore } from "./engine";

beforeEach(() => setActivePinia(createPinia()));

describe("engine store", () => {
  it("shows the engine's measured speed for a downloading package", () => {
    const store = useEngineStore();
    store.apply(snapshot({ packages: [pkg({ speed: 500 })] }));
    expect(store.speedOf(store.packages[0])).toBe(500);
    expect(store.totalSpeed).toBe(500);
  });

  it("reads zero for a package that is not downloading", () => {
    const store = useEngineStore();
    store.apply(snapshot({ packages: [pkg({ speed: 500, state: "paused" })] }));
    expect(store.speedOf(store.packages[0])).toBe(0);
    expect(store.totalSpeed).toBe(0);
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

  it("totals what the downloads get", () => {
    const store = useEngineStore();
    store.apply(snapshot({ packages: [pkg({ id: 1, speed: 700 }), pkg({ id: 2, speed: 300 })] }));
    expect(store.totalSpeed).toBe(1000);
  });
});
