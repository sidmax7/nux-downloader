import { describe, expect, it } from "vitest";
import { linkColor } from "./colors";

describe("linkColor", () => {
  it("uses the user's colour when it is valid", () => {
    expect(linkColor("wlan0", "#33aa77")).toBe("#33aa77");
  });

  it("ignores an invalid choice", () => {
    expect(linkColor("wlan0", "red")).toBe(linkColor("wlan0", null));
  });

  it("is stable for a name and spreads names around", () => {
    expect(linkColor("eth0", null)).toBe(linkColor("eth0", null));
    const colors = new Set(["eth0", "wlan0", "usb0", "en6", "bridge0"].map((n) => linkColor(n, null)));
    expect(colors.size).toBeGreaterThan(2);
  });
});
