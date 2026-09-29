// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import {
  charAfterCaret,
  compareCarets,
  copySelectionText,
  firstSelectableCaret,
  isChromeNode,
  isWhitespaceAfter,
  lastSelectableCaret,
  rebuildSelection,
  repeatMotion,
  revealCaret,
  runMotion,
  stepOnce,
  swapEnds,
  VISUAL_CHROME,
  type Caret,
  type CharMotion,
  type SelectionLike,
  type VisualModel,
} from "./visual";

/** A `SelectionLike` whose `modify()` follows a supplied caret path, one step per call,
 *  clamped at the last position, while recording calls. It does not implement word movement.
 *  It checks the steps requested by `visual.ts` and supplies DOM positions for exercising
 *  `stepOnce`/`repeatMotion` boundary, chrome and no-progress handling. */
function fakeSelection(path: Caret[]): SelectionLike & { calls: string[] } {
  let index = 0;
  let anchorNode: Node = path[0].node;
  let anchorOffset = path[0].offset;
  let focusNode: Node = path[0].node;
  let focusOffset = path[0].offset;
  const calls: string[] = [];
  return {
    calls,
    get anchorNode() {
      return anchorNode;
    },
    get anchorOffset() {
      return anchorOffset;
    },
    get focusNode() {
      return focusNode;
    },
    get focusOffset() {
      return focusOffset;
    },
    rangeCount: 0,
    getRangeAt(): Range {
      throw new Error("not used by these tests");
    },
    collapse(node, offset) {
      anchorNode = focusNode = node;
      anchorOffset = focusOffset = offset;
      const found = path.findIndex((p) => p.node === node && p.offset === offset);
      if (found !== -1) index = found;
    },
    setBaseAndExtent(an, ao, fn, fo) {
      anchorNode = an;
      anchorOffset = ao;
      focusNode = fn;
      focusOffset = fo;
    },
    modify(alter, direction, granularity) {
      calls.push(`${alter} ${direction} ${granularity}`);
      index = Math.min(index + 1, path.length - 1);
      focusNode = path[index].node;
      focusOffset = path[index].offset;
      if (alter === "move") {
        anchorNode = focusNode;
        anchorOffset = focusOffset;
      }
    },
    toString() {
      return "";
    },
  };
}

function textCaret(text: Text, offset: number): Caret {
  return { node: text, offset };
}

describe("visual.ts: whitespace and word motions", () => {
  it("isWhitespaceAfter reads the character right after the caret, not before it", () => {
    const text = document.createTextNode("ab cd");
    expect(isWhitespaceAfter(textCaret(text, 0))).toBe(false); // 'a'
    expect(isWhitespaceAfter(textCaret(text, 2))).toBe(true); // ' '
    expect(isWhitespaceAfter(textCaret(text, 5))).toBe(true); // past the end: treated as a boundary
  });

  it("charAfterCaret returns null past the end of a text node with no known next text", () => {
    const text = document.createTextNode("x");
    expect(charAfterCaret(textCaret(text, 1))).toBeNull();
  });

  it("w issues forward+backward word when on whitespace, forward+forward+backward otherwise", () => {
    const onWhitespace = document.createTextNode(" rest");
    const midWord = document.createTextNode("word rest");
    const path = [textCaret(onWhitespace, 0), textCaret(onWhitespace, 1), textCaret(onWhitespace, 2)];

    const sel1 = fakeSelection(path);
    runMotion(sel1, textCaret(onWhitespace, 0), "w", null);
    expect(sel1.calls).toEqual(["move forward word", "move backward word"]);

    const sel2 = fakeSelection([textCaret(midWord, 0), textCaret(midWord, 1), textCaret(midWord, 2), textCaret(midWord, 3)]);
    runMotion(sel2, textCaret(midWord, 0), "w", null);
    expect(sel2.calls).toEqual(["move forward word", "move forward word", "move backward word"]);
  });

  it("h/l/e/b/0/$ each issue exactly the expected modify() calls", () => {
    const text = document.createTextNode("hello");
    const cursor = textCaret(text, 2);
    const path = [cursor, textCaret(text, 3), textCaret(text, 4)];

    expect(fakeSelection(path).calls).toEqual([]);
    const h = fakeSelection(path);
    runMotion(h, cursor, "h", null);
    expect(h.calls).toEqual(["move backward character"]);

    const l = fakeSelection(path);
    runMotion(l, cursor, "l", null);
    expect(l.calls).toEqual(["move forward character"]);

    const e = fakeSelection(path);
    runMotion(e, cursor, "e", null);
    expect(e.calls).toEqual(["move forward character", "move forward word", "move backward character"]);

    const b = fakeSelection(path);
    runMotion(b, cursor, "b", null);
    expect(b.calls).toEqual(["move backward word"]);

    const zero = fakeSelection(path);
    runMotion(zero, cursor, "0", null);
    expect(zero.calls).toEqual(["move backward paragraphboundary"]);

    const dollar = fakeSelection(path);
    runMotion(dollar, cursor, "$", null);
    expect(dollar.calls).toEqual(["move forward paragraphboundary"]);
  });

  it("j/k issue one line-granularity modify() and clear the goal column on every OTHER motion", () => {
    const text = document.createTextNode("a\nb");
    const cursor = textCaret(text, 0);
    const path = [cursor, textCaret(text, 2)];

    const j = fakeSelection(path);
    const result = runMotion(j, cursor, "j", null);
    expect(j.calls).toEqual(["move forward line"]);
    // jsdom has no layout, so goal-column snapping does nothing and `goalX` falls back to null.
    // Real goal-column snapping is verified only by the browser harness.
    expect(result.goalX).toBeNull();

    const h = fakeSelection(path);
    const afterH = runMotion(h, cursor, "h", 40);
    expect(afterH.goalX).toBeNull();
  });
});

