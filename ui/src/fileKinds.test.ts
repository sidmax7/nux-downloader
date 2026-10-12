import { describe, expect, it } from "vitest";
import { fileKind } from "./fileKinds";

describe("fileKind", () => {
  it("knows the common kinds, whatever the case", () => {
    expect(fileKind("clip.MP4")).toBe("video");
    expect(fileKind("a.b.mkv")).toBe("video");
    expect(fileKind("thumb.jpg")).toBe("image");
    expect(fileKind("song.flac")).toBe("audio");
    expect(fileKind("pack.part01.rar")).toBe("archive");
    expect(fileKind("book.pdf")).toBe("document");
  });

  it("treats split-archive volumes as archives", () => {
    expect(fileKind("data.7z.001")).toBe("archive");
    expect(fileKind("data.r00")).toBe("archive");
  });

  it("files the rest under other", () => {
    expect(fileKind("README")).toBe("other");
    expect(fileKind("setup.exe")).toBe("other");
    expect(fileKind("file.abc")).toBe("other");
  });
});
