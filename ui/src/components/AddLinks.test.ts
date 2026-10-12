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
      { source: "https://x/a.part1.rar", name: "a.part1.rar", size: 10, error: null, found_on: null },
      { source: "https://x/a.part2.rar", name: "a.part2.rar", size: 10, error: null, found_on: null },
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
      files: null,
    });
    expect(wrapper.emitted("close")).toBeTruthy();
  });

  it("scans pages by default and passes the deep option only with it", async () => {
    api.collect.mockResolvedValue([draft()]);
    const wrapper = setup();
    const [pages, deep] = wrapper.findAll(".options input[type=checkbox]");
    expect((pages.element as HTMLInputElement).checked).toBe(true);
    await deep.setValue(true);
    await wrapper.find("button.primary").trigger("click");
    await flushPromises();
    expect(api.collect).toHaveBeenLastCalledWith(expect.any(Array), { scan_pages: true, deep: true });

    const again = setup();
    const [pages2] = again.findAll(".options input[type=checkbox]");
    await pages2.setValue(false);
    expect(again.findAll(".options input[type=checkbox]")[1].attributes("disabled")).toBeDefined();
    await again.find("button.primary").trigger("click");
    await flushPromises();
    expect(api.collect).toHaveBeenLastCalledWith(expect.any(Array), { scan_pages: false, deep: false });
  });

  it("adds only the ticked files and shows where they were found", async () => {
    const page = "https://site.example/album/42";
    api.collect.mockResolvedValue([
      draft({
        id: 9,
        name: "Album",
        files: [
          { source: "https://cdn/1.jpg", name: "1.jpg", size: 5, error: null, found_on: page },
          { source: "https://cdn/2.jpg", name: "2.jpg", size: 5, error: null, found_on: page },
          { source: "https://cdn/3.jpg", name: null, size: null, error: "gone", found_on: page },
        ],
        ok_count: 2,
      }),
    ]);
    const wrapper = setup();
    await wrapper.find("button.primary").trigger("click");
    await flushPromises();
    expect(wrapper.text()).toContain("2 of 3 files");
    expect(wrapper.text()).toContain("found on site.example/album/42");

    const files = wrapper.findAll(".files input[type=checkbox]");
    expect((files[2].element as HTMLInputElement).disabled).toBe(true);
    await files[1].setValue(false);
    const all = wrapper.find(".head input[type=checkbox]").element as HTMLInputElement;
    expect(all.indeterminate).toBe(true);
    expect(wrapper.findAll("footer button").at(-1)?.text()).toBe("Add 1 file");

    await wrapper.findAll("footer button").at(-1)?.trigger("click");
    await flushPromises();
    expect(api.enqueue).toHaveBeenCalledWith(
      expect.objectContaining({ draft_id: 9, files: ["https://cdn/1.jpg"] }),
    );
  });

  it("picks files by type, with counts and a mixed state", async () => {
    const at = "https://site.example/a";
    const file = (name: string) => ({
      source: `https://cdn/${name}`,
      name,
      size: 1,
      error: null,
      found_on: at,
    });
    api.collect.mockResolvedValue([
      draft({
        id: 3,
        name: "Album",
        files: [file("1.mp4"), file("2.mp4"), file("t1.jpg"), file("t2.jpg"), file("t3.jpg")],
        ok_count: 5,
      }),
    ]);
    const wrapper = setup();
    await wrapper.find("button.primary").trigger("click");
    await flushPromises();

    const chips = wrapper.findAll(".chip");
    expect(chips.map((c) => c.text())).toEqual(["Videos 2", "Images 3"]);
    expect(chips.every((c) => c.attributes("aria-pressed") === "true")).toBe(true);

    // One click unticks every image; the videos stay.
    await chips[1].trigger("click");
    expect(chips[1].attributes("aria-pressed")).toBe("false");
    expect(chips[0].attributes("aria-pressed")).toBe("true");
    expect(wrapper.findAll("footer button").at(-1)?.text()).toBe("Add 2 files");

    // Unticking one video makes the Videos pill mixed.
    await wrapper.findAll(".files input[type=checkbox]")[0].setValue(false);
    expect(chips[0].attributes("aria-pressed")).toBe("mixed");

    // "only images" ticks the images and unticks the videos.
    await wrapper.findAll(".only")[1].trigger("click");
    expect(chips[0].attributes("aria-pressed")).toBe("false");
    expect(chips[1].attributes("aria-pressed")).toBe("true");
    expect(wrapper.findAll("footer button").at(-1)?.text()).toBe("Add 3 files");

    await wrapper.findAll("footer button").at(-1)?.trigger("click");
    await flushPromises();
    expect(api.enqueue).toHaveBeenCalledWith(
      expect.objectContaining({
        files: ["https://cdn/t1.jpg", "https://cdn/t2.jpg", "https://cdn/t3.jpg"],
      }),
    );
  });

  it("shows no type filter when everything is one kind", async () => {
    api.collect.mockResolvedValue([draft()]);
    const wrapper = setup();
    await wrapper.find("button.primary").trigger("click");
    await flushPromises();
    expect(wrapper.find(".kinds").exists()).toBe(false);
  });

  it("warns when part of an archive is unticked", async () => {
    api.collect.mockResolvedValue([archive()]);
    const wrapper = setup();
    await wrapper.find("button.primary").trigger("click");
    await flushPromises();
    expect(wrapper.text()).not.toContain("can’t be extracted");
    await wrapper.findAll(".files input[type=checkbox]")[1].setValue(false);
    expect(wrapper.text()).toContain("can’t be extracted");
  });

  it("warns about missing volumes and skips a draft with nothing usable", async () => {
    api.collect.mockResolvedValue([
      draft({
        id: 2,
        name: "broken",
        ok_count: 0,
        missing_volumes: [2, 3],
        files: [{ source: "https://x/b.part1.rar", name: null, size: null, error: "404", found_on: null }],
      }),
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