describe("visual.ts: counts stop at the first step that makes no progress", () => {
  it("repeatMotion stops early once a motion stops advancing, even with a larger count", () => {
    const root = document.createElement("div");
    const text = document.createTextNode("abcd");
    root.appendChild(text);
    const waypoints = [textCaret(text, 0), textCaret(text, 1), textCaret(text, 2)]; // the fake clamps here
    const sel = fakeSelection(waypoints);
    const model: VisualModel = { anchor: waypoints[0], cursor: waypoints[0], kind: "char", goalX: null };

    const after = repeatMotion(sel, model, "l", 5, root);

    expect(after.cursor).toEqual(waypoints[2]);
    // Only two real advances happened (0->1, 1->2); the fake's modify() was still called for the
    // third attempt (the one that discovered no progress), never a fourth or fifth.
    expect(sel.calls.length).toBe(3);
  });

  it("stepOnce undoes a step that would leave the list, reporting no progress", () => {
    const root = document.createElement("div");
    const inside = document.createTextNode("in");
    root.appendChild(inside);
    const outside = document.createTextNode("out"); // never appended to root
    const cursor = textCaret(inside, 0);
    const sel = fakeSelection([cursor, textCaret(outside, 0)]);
    const model: VisualModel = { anchor: cursor, cursor, kind: "char", goalX: null };

    const { model: after, moved } = stepOnce(sel, model, "l", root);

    expect(moved).toBe(false);
    expect(after).toBe(model);
  });
});

describe("visual.ts: chrome is skipped, never rested in", () => {
  it("isChromeNode matches every VISUAL_CHROME member", () => {
    const root = document.createElement("div");
    root.innerHTML =
      '<span class="row-sign" aria-hidden="true">›</span><button>Approve</button>' +
      '<input /><textarea></textarea><span data-nav-action="allow"></span>' +
      '<span class="fold-marker">▸</span><span class="prose">real text</span>';
    for (const selector of VISUAL_CHROME) {
      const el = root.querySelector(selector);
      expect(el, `no element matched VISUAL_CHROME selector ${selector}`).not.toBeNull();
      if (el !== null) expect(isChromeNode(el, root)).toBe(true);
    }
    const prose = root.querySelector(".prose");
    expect(prose).not.toBeNull();
    if (prose !== null) expect(isChromeNode(prose.firstChild ?? prose, root)).toBe(false);
  });

  it("stepOnce steps past a chrome waypoint to the real text beyond it", () => {
    const root = document.createElement("div");
    const before = document.createTextNode("a");
    const button = document.createElement("button");
    button.textContent = "x";
    const after = document.createTextNode("b");
    root.append(before, button, after);
    const chromeCaret = textCaret(button.firstChild as Text, 0);
    const path = [textCaret(before, 0), chromeCaret, textCaret(after, 0)];
    const sel = fakeSelection(path);
    const model: VisualModel = { anchor: path[0], cursor: path[0], kind: "char", goalX: null };

    const { model: landed, moved } = stepOnce(sel, model, "l", root);

    expect(moved).toBe(true);
    expect(landed.cursor).toEqual(path[2]);
  });
});

