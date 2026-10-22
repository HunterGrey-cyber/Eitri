// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { forgetStatus, setDimmed, setModeWords, showMessage, showMode, showStatus, showWhichKey } from "./hud";
import { STYLE_ID, STYLE_TEXT, ensureStyle } from "./style";

const COLOUR_ONLY = ["background", "background-color", "color", "border-color", "outline-color", "font-family"];
const THEMED_PROPS = new Set([...COLOUR_ONLY, "border", "outline", "font"]);
const THEMED = /var\(--nv-[a-z0-9-]+, [^()]+\)/;
const THEMED_ALL = /var\(--nv-[a-z0-9-]+, [^()]+\)/g;

function hud() { return document.getElementById("eitri-keys-hud")!; }
function part(name: string) { return hud().querySelector<HTMLElement>(`.eitri-keys-${name}`)!; }

afterEach(() => {
  setModeWords(undefined);
  showMode("normal", "");
  showMessage("");
  showWhichKey("");
  forgetStatus();
  setDimmed(false);
  document.body.replaceChildren();
  document.head.replaceChildren();
});

describe("the kit stylesheet", () => {
  it("writes every colour and font as a theme variable with a literal fallback", () => {
    const declarations = STYLE_TEXT.split("}").filter(Boolean)
      .flatMap((rule) => rule.slice(rule.indexOf("{") + 1).split(";").filter(Boolean))
      .map((d) => [d.slice(0, d.indexOf(":")), d.slice(d.indexOf(":") + 1)] as const)
      .filter(([prop]) => THEMED_PROPS.has(prop));
    expect(declarations.length).toBeGreaterThanOrEqual(20);
    for (const [prop, value] of declarations) {
      expect(value, prop).toMatch(THEMED);
      const rest = value.replace(THEMED_ALL, "").trim();
      expect(rest, `${prop}:${value}`).not.toContain("#");
      if (COLOUR_ONLY.includes(prop)) expect(rest, `${prop}:${value}`).toBe("");
      else expect(rest, `${prop}:${value}`).toMatch(/^[0-9a-z ./]*$/);
    }
  });

  it("is added to the document once", () => {
    ensureStyle();
    ensureStyle();
    showMode("normal", "");
    expect(document.querySelectorAll(`#${STYLE_ID}`)).toHaveLength(1);
    expect(document.getElementById(STYLE_ID)!.textContent).toBe(STYLE_TEXT);
  });
});

describe("the HUD", () => {
  it("the_hud_shows_no_mode_word_without_the_keys", () => {
    for (const mode of ["normal", "hint", "insert"] as const) {
      showMode(mode, "");
      setDimmed(true);
      expect(hud().hidden).toBe(true);
      setDimmed(false);
      expect(hud().hidden).toBe(false);
      expect(part("mode").textContent).toBe(mode.toUpperCase());
    }
    expect(STYLE_TEXT).toContain("#eitri-keys-hud[hidden]{display:none}");
  });

  it("names the modes in English by default", () => {
    showMode("normal", "");
    expect(part("mode").textContent).toBe("NORMAL");
    showMode("hint", "");
    expect(part("mode").textContent).toBe("HINT");
    showMode("insert", "");
    expect(part("mode").textContent).toBe("INSERT");
  });

  it("takes the page's mode words, keeping the defaults it leaves out", () => {
    setModeWords({ normal: "普通", insert: "输入" });
    showMode("normal", "");
    expect(part("mode").textContent).toBe("普通");
    showMode("hint", "");
    expect(part("mode").textContent).toBe("HINT");
    showMode("insert", "");
    expect(part("mode").textContent).toBe("输入");
  });

  it("marks the mode block with the mode", () => {
    for (const mode of ["normal", "hint", "insert"] as const) {
      showMode(mode, "");
      expect(part("mode").dataset.mode).toBe(mode);
      expect(hud().dataset.mode).toBe(mode);
    }
  });

  it("draws the pending keys, the message and the status in their own segments", () => {
    showMode("normal", "]");
    showMessage("saved");
    showStatus("item 3/8");
    expect(part("pending").textContent).toBe("]");
    expect(part("message").textContent).toBe("saved");
    expect(part("status").textContent).toBe("item 3/8");
    expect([...hud().children].map((c) => c.className))
      .toEqual(["eitri-keys-mode", "eitri-keys-pending", "eitri-keys-whichkey", "eitri-keys-message", "eitri-keys-status"]);
    expect(hud().getAttribute("role")).toBe("status");
  });

  it("shows the which-key line in place of the message, and the message again when it goes", () => {
    showMode("normal", "z");
    showMessage("saved");
    showWhichKey("z → a 折叠 · R 全展开");
    expect(part("whichkey").textContent).toBe("z → a 折叠 · R 全展开");
    expect(part("message").hidden).toBe(true);
    expect(part("message").textContent).toBe("saved");
    showWhichKey("");
    expect(part("whichkey").textContent).toBe("");
    expect(part("message").hidden).toBe(false);
    expect(part("message").textContent).toBe("saved");
  });

  it("draws the which-key line as text", () => {
    showWhichKey('z → a <img src=x onerror="alert(1)">');
    expect(hud().querySelector("img")).toBeNull();
    expect(part("whichkey").textContent).toBe('z → a <img src=x onerror="alert(1)">');
  });

  it("keeps the status across messages, mode changes and a dim, until it is replaced or cleared", () => {
    showStatus("item 3/8");
    showMessage("right");
    showMode("hint", "f");
    showMessage("");
    expect(part("status").textContent).toBe("item 3/8");
    setDimmed(true);
    expect(hud().hidden).toBe(true);
    expect(part("status").textContent).toBe("item 3/8");
    setDimmed(false);
    expect(hud().hidden).toBe(false);
    expect(part("status").textContent).toBe("item 3/8");
    showStatus("item 4/8");
    expect(part("status").textContent).toBe("item 4/8");
    showStatus("");
    expect(part("status").textContent).toBe("");
  });

  it("draws page text as text and colours nothing inline", () => {
    setModeWords({ normal: "<b>N</b>" });
    showMode("normal", "");
    showStatus('<img src=x onerror="alert(1)">');
    showMessage("<i>m</i>");
    expect(hud().querySelector("b, img, i")).toBeNull();
    expect(part("status").textContent).toBe('<img src=x onerror="alert(1)">');
    for (const el of [hud(), ...hud().querySelectorAll<HTMLElement>("*")]) expect(el.getAttribute("style")).toBeNull();
  });
});

describe("rendering", () => {
  it("render writes no DOM when nothing it shows changed", () => {
    showMode("normal", "g");
    showMessage("saved");
    const span = part("mode");
    const set = vi.fn();
    const textContent = Object.getOwnPropertyDescriptor(Node.prototype, "textContent")!;
    Object.defineProperty(span, "textContent", {
      configurable: true,
      get: () => textContent.get!.call(span),
      set: (value: string) => { set(value); textContent.set!.call(span, value); },
    });
    showMode("normal", "g");
    showMessage("saved");
    showStatus("");
    setDimmed(false);
    expect(set).not.toHaveBeenCalled();
    showMode("hint", "g");
    expect(set).toHaveBeenCalledWith("HINT");
    expect(span.textContent).toBe("HINT");
  });
});
