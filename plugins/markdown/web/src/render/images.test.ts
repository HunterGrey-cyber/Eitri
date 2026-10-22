import { describe, expect, it } from "vitest";
import { boxHtml, resolveImage } from "./images";

const doc = { root: "eitri-plugin://markdown/.eitri/doc/4/", base: "eitri-plugin://markdown/.eitri/doc/4/docs/" };

describe("resolveImage", () => {
  it("serves a relative path from the document's directory and a leading / from the project", () => {
    expect(resolveImage("x.png", doc, "https")).toEqual({ url: `${doc.base}x.png` });
    expect(resolveImage("../assets/x.png", doc, "https")).toEqual({ url: `${doc.root}assets/x.png` });
    expect(resolveImage("/assets/x.png", doc, "https")).toEqual({ url: `${doc.root}assets/x.png` });
    expect(resolveImage("a%20b.png", doc, "https")).toEqual({ url: `${doc.base}a%20b.png` });
  });
  it("boxes anything that leaves the project", () => {
    for (const src of ["../../x.png", "%2e%2e/%2e%2e/x.png", "a/../../../x.png", "/../x.png", "..\\..\\x.png",
      "file:///etc/passwd", "~/x.png", "C:\\x.png", "//example.org/x.png", "ftp://example.org/x.png", "javascript:x", ""]) {
      expect(resolveImage(src, doc, "https"), src).toEqual({ blocked: "outside" });
    }
  });
  it("loads https unless remote images are off, never http, and keeps data images", () => {
    expect(resolveImage("https://example.org/b.svg", doc, "https")).toEqual({ url: "https://example.org/b.svg" });
    expect(resolveImage("HTTPS://example.org/b.svg", null, "https")).toEqual({ url: "HTTPS://example.org/b.svg" });
    expect(resolveImage("https://example.org/b.svg", doc, "off")).toEqual({ blocked: "remote_off" });
    expect(resolveImage("http://example.org/b.png", doc, "https")).toEqual({ blocked: "http" });
    expect(resolveImage("data:image/png;base64,AAAA", doc, "off")).toEqual({ url: "data:image/png;base64,AAAA" });
    expect(resolveImage("data:text/html,x", doc, "https")).toEqual({ blocked: "outside" });
  });
  it("asks for a saved file before showing a local image of an unnamed buffer", () => {
    expect(resolveImage("x.png", null, "https")).toEqual({ blocked: "unsaved" });
    expect(resolveImage("/x.png", null, "https")).toEqual({ blocked: "unsaved" });
  });
});

describe("boxHtml", () => {
  it("draws the alt text and the reason, escaped and renderer-marked", () => {
    const host = document.createElement("div");
    host.innerHTML = boxHtml('a <b>"x"</b>', "http", "n1");
    const box = host.firstElementChild!;
    expect([box.className, box.getAttribute("data-eitri-blocked"), box.getAttribute("data-eitri-n")])
      .toEqual(["mdp-box", "http", "n1"]);
    expect(box.querySelector(".mdp-box-reason")!.textContent).toBe("http image not loaded");
    expect(box.textContent).toBe('http image not loaded a <b>"x"</b>');
    expect(box.querySelector("b")).toBeNull();
  });
});