describe("visual.ts: o swaps ends", () => {
  it("swapEnds exchanges anchor and cursor and clears the goal column", () => {
    const text = document.createTextNode("ab");
    const a = textCaret(text, 0);
    const c = textCaret(text, 1);
    const model: VisualModel = { anchor: a, cursor: c, kind: "char", goalX: 12 };
    const swapped = swapEnds(model);
    expect(swapped.anchor).toEqual(c);
    expect(swapped.cursor).toEqual(a);
    expect(swapped.goalX).toBeNull();
  });
});

describe("visual.ts: rebuild", () => {
  function buildRebuildFixture() {
    const text = document.createTextNode("hello world");
    const earlier = textCaret(text, 2);
    const later = textCaret(text, 7);
    const lineStart = textCaret(text, 0);
    const lineEnd = textCaret(text, 11);
    return { text, earlier, later, lineStart, lineEnd };
  }

  it("charwise: setBaseAndExtent(earlier, later) then one extend-forward-character call, cursor after anchor", () => {
    const { text, earlier, later } = buildRebuildFixture();
    const sel = fakeSelection([earlier, later]);
    const model: VisualModel = { anchor: earlier, cursor: later, kind: "char", goalX: null };

    rebuildSelection(sel, model);

    expect(sel.calls).toEqual(["extend forward character"]);
    expect(sel.anchorNode).toBe(text);
    expect(sel.anchorOffset).toBe(earlier.offset);
  });

  it("charwise: reorders when the cursor sits BEFORE the anchor -- the native anchor is still the earlier point", () => {
    const { text, earlier, later } = buildRebuildFixture();
    const sel = fakeSelection([earlier, later]);
    // The user extended backward: anchor is the LATER document position, cursor the earlier one.
    const model: VisualModel = { anchor: later, cursor: earlier, kind: "char", goalX: null };

    rebuildSelection(sel, model);

    expect(sel.calls).toEqual(["extend forward character"]);
    expect(sel.anchorNode).toBe(text);
    expect(sel.anchorOffset).toBe(earlier.offset);
  });

  it("linewise: collapses to each end's paragraph boundary, then setBaseAndExtent(lineStart, lineEnd)", () => {
    const { earlier, later, lineStart, lineEnd } = buildRebuildFixture();
    const path = [earlier, lineStart, later, lineEnd];
    const sel = fakeSelection(path);
    const model: VisualModel = { anchor: earlier, cursor: later, kind: "line", goalX: null };

    const built = rebuildSelection(sel, model);

    expect(sel.calls).toEqual(["move backward paragraphboundary", "move forward paragraphboundary"]);
    expect(built.anchorOffset).toBe(lineStart.offset);
    expect(built.focusOffset).toBe(lineEnd.offset);
  });
});

