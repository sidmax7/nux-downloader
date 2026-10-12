import { flushPromises, mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { pkg } from "../testing";

const api = vi.hoisted(() => ({
  pause: vi.fn(),
  resume: vi.fn(),
  remove: vi.fn(),
  setPostOptions: vi.fn(),
  setPackageLimit: vi.fn(),
  setPriority: vi.fn(),
  move: vi.fn(),
  restartFile: vi.fn(),
  blocks: vi.fn(),
}));
vi.mock("../api", () => ({ api }));
vi.mock("../bindings", () => ({ events: {} }));

import PackageRow from "./PackageRow.vue";

beforeEach(() => {
  setActivePinia(createPinia());
  vi.resetAllMocks();
  api.pause.mockResolvedValue(null);
  api.resume.mockResolvedValue(null);
  api.remove.mockResolvedValue(null);
  api.setPostOptions.mockResolvedValue(null);
  api.blocks.mockResolvedValue({ blocks: [] });
});

const mountRow = (over = {}) =>
  mount(PackageRow, { props: { pkg: pkg(over), index: 1, count: 3 } });

describe("PackageRow", () => {
  it("shows name, size and progress", () => {
    const wrapper = mountRow();
    expect(wrapper.text()).toContain("Movie pack");
    expect(wrapper.text()).toContain("250 B of 1000 B");
    expect(wrapper.find("[role=progressbar]").attributes("aria-valuenow")).toBe("25");
  });

  it("offers Pause while downloading and Resume while paused", () => {
    const running = mountRow();
    expect(running.find('button[aria-label="Pause Movie pack"]').exists()).toBe(true);
    expect(running.find('button[aria-label="Resume Movie pack"]').exists()).toBe(false);
    const paused = mountRow({ state: "paused" });
    expect(paused.find('button[aria-label="Resume Movie pack"]').exists()).toBe(true);
    expect(paused.find('button[aria-label="Pause Movie pack"]').exists()).toBe(false);
  });

  it("shows why a package paused or failed", () => {
    expect(mountRow({ state: "paused", error: "disk error: No space left" }).text()).toContain(
      "Paused: disk error: No space left",
    );
    expect(mountRow({ state: "failed", error: "one or more files failed" }).text()).toContain(
      "Failed: one or more files failed",
    );
  });

  it("disables moving past either end of the queue", async () => {
    const first = mount(PackageRow, { props: { pkg: pkg(), index: 0, count: 3 } });
    await first.find('button[aria-label^="Expand"]').trigger("click");
    expect(first.find('button[aria-label="Move Movie pack up"]').attributes("disabled")).toBeDefined();
    const last = mount(PackageRow, { props: { pkg: pkg(), index: 2, count: 3 } });
    await last.find('button[aria-label^="Expand"]').trigger("click");
    expect(last.find('button[aria-label="Move Movie pack down"]').attributes("disabled")).toBeDefined();
  });

  it("confirms before removing, and can delete the files too", async () => {
    const wrapper = mountRow();
    await wrapper.find('button[aria-label="Remove Movie pack"]').trigger("click");
    expect(api.remove).not.toHaveBeenCalled();
    await wrapper.find("[role=dialog] input[type=checkbox]").setValue(true);
    await wrapper.find("[role=dialog] .btn.danger").trigger("click");
    await flushPromises();
    expect(api.remove).toHaveBeenCalledWith(1, true);
  });

  it("shows the two extraction options only for archives, and wires them up", async () => {
    const plain = mountRow();
    await plain.find('button[aria-label^="Expand"]').trigger("click");
    expect(plain.text()).not.toContain("Extract after download");

    const archive = mountRow({ is_archive: true, extract: true });
    await archive.find('button[aria-label^="Expand"]').trigger("click");
    const boxes = archive.findAll(".options input[type=checkbox]");
    expect(boxes).toHaveLength(2);
    await boxes[1].setValue(true);
    await flushPromises();
    expect(api.setPostOptions).toHaveBeenCalledWith(1, true, true);
  });

  it("rejects an unreadable speed limit without calling the engine", async () => {
    const wrapper = mountRow();
    await wrapper.find('button[aria-label^="Expand"]').trigger("click");
    const input = wrapper.find("input.limit");
    await input.setValue("fast");
    await input.trigger("blur");
    await flushPromises();
    expect(api.setPackageLimit).not.toHaveBeenCalled();
  });

  it("offers a restart for a failed file", async () => {
    const failed = pkg({
      files: [
        { id: 11, name: "bad.bin", state: "failed", error: "gone", size: 5, downloaded: 0, blocks: 0 },
      ],
    });
    const wrapper = mount(PackageRow, { props: { pkg: failed, index: 0, count: 1 } });
    await wrapper.find('button[aria-label^="Expand"]').trigger("click");
    await wrapper.find('button[aria-label="Restart bad.bin"]').trigger("click");
    expect(api.restartFile).toHaveBeenCalledWith(11);
  });
});
