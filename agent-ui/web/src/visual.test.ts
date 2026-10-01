// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import {
  compareCarets,
  copySelectionText,
  fallbackGraphemeStarts,
  firstSelectableCaret,
  isChromeNode,
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

/** A `SelectionLike` whose `modify()` walks a fixed, caller-supplied path of carets (one step per
 *  call, clamped at the last), and records every call it saw. It has no idea what "forward word"
 *  actually means -- the point is to verify what `visual.ts` ASKS the selection to do (D4's own
 *  table), and to give `stepOnce`/`repeatMotion` a real sequence of DOM positions to walk so their
 *  boundary/chrome/no-progress logic (D5/D6) can be exercised against a real small DOM tree. */
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

describe("visual.ts: the D4 table's modify() calls", () => {
  it("h/l/0/$ each issue exactly the D4 table's own modify() calls; w/e/b issue none (2026-10-01)", () => {
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

    // w/e/b follow vim's own word rules over the DOM text (2026-10-01), never WebKit's `word`
    // granularity, whose boundaries are not vim's (`has_flag` is one ICU word, `=`/`|` none).
    for (const motion of ["w", "e", "b"] as const) {
      const sel = fakeSelection(path);
      runMotion(sel, cursor, motion, null);
      expect(sel.calls, motion).toEqual([]);
    }

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
    // jsdom has no layout, so the goal-column snap is a no-op and goalX falls back to null here --
    // real-browser goal-column snapping is R3, verified only in the real-WebKit harness.
    expect(result.goalX).toBeNull();

    const h = fakeSelection(path);
    const afterH = runMotion(h, cursor, "h", 40);
    expect(afterH.goalX).toBeNull();
  });
});