describe("visual.ts: copySelectionText hides chrome for one synchronous read", () => {
  it("marks only the rows the selection touches, reads toString(), then clears the marks", () => {
    const root = document.createElement("div");
    root.className = "message-list";
    const row1 = document.createElement("div");
    row1.setAttribute("data-nav-stop", "row");
    const row1Text = document.createTextNode("first row");
    row1.appendChild(row1Text);
    const row2 = document.createElement("div");
    row2.setAttribute("data-nav-stop", "row");
    const row2Text = document.createTextNode("second row");
    row2.appendChild(row2Text);
    const row3 = document.createElement("div");
    row3.setAttribute("data-nav-stop", "row");
    row3.appendChild(document.createTextNode("third row"));
    root.append(row1, row2, row3);

    let sawAttributeSet = false;
    const sel: SelectionLike = {
      anchorNode: row1Text,
      anchorOffset: 0,
      focusNode: row2Text,
      focusOffset: 0,
      rangeCount: 1,
      getRangeAt(): Range {
        const range = document.createRange();
        range.setStart(row1Text, 0);
        range.setEnd(row2Text, 0);
        return range;
      },
      collapse() {},
      setBaseAndExtent() {},
      modify() {},
      toString() {
        sawAttributeSet =
          row1.getAttribute("data-visual-copying") === "true" &&
          row2.getAttribute("data-visual-copying") === "true" &&
          !row3.hasAttribute("data-visual-copying");
        return "first row\nsecond row";
      },
    };
    const model: VisualModel = { anchor: textCaret(row1Text, 0), cursor: textCaret(row2Text, 0), kind: "char", goalX: null };

    const text = copySelectionText(sel, root, model);

    expect(text).toBe("first row\nsecond row");
    expect(sawAttributeSet).toBe(true);
    expect(row1.hasAttribute("data-visual-copying")).toBe(false);
    expect(row2.hasAttribute("data-visual-copying")).toBe(false);
    expect(row3.hasAttribute("data-visual-copying")).toBe(false);
  });

  /* `.message-list` contains the history notice above its rows, including a `Copy path` button.
     `k` from the first row can extend the selection into that notice, so the notice needs the
     copying attribute too. jsdom's `toString()` ignores CSS; this test checks the attribute
     at read time, where `[data-visual-copying] button` in `index.css` hides the controls. */
  it("also marks a block of the list that is not a row -- the history notice -- when the selection crosses it", () => {
    const root = document.createElement("div");
    root.className = "agent-ui-conversation";
    const list = document.createElement("div");
    list.className = "message-list";
    const notice = document.createElement("div");
    notice.className = "history-notice";
    notice.setAttribute("data-nav-stop", "notice");
    const noticeText = document.createTextNode("Restored 12 items from the transcript");
    const noticeSpan = document.createElement("span");
    noticeSpan.appendChild(noticeText);
    const copyPath = document.createElement("button");
    copyPath.textContent = "Copy path";
    notice.append(noticeSpan, copyPath);
    const row1 = document.createElement("div");
    row1.setAttribute("data-nav-stop", "row");
    const row1Text = document.createTextNode("first row");
    row1.appendChild(row1Text);
    const row2 = document.createElement("div");
    row2.setAttribute("data-nav-stop", "row");
    row2.appendChild(document.createTextNode("second row"));
    list.append(notice, row1, row2);
    root.appendChild(list);

    const marked = new Set<Element>();
    const range = document.createRange();
    range.setStart(noticeText, 9);
    range.setEnd(row1Text, 5);
    const sel: SelectionLike = {
      anchorNode: noticeText,
      anchorOffset: 9,
      focusNode: row1Text,
      focusOffset: 5,
      rangeCount: 1,
      getRangeAt: () => range,
      collapse() {},
      setBaseAndExtent() {},
      modify() {},
      toString() {
        for (const el of [notice, row1, row2]) if (el.getAttribute("data-visual-copying") === "true") marked.add(el);
        return "12 items from the transcript\nfirst";
      },
    };
    // The anchor is in no row at all: `rowOf` finds nothing for the notice.
    const model: VisualModel = { anchor: textCaret(noticeText, 9), cursor: textCaret(row1Text, 4), kind: "char", goalX: null };

    copySelectionText(sel, root, model);

    expect(marked.has(notice), "the notice the selection starts in is shielded too").toBe(true);
    expect(marked.has(row1)).toBe(true);
    for (const el of [notice, row1, row2]) expect(el.hasAttribute("data-visual-copying")).toBe(false);
  });

  it("marks no block the selection does not cross: a selection inside one row leaves the notice alone", () => {
    const list = document.createElement("div");
    list.className = "message-list";
    const notice = document.createElement("div");
    notice.className = "history-notice";
    notice.appendChild(document.createTextNode("Restored"));
    const row = document.createElement("div");
    row.setAttribute("data-nav-stop", "row");
    const rowText = document.createTextNode("only row");
    row.appendChild(rowText);
    list.append(notice, row);
    const range = document.createRange();
    range.setStart(rowText, 0);
    range.setEnd(rowText, 4);
    let noticeMarked: boolean | null = null;
    const sel: SelectionLike = {
      anchorNode: rowText,
      anchorOffset: 0,
      focusNode: rowText,
      focusOffset: 4,
      rangeCount: 1,
      getRangeAt: () => range,
      collapse() {},
      setBaseAndExtent() {},
      modify() {},
      toString() {
        noticeMarked = notice.hasAttribute("data-visual-copying");
        return "only";
      },
    };
    copySelectionText(sel, list, { anchor: textCaret(rowText, 0), cursor: textCaret(rowText, 3), kind: "char", goalX: null });
    expect(noticeMarked).toBe(false);
  });
});

