import { describe, expect, it } from "vitest";
import { applyFontSize, TEXT_PROPERTY } from "./text-size";

describe("the page's base text size", () => {
  it("puts a font_size on the root element, where it outranks the style sheet, and takes it away again", () => {
    const root = document.createElement("html");
    expect(root.style.getPropertyValue(TEXT_PROPERTY)).toBe("");
    applyFontSize(root, 18);
    expect(root.style.getPropertyValue(TEXT_PROPERTY)).toBe("18px");
    applyFontSize(root, 11);
    expect(root.style.getPropertyValue(TEXT_PROPERTY)).toBe("11px");
    applyFontSize(root, null);
    expect(root.style.getPropertyValue(TEXT_PROPERTY)).toBe("");
  });

  it("leaves the properties it does not own alone", () => {
    const root = document.createElement("html");
    root.style.setProperty("--nv-text-size", "20px");
    root.style.setProperty("--mdp-reserve", "110px");
    applyFontSize(root, 16);
    applyFontSize(root, null);
    expect(root.style.getPropertyValue("--nv-text-size")).toBe("20px");
    expect(root.style.getPropertyValue("--mdp-reserve")).toBe("110px");
  });
});
