// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { lookOnScreen } from "./lookOnScreen";

/** jsdom lays nothing out: every character of the band's one text node is 10 px wide on one 10 px line, and the
 *  hit test answers from `cover` (what lies over a point, if anything), else the flash itself. */
const TEXT = "https://github.com-sign-in.example.net/ — click again to open";
const HOST = "github.com-sign-in.example.net";
let cover: ((x: number, y: number) => Element | null) | null;
let flash: HTMLElement;
const proto = Range.prototype as unknown as { getClientRects?: () => DOMRect[] };
const savedRects = proto.getClientRects;
const doc = document as unknown as { elementFromPoint?: (x: number, y: number) => Element | null };
const savedHit = doc.elementFromPoint;
const rect = (left: number, right: number, top = 0, bottom = 10) =>
  ({ left, right, top, bottom, width: right - left, height: bottom - top, x: left, y: top }) as DOMRect;

beforeEach(() => {
  document.body.innerHTML = `<div class="status-band-above"><div class="band-above-line band-message">${TEXT}</div></div><div class="other"></div>`;
  flash = document.querySelector<HTMLElement>(".band-message")!;
  cover = null;
  proto.getClientRects = function (this: Range) {
    return [rect(this.startOffset * 10, this.endOffset * 10)];
  };
  doc.elementFromPoint = (x: number, y: number) => cover?.(x, y) ?? flash.firstChild?.parentElement ?? null;
});
afterEach(() => {
  proto.getClientRects = savedRects;
  doc.elementFromPoint = savedHit;
  document.body.innerHTML = "";
});

const hostEnd = (TEXT.indexOf(HOST) + HOST.length) * 10;

describe("lookOnScreen", () => {
  it("sees a host drawn whole with nothing over it", () => {
    expect(lookOnScreen(flash, HOST)).toBe(true);
  });

  it("refuses a host whose domain's end a neighbour covers, its middle still showing", () => {
    const other = document.querySelector(".other")!;
    cover = (x) => (x >= hostEnd - 25 && x <= hostEnd ? other : null); // the last two and a half characters
    expect(lookOnScreen(flash, HOST)).toBe(false);
  });

  it("refuses a host only its last character of is covered", () => {
    const other = document.querySelector(".other")!;
    cover = (x) => (x > hostEnd - 10 && x < hostEnd ? other : null);
    expect(lookOnScreen(flash, HOST)).toBe(false);
  });

  it("refuses a host under a layer a hit test looks through (pointer-events: none)", () => {
    const layer = document.createElement("div");
    layer.className = "hint-layer";
    layer.getBoundingClientRect = () => rect(hostEnd - 40, hostEnd + 40);
    document.body.appendChild(layer);
    expect(lookOnScreen(flash, HOST)).toBe(false);
    // Clear of the host, or not shown, it covers nothing.
    layer.getBoundingClientRect = () => rect(0, 100, 50, 80);
    expect(lookOnScreen(flash, HOST)).toBe(true);
    layer.getBoundingClientRect = () => rect(hostEnd - 40, hostEnd + 40);
    layer.style.display = "none";
    expect(lookOnScreen(flash, HOST)).toBe(true);
  });

  it("refuses a host past the viewport or with no boxes", () => {
    proto.getClientRects = function (this: Range) {
      return [rect(this.startOffset * 10 - 5000, this.endOffset * 10 - 5000)];
    };
    expect(lookOnScreen(flash, HOST)).toBe(false);
    proto.getClientRects = () => [];
    expect(lookOnScreen(flash, HOST)).toBe(false);
  });

  it("refuses a host the flash does not hold", () => {
    expect(lookOnScreen(flash, "evil.example")).toBe(false);
  });
});