describe("visual.ts: entry caret and caret ordering", () => {
  it("firstSelectableCaret skips chrome and lands on the first real character", () => {
    const row = document.createElement("div");
    row.innerHTML = '<span class="row-sign" aria-hidden="true">›</span><div class="row-body">hello</div>';
    const caret = firstSelectableCaret(row);
    expect(caret).not.toBeNull();
    expect(caret?.node.textContent).toBe("hello");
    expect(caret?.offset).toBe(0);
  });

  it("firstSelectableCaret returns null for a row with no selectable text at all", () => {
    const row = document.createElement("div");
    row.innerHTML = '<button>Approve</button><input />';
    expect(firstSelectableCaret(row)).toBeNull();
  });

  it("compareCarets orders two carets in the same text node by offset", () => {
    const text = document.createTextNode("abcdef");
    expect(compareCarets(textCaret(text, 1), textCaret(text, 4))).toBeLessThan(0);
    expect(compareCarets(textCaret(text, 4), textCaret(text, 1))).toBeGreaterThan(0);
    expect(compareCarets(textCaret(text, 2), textCaret(text, 2))).toBe(0);
  });
});

// A motion this file exercises for every table row (`CharMotion`, i.e. every `VisualMotion` but
// `gg`/`G`, which never reach `runMotion` at all), so `visual.ts`'s own exhaustiveness check
// (`runMotion`'s `default` branch) has no untested member.
const _allMotionsCovered: CharMotion[] = ["h", "l", "j", "k", "w", "e", "b", "0", "$"];
void _allMotionsCovered;

