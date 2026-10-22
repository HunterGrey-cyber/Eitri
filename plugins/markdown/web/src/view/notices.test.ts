import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import {
  blockedText, BUFFER_CLOSED_TEXT, EDITOR_CLOSED_TEXT, EMPTY_TEXT, FIND_MESSAGES, HUD, statusText, tooLargeText,
} from "./notices";

// A path, not `new URL(..., import.meta.url)`: vite rewrites that pattern into a dev-server address.
const FIXTURES = resolve(dirname(fileURLToPath(import.meta.url)), "../../../../../core/src/plugin/fixtures");
const fixture = (name: string) => JSON.parse(readFileSync(resolve(FIXTURES, name), "utf8"));

describe("notices", () => {
  it("say the fixed texts", () => {
    expect(EMPTY_TEXT).toBe("Open a Markdown file in the editor.");
    expect(BUFFER_CLOSED_TEXT).toBe("The buffer was closed.");
    expect(EDITOR_CLOSED_TEXT).toBe("The editor is closed.");
  });
  it("give both sizes of a document that is too large, as the shared fixture shows them", () => {
    expect(tooLargeText(fixture("ev-editor-text-too-large.json").data.too_large)).toBe(
      "This document is too large to preview (5.2 MB, 61,000 lines; the limit is 4 MB or 50,000 lines)");
    expect(tooLargeText({ bytes: 524288, lines: 999, max_bytes: 1048576, max_lines: 1234567 })).toBe(
      "This document is too large to preview (0.5 MB, 999 lines; the limit is 1 MB or 1,234,567 lines)");
  });
  it("name each reason an image was not loaded", () => {
    expect(blockedText("http")).toBe("http image not loaded");
    expect(blockedText("remote_off")).toBe("remote images are off");
    expect(blockedText("outside")).toBe("outside the project");
    expect(blockedText("unsaved")).toBe("save the file to show local images");
  });
  it("build the status from the file name, whether it is current, and the images not loaded", () => {
    expect(statusText({ name: "/home/u/proj/README.md", current: true, blocked: 0 })).toBe("README.md");
    expect(statusText({ name: "/home/u/proj/README.md", current: false, blocked: 0 })).toBe("README.md [not current]");
    expect(statusText({ name: "/a/b.md", current: true, blocked: 1 })).toBe("b.md · 1 image not loaded");
    expect(statusText({ name: "/a/b.md", current: false, blocked: 2 })).toBe("b.md [not current] · 2 images not loaded");
    expect(statusText({ name: "", current: true, blocked: 0 })).toBe("[No Name]");
  });
  it("hold the HUD's messages", () => {
    expect(HUD.notOpen).toBe("the file is not open in a window");
    expect(HUD.outside).toBe("outside the project");
    expect(HUD.noSuchFile).toBe("no such file");
    expect(HUD.wrapBottom).toBe("search hit BOTTOM, continuing at TOP");
    expect(HUD.wrapTop).toBe("search hit TOP, continuing at BOTTOM");
    expect(HUD.yanked(1)).toBe("yanked 1 line");
    expect(HUD.yanked(3)).toBe("yanked 3 lines");
    expect(FIND_MESSAGES).toEqual({ "find.wrapBottom": HUD.wrapBottom, "find.wrapTop": HUD.wrapTop, "find.count":
      "[{index}/{total}]" });
  });
});
