import { describe, expect, it } from "vitest";
import { anchorId, classifyHref, lineFragment, namesDocument, splitFragment } from "./links";

describe("classifyHref", () => {
  it("sorts a link by what following it does", () => {
    for (const [href, kind] of [
      ["https://example.org/x", "external"], ["HTTP://example.org", "external"], ["mailto:a@example.org", "external"],
      ["#setup", "anchor"], ["other.md", "local"], ["../docs/x.md#setup", "local"], ["/README.md", "local"],
      ["src/main.rs#L10", "local"], ["./a b.md", "local"],
      ["javascript:alert(1)", "none"], [" javascript:x", "none"], ["file:///etc/passwd", "none"], ["data:text/html,x", "none"],
      ["ftp://example.org", "none"], ["tel:123", "none"], ["C:\\x.md", "none"], ["//example.org/x", "none"],
      ["\\\\server\\share", "none"], ["", "none"], ["   ", "none"],
    ] as const) {
      expect(classifyHref(href), href).toBe(kind);
    }
  });
});

describe("fragments", () => {
  it("splits the fragment off at the first #", () => {
    expect(splitFragment("a.md#x#y")).toEqual({ path: "a.md", fragment: "x#y" });
    expect(splitFragment("a.md")).toEqual({ path: "a.md", fragment: null });
    expect(splitFragment("#top")).toEqual({ path: "", fragment: "top" });
  });
  it("reads a line fragment, a range by its first line", () => {
    expect(lineFragment("L10")).toBe(10);
    expect(lineFragment("L10-L20")).toBe(10);
    expect(lineFragment("L0")).toBeNull();
    expect(lineFragment("setup")).toBeNull();
    expect(lineFragment("L10C3")).toBeNull();
    expect(lineFragment(null)).toBeNull();
  });
  it("prefixes a heading id as GitHub does, decoded, once", () => {
    expect(anchorId("setup")).toBe("user-content-setup");
    expect(anchorId("caf%C3%A9")).toBe("user-content-café");
    expect(anchorId("user-content-setup")).toBe("user-content-setup");
    expect(anchorId("%E0%A4%A")).toBe("user-content-%E0%A4%A");
  });
});

describe("namesDocument", () => {
  it("says whether a relative link path, as written, is the shown document itself", () => {
    const doc = "/p/docs/notes.md";
    for (const path of ["notes.md", "./notes.md", "../docs/notes.md", ".//notes.md", "no%74es.md", "../../../p/docs/notes.md"]) {
      expect(namesDocument(path, doc), path).toBe(true);
    }
    for (const path of ["", "other.md", "Notes.md", "docs/notes.md", "/docs/notes.md", "%E0%A4%A", "../notes.md"]) {
      expect(namesDocument(path, doc), path).toBe(false);
    }
    expect(namesDocument("notes.md", "")).toBe(false);
  });
});