describe("visual.ts: gg/G and CARET's dual-end motion", () => {
  // G lands on the last character. One past it would extend the inclusive caret block outside
  // the list, causing the containment check to refuse `y`/`>`.
  it("lastSelectableCaret skips chrome and lands ON the last real character, not past it", () => {
    const row = document.createElement("div");
    row.innerHTML = '<div class="row-body">hello</div><span class="row-sign" aria-hidden="true">›</span>';
    const caret = lastSelectableCaret(row);
    expect(caret).not.toBeNull();
    expect(caret?.node.textContent).toBe("hello");
    expect(caret?.offset).toBe(4);
  });

  it("lastSelectableCaret skips trailing whitespace, and never lands inside a surrogate pair or on a combining mark", () => {
    const pre = document.createElement("div");
    pre.innerHTML = "<pre>fn a() {\n}\n</pre>";
    const inPre = lastSelectableCaret(pre)!;
    expect((inPre.node as Text).data[inPre.offset]).toBe("}");

    const astral = document.createElement("div");
    astral.textContent = "ok \u{1F600}";
    expect(lastSelectableCaret(astral)?.offset).toBe(3); // the high surrogate, the pair's start

    const combining = document.createElement("div");
    combining.textContent = "cafe\u0301";
    expect(lastSelectableCaret(combining)?.offset).toBe(3); // the "e", not its accent
  });

  it("stepOnce never lets a motion land past the list's last character: it lands ON it, as G does", () => {
    const root = document.createElement("div");
    root.innerHTML = '<div class="row-body">first</div><div class="row-body">last</div><span class="row-sign" aria-hidden="true">›</span>';
    const last = root.querySelectorAll(".row-body")[1]?.firstChild as Text;
    const onT: VisualModel = { anchor: textCaret(last, 3), cursor: textCaret(last, 3), kind: "caret", goalX: null };
    // `$` (or `l`) from the last character reaches the end of the text node: one past the list.
    const pastEnd = stepOnce(fakeSelection([textCaret(last, 3), textCaret(last, 4)]), onT, "$", root);
    expect(pastEnd.model.cursor).toEqual(textCaret(last, 3));
    expect(pastEnd.moved).toBe(false);
    // A text-node end with more text after it in the list is left unchanged by `$`.
    const first = root.querySelector(".row-body")?.firstChild as Text;
    const onFirst: VisualModel = { anchor: textCaret(first, 0), cursor: textCaret(first, 0), kind: "caret", goalX: null };
    const endOfFirst = stepOnce(fakeSelection([textCaret(first, 0), textCaret(first, 5)]), onFirst, "$", root);
    expect(endOfFirst.model.cursor).toEqual(textCaret(first, 5));
    expect(endOfFirst.moved).toBe(true);
  });

  // Centring the caret's parent after each `j`/`k` step makes the view jump on every key.
  // A step must not call `scrollIntoView`.
  it("stepOnce's j/k never call scrollIntoView", () => {
    const root = document.createElement("div");
    root.innerHTML = '<div class="row-body">one</div><div class="row-body">two</div>';
    const [one, two] = Array.from(root.querySelectorAll(".row-body")).map((el) => el.firstChild as Text);
    const original = Element.prototype.scrollIntoView;
    const calls: Element[] = [];
    Element.prototype.scrollIntoView = function (this: Element) {
      calls.push(this);
    };
    try {
      const model: VisualModel = { anchor: textCaret(one!, 0), cursor: textCaret(one!, 0), kind: "caret", goalX: null };
      const down = stepOnce(fakeSelection([textCaret(one!, 0), textCaret(two!, 0)]), model, "j", root);
      expect(down.moved).toBe(true);
      stepOnce(fakeSelection([textCaret(two!, 0), textCaret(one!, 0)]), down.model, "k", root);
    } finally {
      Element.prototype.scrollIntoView = original;
    }
    expect(calls).toEqual([]);
  });

  it("lastSelectableCaret returns null for a row with no selectable text at all", () => {
    const row = document.createElement("div");
    row.innerHTML = "<button>Approve</button><input />";
    expect(lastSelectableCaret(row)).toBeNull();
  });

  it("stepOnce places gg/G directly at the list's first/last selectable character, never through Selection.modify", () => {
    const root = document.createElement("div");
    root.innerHTML = '<div class="row-body">first row</div><div class="row-body">second row</div>';
    const secondText = root.querySelectorAll(".row-body")[1]?.firstChild as Text;
    const model: VisualModel = { anchor: textCaret(secondText, 3), cursor: textCaret(secondText, 3), kind: "char", goalX: 40 };
    const sel = fakeSelection([textCaret(secondText, 3)]);

    const gg = stepOnce(sel, model, "gg", root);
    expect(gg.moved).toBe(true);
    expect(gg.model.cursor.node.textContent).toBe("first row");
    expect(gg.model.cursor.offset).toBe(0);
    expect(gg.model.goalX).toBeNull();
    expect(sel.calls).toEqual([]); // never touched Selection.modify

    const G = stepOnce(sel, model, "G", root);
    expect(G.moved).toBe(true);
    expect(G.model.cursor.node.textContent).toBe("second row");
    expect(G.model.cursor.offset).toBe(9); // ON the final "w"
  });

  it("stepOnce reports no progress when gg/G would not move the cursor", () => {
    const root = document.createElement("div");
    root.innerHTML = '<div class="row-body">only row</div>';
    const text = root.querySelector(".row-body")?.firstChild as Text;
    const atStart: VisualModel = { anchor: textCaret(text, 0), cursor: textCaret(text, 0), kind: "char", goalX: null };
    const sel = fakeSelection([textCaret(text, 0)]);
    const result = stepOnce(sel, atStart, "gg", root);
    expect(result.moved).toBe(false);
    expect(result.model).toBe(atStart);
  });

  it("in CARET both ends land on the same place after a motion; in VISUAL only cursor moves", () => {
    const text = document.createTextNode("abcd");
    const root = document.createElement("div");
    root.appendChild(text);
    const path = [textCaret(text, 0), textCaret(text, 1)];

    const caretModel: VisualModel = { anchor: textCaret(text, 0), cursor: textCaret(text, 0), kind: "caret", goalX: null };
    const caretResult = stepOnce(fakeSelection(path), caretModel, "l", root);
    expect(caretResult.moved).toBe(true);
    expect(caretResult.model.anchor).toEqual(caretResult.model.cursor);
    expect(caretResult.model.cursor.offset).toBe(1);

    const charModel: VisualModel = { anchor: textCaret(text, 0), cursor: textCaret(text, 0), kind: "char", goalX: null };
    const charResult = stepOnce(fakeSelection(path), charModel, "l", root);
    expect(charResult.moved).toBe(true);
    expect(charResult.model.anchor.offset).toBe(0);
    expect(charResult.model.cursor.offset).toBe(1);
  });

  // In a table, V-LINE's hard line is the whole row. `paragraphboundary` alone stops at a cell
  // and can omit the neighbouring cells from the copy.
  it("V-LINE inside a table spans whole rows: the first row's first cell to the last row's last cell", () => {
    const table = document.createElement("table");
    table.innerHTML = "<tbody>\n<tr><td>a</td><td>b</td></tr>\n<tr><td>1</td><td>2 </td></tr>\n</tbody>";
    document.body.appendChild(table);
    try {
      const [a, b, one, two] = Array.from(table.querySelectorAll("td")).map((td) => td.firstChild as Text);
      const model: VisualModel = { anchor: textCaret(b!, 0), cursor: textCaret(one!, 0), kind: "line", goalX: null };
      const built = rebuildSelection(fakeSelection([textCaret(b!, 0), textCaret(one!, 1)]), model);
      expect(built.anchorNode).toBe(a);
      expect(built.anchorOffset).toBe(0);
      expect(built.focusNode).toBe(two);
      expect(built.focusOffset).toBe(1); // just past "2", its trailing space left out
    } finally {
      table.remove();
    }
  });

  it("CARET's own rebuild is a one-character selection, the same as VISUAL's with anchor === cursor", () => {
    const text = document.createTextNode("hello");
    const caret = textCaret(text, 2);
    const model: VisualModel = { anchor: caret, cursor: caret, kind: "caret", goalX: null };
    const sel = fakeSelection([caret, textCaret(text, 3)]);

    rebuildSelection(sel, model);

    expect(sel.calls).toEqual(["extend forward character"]);
  });
});

