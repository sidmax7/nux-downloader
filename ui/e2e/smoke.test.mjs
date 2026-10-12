// End-to-end smoke test: starts the built app through `tauri-driver` (a
// WebDriver server) and checks that the window renders, the tabs work and
// the add dialog opens. Uses only Node's built-in test runner and fetch.
//
//   tauri-driver &                       # listens on :4444
//   NUXD_APP=../target/release/nux-downloader node --test e2e/
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { after, before, describe, it } from "node:test";

const DRIVER = process.env.TAURI_DRIVER_URL ?? "http://127.0.0.1:4444";
const APP = path.resolve(process.env.NUXD_APP ?? "../target/release/nux-downloader");

let session = "";
let dataDir = "";

async function call(method, route, body) {
  const response = await fetch(`${DRIVER}${route}`, {
    method,
    headers: { "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const json = await response.json();
  if (!response.ok) throw new Error(`${method} ${route}: ${JSON.stringify(json)}`);
  return json.value;
}

const find = async (using, value) => {
  const element = await call("POST", `/session/${session}/element`, { using, value });
  return Object.values(element)[0];
};

const text = (id) => call("GET", `/session/${session}/element/${id}/text`);
const click = (id) => call("POST", `/session/${session}/element/${id}/click`, {});

/** Waits until `check` stops throwing. */
async function eventually(check, what, ms = 15000) {
  const end = Date.now() + ms;
  for (;;) {
    try {
      return await check();
    } catch (error) {
      if (Date.now() > end) throw new Error(`timed out waiting for ${what}: ${error.message}`);
      await new Promise((resolve) => setTimeout(resolve, 250));
    }
  }
}

const button = (label) => find("xpath", `//button[normalize-space()='${label}']`);

describe("app", () => {
  before(async () => {
    dataDir = mkdtempSync(path.join(tmpdir(), "nuxd-e2e-"));
    const created = await call("POST", "/session", {
      capabilities: {
        alwaysMatch: {
          browserName: "wry",
          "tauri:options": { application: APP, args: [], env: { NUXD_DATA_DIR: dataDir } },
        },
      },
    });
    session = created.sessionId;
  });

  after(async () => {
    if (session) await call("DELETE", `/session/${session}`).catch(() => undefined);
    if (dataDir) rmSync(dataDir, { recursive: true, force: true });
  });

  it("renders the header and the empty queue", async () => {
    assert.equal(await call("GET", `/session/${session}/title`), "Nux Downloader");
    const heading = await eventually(() => find("css selector", "h1"), "the heading");
    assert.equal(await text(heading), "Downloads");
    const empty = await eventually(() => find("css selector", ".empty"), "the empty queue");
    assert.match(await text(empty), /Nothing downloading/);
  });

  it("switches between tabs", async () => {
    await click(await button("Settings"));
    await eventually(() => find("css selector", "form[aria-label='Settings']"), "the settings form");
    await click(await button("Links"));
    await eventually(() => find("css selector", "section[aria-label='Network links']"), "the links panel");
    await click(await button("Downloads"));
    await eventually(() => find("css selector", "section[aria-label='Downloads']"), "the downloads list");
  });

  it("opens and closes the add dialog", async () => {
    await click(await button("Add links"));
    // The dialog fades in from opacity 0, and WebDriver reports no text
    // for an element it can't see, so wait for the text, not the element.
    await eventually(async () => {
      const dialog = await find("css selector", "[role='dialog']");
      assert.match(await text(dialog), /Paste one or more links/);
    }, "the add dialog");
    await click(await button("Cancel"));
    await eventually(async () => {
      try {
        await find("css selector", "[role='dialog']");
      } catch {
        return;
      }
      throw new Error("the dialog is still open");
    }, "the dialog to close");
  });
});
