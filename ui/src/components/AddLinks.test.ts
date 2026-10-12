import { flushPromises, mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { draft } from "../testing";

const api = vi.hoisted(() => ({
  collect: vi.fn(),
  enqueue: vi.fn(),
  discardDrafts: vi.fn(),
  downloadPath: vi.fn(),
  pickFolder: vi.fn(),
  getSettings: vi.fn(),
}));
vi.mock("../api", () => ({ api }));

import AddLinks from "./AddLinks.vue";

function setup() {
  return mount(AddLinks, { props: { initialText: "https://x/a.part1.rar\nhttps://x/a.part2.rar" } });
}

beforeEach(() => {
  setActivePinia(createPinia());
  vi.resetAllMocks();
  api.downloadPath.mockResolvedValue("/home/me/Downloads");
  api.discardDrafts.mockResolvedValue(null);
  api.enqueue.mockResolvedValue(7);
});

const archive = () =>
  draft({
    id: 5,
    name: "a",
    offers_extract: true,
    files: [
      { source: "https://x/a.part1.rar", name: "a.part1.rar", size: 10, error: null },
      { source: "https://x/a.part2.rar", name: "a.part2.rar", size: 10, error: null },
    ],
  });

describe("AddLinks", () => {
  it("counts the pasted links", () => {
    const wrapper = setup();
    expect(wrapper.text()).toContain("2 links");
  });

  it("always offers the two archive checkboxes for an archive", async () => {
    api.collect.mockResolvedValue([archive()]);
    const wrapper = setup();
    await wrapper.find("button.primary").trigger("click");
    await flushPromises();
    const text = wrapper.text();
    expect(text).toContain("Extract after download");
    expect(text).toContain("Delete archive after extraction");
  });

  it("offers neither for a plain file", async () => {
    api.collect.mockResolvedValue([draft()]);
    const wrapper = setup();
    await wrapper.find("button.primary").trigger("click");
    await flushPromises();
    expect(wrapper.text()).not.toContain("Extract after download");
  });

  it("keeps delete-after-extraction off until extraction is ticked", async () => {
    api.collect.mockResolvedValue([archive()]);
    const wrapper = setup();
    await wrapper.find("button.primary").trigger("click");
    await flushPromises();
    const boxes = wrapper.findAll(".post input[type=checkbox]");
    expect(boxes).toHaveLength(2);
    // settings are not loaded in this test, so extraction starts ticked
    expect((boxes[0].element as HTMLInputElement).checked).toBe(true);
    expect((boxes[1].element as HTMLInputElement).disabled).toBe(false);
    await boxes[0].setValue(false);
    expect((boxes[1].element as HTMLInputElement).disabled).toBe(true);
  });

  it("sends the chosen options when adding", async () => {
    api.collect.mockResolvedValue([archive()]);
    const wrapper = setup();
    await wrapper.find("button.primary").trigger("click");
    await flushPromises();
    await wrapper.findAll(".post input[type=checkbox]")[1].setValue(true);
    const buttons = wrapper.findAll("footer button");
    await buttons[buttons.length - 1].trigger("click");
    await flushPromises();
    expect(api.enqueue).toHaveBeenCalledWith({
      draft_id: 5,
      name: "a",
      dest_dir: "/home/me/Downloads",
      priority: "normal",
      extract: true,
      delete_archives: true,
    });
    expect(wrapper.emitted("close")).toBeTruthy();
  });

  it("warns about missing volumes and skips a draft with nothing usable", async () => {
    api.collect.mockResolvedValue([
      draft({ id: 2, name: "broken", ok_count: 0, missing_volumes: [2, 3] }),
    ]);
    const wrapper = setup();
    await wrapper.find("button.primary").trigger("click");
    await flushPromises();
    expect(wrapper.text()).toContain("Missing volumes: 2, 3");
    const add = wrapper.findAll("footer button").at(-1);
    expect(add?.attributes("disabled")).toBeDefined();
  });

  it("discards the drafts on cancel", async () => {
    const wrapper = setup();
    await wrapper.findAll("footer button")[0].trigger("click");
    await flushPromises();
    expect(api.discardDrafts).toHaveBeenCalled();
    expect(wrapper.emitted("close")).toBeTruthy();
  });
});