// A wrapped neighbouring cell can make a `<tr>` taller than `MAX_LINE_PROBE_PX`, leaving
// `stepByLinePoint` unable to reach the next row. `stepToAdjacentTableRow` uses DOM position
// so `j`/`k` can continue. The fake `SelectionLike.modify("line")` stays in the current text
// node and lands at its start/end, mimicking a browser that cannot cross the cell boundary.
// That makes `advancedVisually` false and exercises the fallback chain. jsdom lacks
// `getClientRects` and `caretRangeFromPoint`, so the pixel probe returns `null` here.
// This tests the boundary failure and DOM fallback, not the rendered height of a tall row.
describe("visual.ts: stepToAdjacentTableRow -- j/k cross a table row the pixel probe cannot reach", () => {
  /** `alter: "move"` only -- every `CharMotion` case in `runMotion` uses it, never `"extend"`. Line
   *  granularity collapses to the caret's OWN node's start (backward) or end (forward), the same as
   *  `App.test.tsx`'s `stubSelectionModify`'s non-character branch: a real `<td>`'s own line granularity
   *  never reaches into a sibling cell either. */
  function lineBoundarySelection(): SelectionLike {
    let focusNode: Node = document.createTextNode("");
    let focusOffset = 0;
    return {
      get anchorNode() {
        return focusNode;
      },
      get anchorOffset() {
        return focusOffset;
      },
      get focusNode() {
        return focusNode;
      },
      get focusOffset() {
        return focusOffset;
      },
      rangeCount: 1,
      getRangeAt() {
        const r = document.createRange();
        r.setStart(focusNode, focusOffset);
        return r;
      },
      collapse(node, offset) {
        focusNode = node;
        focusOffset = offset;
      },
      setBaseAndExtent(_aN, _aO, fN, fO) {
        focusNode = fN;
        focusOffset = fO;
      },
      modify(_alter, direction, _granularity) {
        const text = focusNode.textContent ?? "";
        focusOffset = direction === "forward" ? text.length : 0;
      },
      toString() {
        return "";
      },
    };
  }

  function threeRowTable() {
    const table = document.createElement("table");
    table.innerHTML = "<tbody>\n<tr><td>a</td><td>b</td></tr>\n<tr><td>1</td><td>2</td></tr>\n<tr><td>x</td><td>y</td></tr>\n</tbody>";
    document.body.appendChild(table);
    const cells = Array.from(table.querySelectorAll("td")).map((td) => td.firstChild as Text);
    return { table, cells };
  }

  it("j from the end of a cell lands on the SAME column's cell in the next row, not stuck", () => {
    const { table, cells } = threeRowTable();
    try {
      const [a, , one] = cells; // row 1 col 0 ("a"), row 2 col 0 ("1")
      const cursor = textCaret(a!, 1); // already at "a"'s own end -- nothing left for modify() alone
      const { caret, goalX: _goalX } = runMotion(lineBoundarySelection(), cursor, "j", null, table);
      expect(caret.node).toBe(one);
      expect(caret.offset).toBe(0);
    } finally {
      table.remove();
    }
  });

  it("k from the start of a cell lands on the SAME column's cell in the previous row", () => {
    const { table, cells } = threeRowTable();
    try {
      const [a, , one] = cells; // row 1 col 0 ("a"), row 2 col 0 ("1")
      const cursor = textCaret(one!, 0); // already at "1"'s own start
      const { caret } = runMotion(lineBoundarySelection(), cursor, "k", null, table);
      expect(caret.node).toBe(a);
      expect(caret.offset).toBe(0);
    } finally {
      table.remove();
    }
  });

  it("a column past the target row's own last cell clamps to that row's last cell", () => {
    const table = document.createElement("table");
    // Row 1 has three cells, row 2 only two -- the third column has nowhere to go but the last one.
    table.innerHTML = "<tbody>\n<tr><td>a</td><td>b</td><td>c</td></tr>\n<tr><td>1</td><td>2</td></tr>\n</tbody>";
    document.body.appendChild(table);
    try {
      const cells = Array.from(table.querySelectorAll("td")).map((td) => td.firstChild as Text);
      const [, , c, , two] = cells;
      const cursor = textCaret(c!, 1); // row 1, column 2 ("c")
      const { caret } = runMotion(lineBoundarySelection(), cursor, "j", null, table);
      expect(caret.node).toBe(two); // row 2's own last cell, clamped
      expect(caret.offset).toBe(0);
    } finally {
      table.remove();
    }
  });

  it("j at the table's last row finds no sibling and leaves the caret where modify() left it (the pre-existing boundary)", () => {
    const { table, cells } = threeRowTable();
    try {
      const [, , , , x] = cells; // row 3 col 0 ("x")
      const cursor = textCaret(x!, 1); // already at "x"'s own end
      const { caret } = runMotion(lineBoundarySelection(), cursor, "j", null, table);
      // No fourth row to fall back to: caret stays exactly where the (no-op) modify() left it.
      expect(caret.node).toBe(x);
      expect(caret.offset).toBe(1);
    } finally {
      table.remove();
    }
  });

  // Chained `repeatMotion`/`9999j` across multiple rows needs real layout. After this fallback
  // lands at cell offset 0, jsdom cannot distinguish the next offset-0-to-length move from a new
  // visual line. A browser checks `landedRect.top >= startRect.bottom` and invokes the fallback again
  // if still on the same row. The multi-row composition is covered by the browser harness in
  // `shell/tests/panel_visual_mode.rs`.
});