describe("visual.ts: D5 counts stop at the first step that makes no progress", () => {
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

describe("visual.ts: D6 chrome is skipped, never rested in", () => {
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

describe("visual.ts: D3 rebuild", () => {
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

  /* Fix round 3 (review finding 7, minor): D8's shield covered rows only, but `.message-list` also
     holds the history notice -- not a row, above the first one, with its own `Copy path` button --
     and `k` from the first row can put an end of the selection in the notice's text. Its button
     reached `toString()` unshielded. jsdom's `toString()` ignores CSS, so what is asserted is the
     mark itself, at the moment of the read: the rule that hides the button keys on it (`index.css`,
     `[data-visual-copying] button`). */
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

describe("visual.ts: D5 gg/G (added for 3a) and CARET's dual-end motion", () => {
  // Fix round 2 (review finding): G lands ON the last character, never one past it -- past it, the
  // caret's inclusive one-character block (D3) reached out of the list and D9 refused y/>.
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
    // The same end of a text node with more text after it in the list is left alone (D5's `$`).
    const first = root.querySelector(".row-body")?.firstChild as Text;
    const onFirst: VisualModel = { anchor: textCaret(first, 0), cursor: textCaret(first, 0), kind: "caret", goalX: null };
    const endOfFirst = stepOnce(fakeSelection([textCaret(first, 0), textCaret(first, 5)]), onFirst, "$", root);
    expect(endOfFirst.model.cursor).toEqual(textCaret(first, 5));
    expect(endOfFirst.moved).toBe(true);
  });

  // Fix round 2 (review finding, D8): fix round 1 re-centred the caret's parent after every j/k step,
  // jumping the view on every key; nothing in a step may call scrollIntoView now.
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
    expect(G.model.cursor.offset).toBe(9); // ON the final "w" (fix round 2)
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

  it("D5: in CARET both ends land on the same place after a motion; in VISUAL only cursor moves", () => {
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

  // Fix round 2: in a table, V-LINE's hard line is the row -- paragraphboundary alone stopped at the
  // cell, and the real-WebKit W5 copied the last row as "3" without the "4" beside it.
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

// Fix round 3 (item 3a, review finding, minor): a `<tr>` made TALL by a wrapped neighbouring cell can
// put the next row further away than `stepByLinePoint`'s `MAX_LINE_PROBE_PX` pixel budget, so a real
// engine's own probe never finds it and `j`/`k` (and a counted `9999j`) stop inside the table even
// though it plainly has more rows. `runMotion`'s new DOM-position fallback (`stepToAdjacentTableRow`)
// is exercised here through a `SelectionLike` whose `modify("line")` mimics a real engine's own
// refusal to cross a `<table>` row boundary (the same shape `App.test.tsx`'s `stubSelectionModify`
// uses for jsdom's real `Selection`): it stays within the caret's own text node, landing at that
// node's start/end -- exactly the "nothing left to do locally" state that makes `advancedVisually`
// false and hands off to the fallback chain. jsdom has neither `getClientRects` nor
// `caretRangeFromPoint` (R3), so `stepByLinePoint` is already a guaranteed `null` here, and this
// fallback is what a real engine would additionally need once ITS pixel probe also came up empty for
// the same tall-row reason -- this test cannot reproduce "tall" as a pixel measurement, only the
// modify()-can't-cross-cells shape both engines share.
describe("visual.ts: stepToAdjacentTableRow -- j/k cross a table row the pixel probe cannot reach (fix round 3)", () => {
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

  // A chained `repeatMotion`/`9999j` across MULTIPLE rows is deliberately not exercised here: once
  // this fallback lands at a cell's offset 0, jsdom's own "no real layout, trust modify's own result"
  // heuristic (the comment on `advancedVisually` above, R3) cannot tell that cell's own offset 0 ->
  // length move (what the NEXT `j` immediately does) apart from a genuine move to a new visual line --
  // a real engine's geometry check (`landedRect.top >= startRect.bottom`) correctly says "no, still
  // the same row" and this fallback fires again; jsdom has no such check to fall back on. That
  // composition is exactly what real geometry is needed to arbitrate, so it belongs in the real-WebKit
  // harness (`shell/tests/panel_visual_mode.rs`), not here -- see its own new table-row case.
});

describe("visual.ts: revealCaret, D8's least scroll (fix round 2)", () => {
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

/* CARET/VISUAL `w`/`e`/`b` follow vim's own word rules over the DOM text (2026-10-01). The owner,
   on the real panel, in a code block showing the line below: the first `w` landed before `has`,
   the second did not move -- WebKit's `Selection.modify(..., "word")` steps, composed, returned to
   their start. Every expected sequence here is nvim's own, measured headless (`nvim --headless
   --clean`, a `normal! w`/`e`/`b` loop printing the cursor until it stops moving) on the same text,
   written in nvim's own `line:byte-col` form and converted to this DOM's UTF-16 offsets. */
describe("visual.ts: w/e/b follow vim's word rules (2026-10-01)", () => {
  /** A `.message-list` holding `html`, the shape every caller's `root` has. */
  function listOf(html: string): HTMLElement {
    const root = document.createElement("div");
    root.className = "message-list";
    root.innerHTML = html;
    return root;
  }

  /** `caret`'s offset in `root`'s whole text (every text node before it, in document order). */
  function globalOffset(root: Element, caret: Caret): number {
    const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
    let total = 0;
    for (let node = walker.nextNode(); node !== null; node = walker.nextNode()) {
      if (node === caret.node) return total + caret.offset;
      total += (node as Text).data.length;
    }
    throw new Error("caret not inside root");
  }

  /** The caret at global offset `at` of `root`'s text. */
  function caretAt(root: Element, at: number): Caret {
    const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
    let total = 0;
    for (let node = walker.nextNode(); node !== null; node = walker.nextNode()) {
      const length = (node as Text).data.length;
      if (at < total + length) return { node, offset: at - total };
      total += length;
    }
    throw new Error(`offset ${at} past root's text`);
  }

  /** nvim's `line:byte-col` positions, over `lines` joined by `separator` (`""` where the lines are
   *  separate blocks with no text between them, `"\n"` inside one `<pre>`), as UTF-16 offsets. */
  function fromVim(lines: string[], separator: string, positions: string): number[] {
    const bytes = new TextEncoder();
    return positions.split(" ").map((position) => {
      const [line, col] = position.split(":").map(Number) as [number, number];
      const text = lines[line - 1]!;
      let units = 0;
      while (bytes.encode(text.slice(0, units)).length < col) units++;
      return lines.slice(0, line - 1).reduce((sum, l) => sum + l.length + separator.length, 0) + units;
    });
  }

  /** Every landing of `motion` from offset `from` of `within`'s text (the whole list by default; the
   *  `<code>` where a row sign precedes it), one `stepOnce` at a time in CARET, until a step makes no
   *  progress (the same stop `repeatMotion` uses). The walk's own bound is always the whole list. */
  function landings(root: Element, from: number, motion: "w" | "e" | "b", within: Element = root): number[] {
    let model: VisualModel = { anchor: caretAt(within, from), cursor: caretAt(within, from), kind: "caret", goalX: null };
    const seen: number[] = [];
    for (let i = 0; i < 60; i++) {
      const step = stepOnce(fakeSelection([model.cursor]), model, motion, root);
      if (!step.moved) break;
      model = step.model;
      expect(model.anchor).toEqual(model.cursor); // CARET moves both ends (D5)
      seen.push(globalOffset(within, model.cursor));
    }
    return seen;
  }

  const OWNER_LINE = "let has_flag = |flag: &str| flag_given(..., flag);";
  // As highlight.js renders it inside a fenced code block: the keyword and the type in spans.
  const OWNER_HTML =
    '<div class="row"><span class="row-sign" aria-hidden="true">›</span><div class="row-body">' +
    '<pre><code class="hljs language-rust"><span class="hljs-keyword">let</span> has_flag = |flag: &amp;' +
    '<span class="hljs-type">str</span>| flag_given(..., flag);</code></pre></div></div>';

  const code = (root: Element) => root.querySelector("code")!;

  it("the owner's line: w from `let` visits every vim word, then clamps on the last character", () => {
    const root = listOf(OWNER_HTML);
    expect(root.querySelector("code")?.textContent).toBe(OWNER_LINE);
    expect(landings(root, 0, "w", code(root))).toEqual(fromVim([OWNER_LINE], "", "1:4 1:13 1:15 1:16 1:20 1:22 1:23 1:26 1:28 1:38 1:44 1:48 1:49"));
  });

  it("the owner's line: e from `let` lands on every vim word's last character", () => {
    const root = listOf(OWNER_HTML);
    expect(landings(root, 0, "e", code(root))).toEqual(fromVim([OWNER_LINE], "", "1:2 1:11 1:13 1:15 1:19 1:20 1:22 1:25 1:26 1:37 1:42 1:47 1:49"));
  });

  it("the owner's line: b from the last character walks back to `let`", () => {
    const root = listOf(OWNER_HTML);
    expect(landings(root, OWNER_LINE.length - 1, "b", code(root))).toEqual(
      fromVim([OWNER_LINE], "", "1:48 1:44 1:38 1:28 1:26 1:23 1:22 1:20 1:16 1:15 1:13 1:4 1:0"),
    );
  });

  it("across two paragraphs, the block boundary separates words like a line break", () => {
    const lines = ["alpha beta", "gamma delta"];
    const root = listOf("<p>alpha beta</p><p>gamma delta</p>");
    expect(landings(root, 0, "w")).toEqual(fromVim(lines, "", "1:6 2:0 2:6 2:10"));
    expect(landings(root, 0, "e")).toEqual(fromVim(lines, "", "1:4 1:9 2:4 2:10"));
    expect(landings(root, 20, "b")).toEqual(fromVim(lines, "", "2:6 2:0 1:6 1:0"));
  });

  it("across a code block's two lines", () => {
    const lines = ["alpha beta", "gamma delta"];
    const root = listOf("<pre><code>alpha beta\ngamma delta</code></pre>");
    expect(landings(root, 0, "w")).toEqual(fromVim(lines, "\n", "1:6 2:0 2:6 2:10"));
    expect(landings(root, 0, "e")).toEqual(fromVim(lines, "\n", "1:4 1:9 2:4 2:10"));
    expect(landings(root, 21, "b")).toEqual(fromVim(lines, "\n", "2:6 2:0 1:6 1:0"));
  });

  it("non-ASCII letters are keyword characters; CJK ideographs are their own class, as in vim", () => {
    const line = "café naïve 中文x  über_1";
    const root = listOf(`<p>${line}</p>`);
    expect(landings(root, 0, "w")).toEqual(fromVim([line], "", "1:6 1:13 1:19 1:22 1:28"));
    expect(landings(root, 0, "e")).toEqual(fromVim([line], "", "1:3 1:11 1:16 1:19 1:28"));
    // vim's own b reaches 1:0 too; here 1:0 is the start of the text, which it lands on.
    expect(landings(root, line.length - 1, "b")).toEqual(fromVim([line], "", "1:22 1:19 1:13 1:6 1:0"));
  });

  it("Chinese: a run of hanzi is one word, split by fullwidth punctuation and from Latin letters", () => {
    const line = "这是一段中文，用来记录 ICU 分词的实际结果。";
    const root = listOf(`<p>${line}</p>`);
    expect(landings(root, 0, "w")).toEqual(fromVim([line], "", "1:18 1:21 1:34 1:38 1:59"));
    expect(landings(root, 0, "e")).toEqual(fromVim([line], "", "1:15 1:18 1:30 1:36 1:56 1:59"));
  });

  it("a combining mark belongs to its base character: never a word break, never a landing", () => {
    const root = listOf("<p>café x́y z</p>");
    expect(landings(root, 0, "w")).toEqual([6, 10]);
    expect(landings(root, 0, "e")).toEqual([3, 8, 10]);
  });

  it("from whitespace, w and e go to the next word; b with no word before stays put", () => {
    const line = "  lead  word";
    const root = listOf(`<pre><code>${line}</code></pre>`);
    expect(landings(root, 0, "w")).toEqual(fromVim([line], "", "1:2 1:8 1:11"));
    expect(landings(root, 0, "e")).toEqual(fromVim([line], "", "1:5 1:11"));
    // vim's b from 1:2 goes on to 1:0, a blank; this panel never lands a caret on one.
    expect(landings(root, line.length - 1, "b")).toEqual(fromVim([line], "", "1:8 1:2"));
  });

  it("the last word: w and e clamp on the list's last character, then stop", () => {
    const root = listOf("<p>one two</p>");
    expect(landings(root, 4, "w")).toEqual([6]); // vim: w from the last word reaches its end
    expect(landings(root, 6, "w")).toEqual([]);
    expect(landings(root, 6, "e")).toEqual([]);
    expect(landings(root, 0, "b")).toEqual([]);
  });

  it("a highlight span inside a word does not split it; chrome and rows separate words", () => {
    const root = listOf(
      '<div class="row"><span class="row-sign" aria-hidden="true">›</span><div class="row-body">' +
        '<pre><code><span class="hljs-title">flag</span>_given(x)</code></pre></div></div>' +
        '<div class="row"><span class="row-sign" aria-hidden="true">›</span><div class="row-body">' +
        "<p>see <code>foo</code>bar<button>Copy</button>baz</p></div></div>",
    );
    const text = root.textContent ?? "";
    const at = (s: string) => text.indexOf(s);
    expect(landings(root, at("flag"), "w")).toEqual([at("(x)"), at("x)"), at(")"), at("see"), at("foobar"), at("baz"), at("baz") + 2]);
    expect(landings(root, at("baz") + 2, "b")).toEqual([at("baz"), at("foobar"), at("see"), at(")"), at("x)"), at("(x)"), at("flag")]);
  });

  it("a caret at an element boundary or a text node's end walks from the next character", () => {
    const root = listOf("<p>one</p><p>two three</p>");
    const [first, second] = Array.from(root.querySelectorAll("p"));
    const fromElement = runMotion(fakeSelection([{ node: second!, offset: 0 }]), { node: second!, offset: 0 }, "w", null, root);
    expect(fromElement.caret).toEqual({ node: second!.firstChild, offset: 4 });
    const fromEnd = runMotion(fakeSelection([{ node: first!.firstChild!, offset: 3 }]), { node: first!.firstChild!, offset: 3 }, "e", null, root);
    expect(fromEnd.caret).toEqual({ node: second!.firstChild, offset: 2 });
    const back = runMotion(fakeSelection([{ node: second!, offset: 0 }]), { node: second!, offset: 0 }, "b", null, root);
    expect(back.caret).toEqual({ node: first!.firstChild, offset: 0 });
  });

  it("text that is not rendered is never walked: a hidden span, a closed details' body", () => {
    const hidden = listOf("<p>one <span hidden>SECRET</span> two</p>");
    const text = hidden.textContent ?? "";
    expect(landings(hidden, 0, "w")).toEqual([text.indexOf("two"), text.indexOf("two") + 2]);
    expect(landings(hidden, text.indexOf("two"), "b")).toEqual([0]);
    expect(landings(hidden, 0, "e")).toEqual([2, text.indexOf("two") + 2]);

    const details = listOf("<details><summary>sum</summary><p>body</p></details><p>after</p>");
    const all = details.textContent ?? "";
    expect(landings(details, 0, "w")).toEqual([all.indexOf("after"), all.indexOf("after") + 4]);
    expect(landings(details, all.indexOf("after"), "b")).toEqual([0]);
    // An open one is walked.
    const open = listOf("<details open><summary>sum</summary><p>body</p></details>");
    expect(landings(open, 0, "w")).toEqual([3, 6]);
    // G and the end clamp agree: the last rendered character, never hidden text after it.
    const tail = listOf('<p>shown</p><p hidden>gone</p><details><summary>s</summary>body</details>');
    expect((lastSelectableCaret(tail)?.node as Text).data).toBe("s");
    expect(firstSelectableCaret(listOf("<p hidden>gone</p><p>here</p>"))?.node.textContent).toBe("here");
  });

  it("a combining mark split into the next text node by highlight.js stays with its base", () => {
    const line = "fn café() {}";
    // highlight.js's own rendering of that line as Rust: the title span ends before the accent.
    const root = listOf(
      '<pre><code class="hljs language-rust"><span class="hljs-keyword">fn</span> ' +
        '<span class="hljs-title function_">cafe</span>́() {}</code></pre>',
    );
    expect(code(root).textContent).toBe(line);
    expect(landings(root, 0, "w", code(root))).toEqual(fromVim([line], "", "1:3 1:9 1:12 1:13"));
    expect(landings(root, 0, "e", code(root))).toEqual(fromVim([line], "", "1:1 1:6 1:10 1:13"));
    expect(landings(root, line.length - 1, "b", code(root))).toEqual(fromVim([line], "", "1:12 1:9 1:3 1:0"));
  });

  it("an emoji grapheme is one character, and emoji are their own class, as in vim", () => {
    const zwj = "foo \u{1F469}‍\u{1F4BB} bar";
    const root = listOf(`<p>${zwj}</p>`);
    expect(landings(root, 0, "w")).toEqual(fromVim([zwj], "", "1:4 1:16 1:18"));
    expect(landings(root, 0, "e")).toEqual(fromVim([zwj], "", "1:2 1:4 1:18"));
    expect(landings(root, zwj.length - 1, "b")).toEqual(fromVim([zwj], "", "1:16 1:4 1:0"));

    const flags = "a!\u{1F600}b \u{1F1EF}\u{1F1F5}x";
    const second = listOf(`<p>${flags}</p>`);
    expect(landings(second, 0, "w")).toEqual(fromVim([flags], "", "1:1 1:2 1:6 1:8 1:16"));
    expect(landings(second, 0, "e")).toEqual(fromVim([flags], "", "1:1 1:2 1:6 1:8 1:16"));
  });

  it("the grapheme fallback (no Intl.Segmenter) splits as Intl.Segmenter does on marks, ZWJ, skin tones and flags", () => {
    const segmenter = new (Intl as unknown as { Segmenter: new (l?: string, o?: { granularity: string }) => { segment(s: string): Iterable<{ index: number }> } }).Segmenter(undefined, { granularity: "grapheme" });
    for (const text of ["cafe\u0301 x", "foo \u{1F469}\u200D\u{1F4BB} bar", "\u{1F44D}\u{1F3FD}!", "\u{1F1EF}\u{1F1F5}\u{1F1FA}\u{1F1F8}x", "a\uFE0Fb"]) {
      expect(fallbackGraphemeStarts(text), text).toEqual(Array.from(segmenter.segment(text), (s) => s.index));
    }
  });

  it("kana words keep vim's own classes: the prolonged sound mark, the voicing mark, the middle dot", () => {
    const line = "カーキ ひらがな゛ ア・イ x";
    const root = listOf(`<p>${line}</p>`);
    expect(landings(root, 0, "w")).toEqual(fromVim([line], "", "1:10 1:26 1:36"));
    expect(landings(root, 0, "e")).toEqual(fromVim([line], "", "1:6 1:22 1:32 1:36"));
  });

  it("b moves from a caret before a trailing textless element", () => {
    for (const html of ["<p>one<br></p>", "<p>one<span></span></p>"]) {
      const root = listOf(html);
      const p = root.querySelector("p")!;
      const back = runMotion(fakeSelection([{ node: p, offset: 1 }]), { node: p, offset: 1 }, "b", null, root);
      expect(back.caret, html).toEqual({ node: p.firstChild, offset: 0 });
    }
  });

  it("a textless block or a rule between two pieces of text separates words", () => {
    for (const html of ["<div>one<hr>two</div>", "<div>one<div></div>two</div>", "<div>one<p></p>two</div>"]) {
      const root = listOf(html);
      expect(landings(root, 0, "w"), html).toEqual([3, 5]);
      expect(landings(root, 5, "b"), html).toEqual([3, 0]);
    }
  });

  it("VISUAL extends only the cursor end, and a count repeats w", () => {
    const root = listOf(OWNER_HTML);
    const start = caretAt(code(root), 0);
    const model: VisualModel = { anchor: start, cursor: start, kind: "char", goalX: null };
    const after = repeatMotion(fakeSelection([start]), model, "w", 3, root);
    expect(after.anchor).toEqual(start);
    expect(globalOffset(code(root), after.cursor)).toBe(15); // let -> has_flag -> = -> |
    const toEnd = repeatMotion(fakeSelection([start]), model, "w", 999, root);
    expect(globalOffset(code(root), toEnd.cursor)).toBe(OWNER_LINE.length - 1);
  });
});
