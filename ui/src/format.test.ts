import { describe, expect, it } from "vitest";
import {
  etaSeconds,
  formatBytes,
  formatEta,
  formatSpeed,
  parseRate,
  percent,
  rateText,
  splitLinks,
} from "./format";

describe("formatBytes", () => {
  it("scales units", () => {
    expect(formatBytes(0)).toBe("0 B");
    expect(formatBytes(1023)).toBe("1023 B");
    expect(formatBytes(1536)).toBe("1.50 KiB");
    expect(formatBytes(5 * 1024 ** 2)).toBe("5.00 MiB");
    expect(formatBytes(12.5 * 1024 ** 3)).toBe("12.5 GiB");
    expect(formatBytes(250 * 1024 ** 3)).toBe("250 GiB");
  });

  it("does not invent units past the largest", () => {
    expect(formatBytes(3 * 1024 ** 6)).toBe("3072 PiB");
  });

  it("shows a dash for nonsense", () => {
    expect(formatBytes(-1)).toBe("–");
    expect(formatBytes(Number.NaN)).toBe("–");
  });

  it("formats speeds", () => {
    expect(formatSpeed(2 * 1024 ** 2)).toBe("2.00 MiB/s");
  });
});

describe("formatEta", () => {
  it("picks a readable unit", () => {
    expect(formatEta(9)).toBe("9s");
    expect(formatEta(75)).toBe("1m 15s");
    expect(formatEta(3725)).toBe("1h 2m");
    expect(formatEta(3 * 86400 + 3600)).toBe("3d 1h");
  });

  it("is a dash when unknown", () => {
    expect(formatEta(null)).toBe("–");
    expect(formatEta(Number.POSITIVE_INFINITY)).toBe("–");
  });
});

describe("progress", () => {
  it("computes eta only with a size and a speed", () => {
    expect(etaSeconds(1000, 500, 50)).toBe(10);
    expect(etaSeconds(null, 500, 50)).toBeNull();
    expect(etaSeconds(1000, 500, 0)).toBeNull();
    expect(etaSeconds(1000, 1500, 50)).toBe(0);
  });

  it("computes percent", () => {
    expect(percent(200, 50)).toBe(25);
    expect(percent(200, 999)).toBe(100);
    expect(percent(0, 0)).toBe(100);
    expect(percent(null, 5)).toBeNull();
  });
});

describe("parseRate", () => {
  it("reads sizes with binary units", () => {
    expect(parseRate("500K")).toBe(500 * 1024);
    expect(parseRate("5M")).toBe(5 * 1024 ** 2);
    expect(parseRate("1.5g")).toBe(1.5 * 1024 ** 3);
    expect(parseRate("5 MB/s")).toBe(5 * 1024 ** 2);
    expect(parseRate("1000")).toBe(1000);
  });

  it("treats empty and 'off' words as no limit", () => {
    for (const text of ["", "  ", "0", "none", "Off", "unlimited"]) {
      expect(parseRate(text)).toBeNull();
    }
  });

  it("rejects garbage", () => {
    for (const text of ["fast", "-5M", "5X", "1..2M", "M5"]) {
      expect(parseRate(text)).toBeUndefined();
    }
  });

  it("round-trips through rateText", () => {
    for (const bytes of [512, 1024, 1536, 5 * 1024 ** 2, 2 * 1024 ** 3]) {
      expect(parseRate(rateText(bytes))).toBe(bytes);
    }
    expect(rateText(null)).toBe("");
  });
});

describe("splitLinks", () => {
  it("splits on whitespace and drops duplicates", () => {
    const text = " https://a/1 \n\nhttps://a/2\thttps://a/1\r\nhttps://a/3 ";
    expect(splitLinks(text)).toEqual(["https://a/1", "https://a/2", "https://a/3"]);
  });

  it("is empty for blank text", () => {
    expect(splitLinks("  \n ")).toEqual([]);
  });
});