describe("visual.ts: revealCaret, least scroll", () => {
  /** A caret inside an inner scroll box inside the list, laid out by hand: jsdom lays out nothing,
   *  so each rect is derived from the two boxes' own `scrollTop`, the way a real engine moves them. */
  function nested() {
    const list = document.createElement("div");
    const box = document.createElement("div");
    const text = document.createTextNode("line");
    box.appendChild(text);
    list.appendChild(box);
    document.body.appendChild(list);
    const scroll = { list: 0, box: 0 };
    for (const [el, key, top, bottom] of [
      [list, "list", 0, 100],
      [box, "box", 20, 60],
    ] as const) {
      el.style.overflowY = "auto";
      Object.defineProperty(el, "scrollHeight", { value: 1000 });
      Object.defineProperty(el, "clientHeight", { value: bottom - top });
      Object.defineProperty(el, "scrollTop", {
        get: () => scroll[key],
        set: (v: number) => {
          scroll[key] = v;
        },
      });
      el.getBoundingClientRect = () => {
        const shift = key === "box" ? scroll.list : 0;
        return { top: top - shift, bottom: bottom - shift, left: 0, right: 100, width: 100, height: bottom - top, x: 0, y: top - shift, toJSON() {} } as DOMRect;
      };
    }
    // The caret sits 150px down the box's content: its client top is that, less both scrolls.
    const proto = Range.prototype as unknown as { getClientRects?: () => DOMRect[] };
    const original = proto.getClientRects;
    proto.getClientRects = () => {
      const top = 150 - scroll.box - scroll.list;
      return [{ top, bottom: top + 10, left: 5, right: 5, width: 0, height: 10, x: 5, y: top, toJSON() {} } as DOMRect];
    };
    return {
      list,
      caret: textCaret(text, 0),
      scroll,
      restore() {
        if (original === undefined) delete proto.getClientRects;
        else proto.getClientRects = original;
        list.remove();
      },
    };
  }

  it("nudges the inner box by the least, then re-measures before the list -- which then need not move", () => {
    const { list, caret, scroll, restore } = nested();
    try {
      expect(revealCaret(caret, list)).toBe(true);
      expect(scroll.box).toBe(100); // caret bottom 160 brought to the box's bottom, 60
      // With the rect measured once (the old body), the list scrolled another 60px it never needed.
      expect(scroll.list).toBe(0);
    } finally {
      restore();
    }
  });

  it("moves nothing when the caret is already on screen", () => {
    const { list, caret, scroll, restore } = nested();
    try {
      scroll.box = 110; // caret at 40..50, inside the box (20..60) and the list
      expect(revealCaret(caret, list)).toBe(false);
      expect(scroll).toEqual({ list: 0, box: 110 });
    } finally {
      restore();
    }
  });
});
