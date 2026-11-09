// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import {
  assignLabels,
  clampStep,
  clipBox,
  controlsOf,
  conversationRows,
  currentStop,
  firstShownLine,
  hintTargets,
  hintVisible,
  LINK_LOOK_MAX_MS,
  LINK_LOOK_MIN_MS,
  linkClickNeedsLook,
  linkOpensAtOnce,
  nextControl,
  nextStop,
  permissionTarget,
  rowIndexOf,
  rowOf,
  stopOf,
  stopsIn,
  visibleBox,
  webLinks,
  webUrl,
} from "./nav";
import type { AnswerableItem } from "./nav";

afterEach(() => {
  document.body.innerHTML = "";
});

describe("assignLabels matches core::hint::assign_labels", () => {
  const one = "a s d j k l g h w e r u i o".split(" ");
  const two = `aa as ad aj ak al ag ah aw ae ar au ai ao
sa ss sd sj sk sl sg sh sw se sr su si so
da ds dd dj dk dl dg dh dw de dr du di do
ja js jd jj jk jl jg jh jw je jr ju ji jo
ka ks kd kj kk kl kg kh kw ke kr ku ki ko
la ls ld lj lk ll lg lh lw le lr lu li lo
ga gs gd gj gk gl gg gh gw ge gr gu gi go
ha hs hd hj hk hl hg hh hw he hr hu hi ho
wa ws wd wj wk wl wg wh ww we wr wu wi wo
ea es ed ej ek el eg eh ew ee er eu ei eo
ra rs rd rj rk rl rg rh rw re rr ru ri ro
ua us ud uj uk ul ug uh uw ue ur uu ui uo
ia is id ij ik il ig ih iw ie ir iu ii io
oa os od oj ok ol og oh ow oe or ou oi oo`.split(/\s+/);
  it.each([
    [0, []], [1, ["a"]], [14, one],
    [15, "aa as ad aj ak al ag ah aw ae ar au ai ao sa".split(" ")],
    [20, "aa as ad aj ak al ag ah aw ae ar au ai ao sa ss sd sj sk sl".split(" ")],
    [196, two], [197, [...two.map((label) => `a${label}`), "saa"]],
  ] as const)("%i labels", (count, expected) => {
    expect(assignLabels(count)).toEqual(expected);
  });
});

/** A panel-shaped document: two rows (the second a permission card whose reason box sits ABOVE its
 *  buttons, as the real card's does), a banner with nothing usable in it, and a status line. */
function panel(): HTMLElement {
  document.body.innerHTML = `
    <div id="root" tabindex="0">
      <div data-nav-stop="row" id="r0">prompt</div>
      <div data-nav-stop="row" id="r1">
        <input id="reason" data-nav-order="3" />
        <button id="approve" data-nav-order="1">Approve</button>
        <button id="deny" data-nav-order="2">Deny</button>
      </div>
      <div data-nav-stop="ended">no controls here</div>
      <div data-nav-stop="status"><span>working</span><button id="stop">Stop</button></div>
      <div data-nav-stop="handoff"><button disabled>Continue in a terminal…</button></div>
    </div>`;
  return document.getElementById("root")!;
}
const byId = (id: string) => document.getElementById(id)!;

describe("clampStep", () => {
  it("clamps at both ends instead of wrapping", () => {
    expect(clampStep(3, 2, 1)).toBe(2);
    expect(clampStep(3, 0, -1)).toBe(0);
    expect(clampStep(3, 1, 1)).toBe(2);
    expect(clampStep(0, 0, 1)).toBe(0);
  });
});

describe("stops", () => {
  it("keeps every row but skips a stop with no usable control", () => {
    const root = panel();
    // The "ended" banner has no control and the handoff button is disabled, so neither is a stop.
    expect(stopsIn(root).map((s) => s.getAttribute("data-nav-stop"))).toEqual(["row", "row", "status"]);
  });

  it("walks down from the last row onto the status line, and stays there", () => {
    const root = panel();
    root.focus();
    expect(nextStop(root, 1, 1)).toBe(byId("stop").parentElement);
    byId("stop").focus();
    expect(nextStop(root, 1, 1)).toBe(byId("stop").parentElement);
    // And back up to the row the cursor was on.
    expect(rowIndexOf(root, nextStop(root, 1, -1)!)).toBe(1);
  });

  it("finds the current stop from focus first, and from the cursor otherwise", () => {
    const root = panel();
    root.focus();
    expect(currentStop(root, 0)).toBe(byId("r0"));
    byId("deny").focus();
    expect(currentStop(root, 0)).toBe(byId("r1"));
    root.focus();
    expect(currentStop(root, null)).toBeNull();
  });

  it("lands on the first stop when nothing is current yet (a start screen)", () => {
    const root = panel();
    root.focus();
    expect(nextStop(root, null, 1)).toBe(byId("r0"));
    expect(nextStop(root, null, -1)).toBe(byId("r0"));
  });
});

describe("controls", () => {
  it("orders by data-nav-order, so Approve comes before the reason box that sits above it", () => {
    panel();
    expect(controlsOf(byId("r1")).map((c) => c.id)).toEqual(["approve", "deny", "reason"]);
  });

  it("walks a row's controls left and right, and h from the first returns to the row", () => {
    const root = panel();
    root.focus();
    expect(nextControl(byId("r1"), 1)).toBe(byId("approve"));
    byId("approve").focus();
    expect(nextControl(byId("r1"), 1)).toBe(byId("deny"));
    expect(nextControl(byId("r1"), -1)).toBe("stop");
    byId("reason").focus();
    expect(nextControl(byId("r1"), 1)).toBe(byId("reason"));
  });

  it("does not leave a non-row stop by h: there is no cursor there to go back to", () => {
    panel();
    const status = byId("stop").parentElement!;
    byId("stop").focus();
    expect(nextControl(status, -1)).toBe(byId("stop"));
  });

  it("treats a stop that is itself a control as its own only control", () => {
    document.body.innerHTML = `<button data-nav-stop="mode" id="m">Auto</button>`;
    expect(controlsOf(byId("m"))).toEqual([byId("m")]);
  });
});

describe("permissionTarget", () => {
  const items: AnswerableItem[] = [
    { kind: "other" },
    { kind: "tool", toolUseId: "toolu_1" },
    { kind: "permission", toolUseId: "toolu_1" },
    { kind: "tool", toolUseId: "toolu_2" },
    { kind: "other" },
    { kind: "permission", toolUseId: null },
  ];

  it("answers the card under the cursor", () => {
    expect(permissionTarget(items, 2)).toBe(2);
    expect(permissionTarget(items, 5)).toBe(5);
  });

  it("answers the card that gates the tool call under the cursor", () => {
    expect(permissionTarget(items, 1)).toBe(2);
  });

  it("answers nothing from a row that no card is about, rather than the nearest card", () => {
    expect(permissionTarget(items, 0)).toBeNull();
    // toolu_2 has no card of its own; the next card after it belongs to nobody in particular.
    expect(permissionTarget(items, 3)).toBeNull();
    expect(permissionTarget(items, 4)).toBeNull();
    expect(permissionTarget(items, 99)).toBeNull();
  });

  it("does not link a tool call whose id is empty", () => {
    expect(permissionTarget([{ kind: "tool", toolUseId: "" }, { kind: "permission", toolUseId: "" }], 0)).toBeNull();
  });

  it("never answers the only card from another row (there is no any-row exception)", () => {
    const card: AnswerableItem = { kind: "permission", toolUseId: null };
    const other: AnswerableItem = { kind: "other" };
    expect(permissionTarget([other, card, other], 0)).toBeNull();
    expect(permissionTarget([other, card, other], 2)).toBeNull();
    expect(permissionTarget([other, card, other], 1)).toBe(1);
  });
});

/* jsdom lays nothing out, so every rect is all zeros. `rect` gives one element a fake box; `hintDoc`
   builds a panel whose list viewport is y 0..100, with every element laid out explicitly. Rects are
   set per element rather than on `HTMLElement.prototype`, which would leak into every later test. */
function rect(el: HTMLElement, top: number, height: number, left = 0, width = 100) {
  el.getBoundingClientRect = () =>
    ({ top, bottom: top + height, left, right: left + width, width, height, x: left, y: top }) as DOMRect;
}

function hintDoc(): HTMLElement {
  document.body.innerHTML = `
    <div id="root" tabindex="0">
      <div class="message-list" id="list">
        <div data-nav-stop="row" id="r0">prompt</div>
        <div data-nav-stop="row" id="r1">
          <pre class="code-block" id="code"><code>ls</code></pre>
          <button id="approve" data-nav-order="1">Approve</button>
          <button id="deny" data-nav-order="2" disabled>Deny</button>
        </div>
        <div data-nav-stop="row" id="r2">below the fold</div>
      </div>
      <div data-nav-stop="status" id="status"><button id="stop">Stop</button></div>
    </div>`;
  rect(byId("root"), 0, 200);
  rect(byId("list"), 0, 100);
  rect(byId("r0"), 0, 20);
  rect(byId("r1"), 20, 60);
  rect(byId("code"), 25, 20);
  rect(byId("approve"), 50, 20);
  rect(byId("deny"), 50, 20, 50, 40);
  rect(byId("r2"), 150, 20); // entirely below the list's viewport
  rect(byId("stop"), 180, 20);
  return byId("root");
}

describe("hintTargets", () => {
  it("takes a row inside the list's viewport and leaves out one entirely below it", () => {
    const targets = hintTargets(hintDoc());
    const rows = targets.filter((t) => t.kind === "row");
    expect(rows).toEqual([
      { kind: "row", el: byId("r0"), rowIndex: 0 },
      { kind: "row", el: byId("r1"), rowIndex: 1 },
    ]);
  });

  it("judges a row by the list's viewport, not the root's, which still contains it", () => {
    // r2 (y 150..170) is inside the root (0..200) but below the list (0..100): only the list's
    // own scrollport decides what is on screen for anything inside it.
    const root = hintDoc();
    expect(hintTargets(root).some((t) => t.el === byId("r2"))).toBe(false);
    rect(byId("r2"), 90, 20); // partly inside: a sliver on screen still counts
    expect(hintTargets(root).some((t) => t.el === byId("r2"))).toBe(true);
  });

  it("puts a row's code block and then its controls right after it, in that order", () => {
    const targets = hintTargets(hintDoc());
    expect(targets.map((t) => `${t.kind}:${t.el.id}`)).toEqual([
      "row:r0",
      "row:r1",
      "code:code",
      "control:approve",
      "control:stop",
    ]);
    expect(targets[2]).toEqual({ kind: "code", el: byId("code"), rowIndex: 1 });
  });

  it("offers a problem row's link on a start screen, which is in no conversation row", () => {
    const root = hintDoc();
    root.insertAdjacentHTML(
      "beforeend",
      `<div class="row row-error"><div class="row-problem"><a id="get" href="https://code.claude.com/docs/en/quickstart">x</a></div></div>`,
    );
    rect(byId("get"), 160, 20);
    const targets = hintTargets(root);
    expect(targets.filter((t) => t.el === byId("get"))).toEqual([{ kind: "link", el: byId("get"), rowIndex: null }]);
    // An address the page cannot open is not offered.
    byId("get").setAttribute("href", "javascript:alert(1)");
    expect(hintTargets(root).some((t) => t.el === byId("get"))).toBe(false);
  });

  it("offers a problem link inside a conversation row once, as that row's link", () => {
    const root = hintDoc();
    byId("r0").innerHTML = `<div class="row-problem"><a id="inrow" href="https://code.claude.com/docs/en/quickstart">x</a></div>`;
    rect(byId("inrow"), 2, 10);
    const found = hintTargets(root).filter((t) => t.el === byId("inrow"));
    expect(found).toEqual([{ kind: "link", el: byId("inrow"), rowIndex: 0 }]);
  });

  it("offers the status line's Stop button, which is not a row", () => {
    const targets = hintTargets(hintDoc());
    expect(targets).toContainEqual({ kind: "control", el: byId("stop") });
  });

  it("never offers a disabled button", () => {
    const targets = hintTargets(hintDoc());
    expect(targets.some((t) => t.el === byId("deny"))).toBe(false);
  });

  it("leaves out a zero-sized element, which is how a hidden one measures", () => {
    const root = hintDoc();
    rect(byId("stop"), 180, 0); // full width, no height: its box still "intersects" the root
    expect(hintTargets(root).some((t) => t.el === byId("stop"))).toBe(false);
  });

  it("works on a start screen, which has no message list: its choice buttons are controls", () => {
    document.body.innerHTML = `
      <div id="root">
        <button data-nav-stop="choice" id="c0">New session</button>
        <button data-nav-stop="mode" id="m0">Auto</button>
      </div>`;
    rect(byId("root"), 0, 200);
    rect(byId("c0"), 10, 20);
    rect(byId("m0"), 40, 20);
    expect(hintTargets(byId("root"))).toEqual([
      { kind: "control", el: byId("c0") },
      { kind: "control", el: byId("m0") },
    ]);
  });

  it("judges a start-screen choice by its own scrolling list, not by the root", () => {
    // `.chooser-list` (the launch chooser) scrolls its own content (index.css): a choice
    // scrolled out of it is still inside the root, and must not get a label drawn over the controls
    // below the list.
    document.body.innerHTML = `
      <div id="root">
        <div class="chooser-list" id="list">
          <button data-nav-stop="choice" id="c0">New session</button>
          <button data-nav-stop="choice" id="c1">claude 1234</button>
          <button data-nav-stop="choice" id="c2">claude 5678</button>
        </div>
        <button data-nav-stop="mode" id="m0">Auto</button>
      </div>`;
    rect(byId("root"), 0, 400);
    rect(byId("list"), 0, 100);
    rect(byId("c0"), 10, 20);
    rect(byId("c1"), 50, 20);
    rect(byId("c2"), 150, 20); // below the list's viewport, inside the root's
    rect(byId("m0"), 300, 20);
    expect(hintTargets(byId("root")).map((t) => t.el.id)).toEqual(["c0", "c1", "m0"]);
  });

  it("judges an element by any ancestor that clips its overflow, not only the known lists", () => {
    document.body.innerHTML = `
      <div id="root">
        <div id="clip" style="overflow-y: auto">
          <button data-nav-stop="mode" id="in">In</button>
          <button data-nav-stop="mode" id="out">Out</button>
        </div>
      </div>`;
    rect(byId("root"), 0, 400);
    rect(byId("clip"), 0, 100);
    rect(byId("in"), 10, 20);
    rect(byId("out"), 150, 20);
    expect(hintTargets(byId("root")).map((t) => t.el.id)).toEqual(["in"]);
  });
});

/* A reply's own
   HTML sits inside its row, so anything in it that claims to be a stop is the row's content. */
describe("a stop inside another stop is content, not a stop", () => {
  /** Rows r0-r3 as the list's own children; r1 is a reply whose HTML carries two hidden "rows", a
   *  "notice" with a button, a link, and a HINT composer marker; r2 is a card. */
  function injected(): HTMLElement {
    document.body.innerHTML = `
      <div id="root" tabindex="0">
        <div class="message-list" id="list">
          <div data-nav-stop="row" id="r0">prompt</div>
          <div data-nav-stop="row" id="r1"><div class="row-body"><div>
            <p>reply <a href="#x" id="link">link</a></p>
            <div data-nav-stop="row" id="fake0" hidden></div>
            <div data-nav-stop="row" id="fake1" hidden></div>
            <div data-nav-stop="notice" id="fakeNotice"><button id="fakeButton">Approve</button></div>
            <div data-hint-composer id="fakeComposer">type here</div>
          </div></div></div>
          <div data-nav-stop="row" id="r2">
            <button id="approve" data-nav-order="1">Approve</button>
            <button id="deny" data-nav-order="2">Deny</button>
          </div>
          <div data-nav-stop="row" id="r3">tool</div>
        </div>
        <div data-hint-composer id="composer">composer</div>
      </div>`;
    return byId("root");
  }

  it("counts only the list's own rows, so index i is timeline item i", () => {
    const root = injected();
    expect(conversationRows(root).map((r) => r.id)).toEqual(["r0", "r1", "r2", "r3"]);
    expect(rowIndexOf(root, byId("r2"))).toBe(2);
    expect(rowIndexOf(root, byId("fake0"))).toBeNull();
    expect(currentStop(root, 2)).toBe(byId("r2"));
  });

  it("walks j/k over the real stops only", () => {
    const root = injected();
    expect(stopsIn(root).map((s) => s.id)).toEqual(["r0", "r1", "r2", "r3"]);
    root.focus();
    expect(nextStop(root, 1, 1)).toBe(byId("r2"));
    expect(nextStop(root, 2, -1)).toBe(byId("r1"));
  });

  it("puts an element inside a reply in the reply's row, whatever the reply claims", () => {
    const root = injected();
    expect(stopOf(root, byId("fakeButton"))).toBe(byId("r1"));
    expect(rowOf(root, byId("fake1"))).toBe(byId("r1"));
    expect(rowOf(root, byId("deny"))).toBe(byId("r2"));
    expect(stopOf(root, byId("composer"))).toBeNull();
    // Focus on a link in the reply: the reply's row holds the keys, not a "stop" it drew.
    byId("link").focus();
    expect(currentStop(root, 0)).toBe(byId("r1"));
  });

  it("gives HINT row targets their real row index, and the real composer", () => {
    const root = injected();
    for (const el of document.querySelectorAll<HTMLElement>("#root, #list, [data-nav-stop], button, [data-hint-composer]")) {
      rect(el, 0, 10);
    }
    rect(root, 0, 400);
    rect(byId("list"), 0, 400);
    const targets = hintTargets(root);
    expect(targets.filter((t) => t.kind === "row").map((t) => [t.el.id, (t as { rowIndex: number }).rowIndex])).toEqual([
      ["r0", 0],
      ["r1", 1],
      ["r2", 2],
      ["r3", 3],
    ]);
    expect(targets.filter((t) => t.kind === "composer").map((t) => t.el.id)).toEqual(["composer"]);
  });
});

/* The web links `gx` and the global HINT reach. An href counts only as the
   WHATWG parser (`new URL`, no base) reads it -- http(s), no userinfo, a plain host that is not the
   panel's own `eitri.invalid` -- and the NORMALIZED `href` is what a pick shows and `open_url` sends,
   never the spelling the reply wrote. Fixtures are built with `setAttribute` (and `String.raw` for a
   backslash), so what reaches the parser is exactly the string written here. */
describe("webUrl and webLinks", () => {
  /** A row holding one anchor per href (an entry of `null` is an anchor with no `href` at all). */
  function rowWithLinks(hrefs: (string | null)[]): HTMLElement {
    const row = document.createElement("div");
    row.setAttribute("data-nav-stop", "row");
    hrefs.forEach((href, i) => {
      const a = document.createElement("a");
      a.id = `a${i}`;
      if (href !== null) a.setAttribute("href", href);
      a.textContent = href ?? "no href";
      row.appendChild(a);
    });
    document.body.appendChild(row);
    return row;
  }

  it("keeps http(s) links in document order, once per normalized address, as that address", () => {
    const row = rowWithLinks([
      "https://EXAMPLE.com",
      "http://x.y:8080/p?q=1#frag",
      "https://example.com/",
      "https://example.com/other",
      "https://xn--r8jz45g.jp/",
    ]);
    expect(webLinks(row).map((l) => [l.el.id, l.url])).toEqual([
      ["a0", "https://example.com/"],
      ["a1", "http://x.y:8080/p?q=1#frag"],
      ["a3", "https://example.com/other"],
      ["a4", "https://xn--r8jz45g.jp/"],
    ]);
  });

  it("writes an internationalized host the way the browser will send it (punycode), never as typed", () => {
    // A Cyrillic "a" (U+0430) in place of the Latin one: the address that opens is the punycode one.
    const row = rowWithLinks(["https://\u0430pple.com/"]);
    expect(webLinks(row).map((l) => l.url)).toEqual(["https://xn--pple-43d.com/"]);
  });

  it("drops what is not a plain http(s) address a person could recognise", () => {
    const dropped: (string | null)[] = [
      "docs/a.md", // relative: `new URL` without a base throws
      "/x",
      "#frag",
      "https://eitri.invalid/x", // the panel's own base, where every relative link points
      "https://EITRI.INVALID/x",
      "https://%65itri.invalid/x", // percent-encoded, decoded to it by the parser
      String.raw`https:\\eitri.invalid\x`, // backslashes are slashes to the parser
      "https://eitri.invalid./x", // the same host with a trailing dot
      "https://user:pw@example.com/", // userinfo hides the real host behind a familiar one
      "https://example.com@evil.example/",
      "javascript:alert(1)",
      "mailto:a@b",
      "file:///etc/passwd",
      "https://a$b.com/", // legal to the parser, not a plain host: Rust refuses it, so no letter may offer it
      "https://my_host.x/",
      "https://[::1]/",
      "https://",
      "",
      String.raw`https://example.com/?a\b`, // a backslash left in the address
      null, // an anchor with no href
    ];
    expect(webLinks(rowWithLinks(dropped))).toEqual([]);
    for (const href of dropped) expect(webUrl(href), String(href)).toBeNull();
  });

  it("keeps a numeric address, a port and a query, which are plain enough", () => {
    expect(webUrl("http://127.0.0.1:3000/")).toBe("http://127.0.0.1:3000/");
    expect(webUrl("https://example.com/a?b=c&d=e#f")).toBe("https://example.com/a?b=c&d=e#f");
    expect(webUrl("https://example.com/@user")).toBe("https://example.com/@user");
    expect(webUrl("HTTPS://EXAMPLE.COM/A")).toBe("https://example.com/A");
  });

  it("looks only at anchors inside the row it is given", () => {
    const row = rowWithLinks(["https://example.com/in"]);
    const elsewhere = rowWithLinks(["https://example.com/out"]);
    expect(webLinks(row).map((l) => l.url)).toEqual(["https://example.com/in"]);
    expect(webLinks(elsewhere).map((l) => l.url)).toEqual(["https://example.com/out"]);
  });
});

describe("linkClickNeedsLook: a click on a reply link whose text is not its address shows the address first", () => {
  /** One row in a root whose box is y 0..200, holding one anchor with `text` and `href`, on screen. */
  function anchorWith(href: string, text: string) {
    document.body.innerHTML = `<div id="root"><div data-nav-stop="row" id="row"><a id="link"></a></div></div>`;
    const a = byId("link") as HTMLAnchorElement;
    a.setAttribute("href", href);
    a.textContent = text;
    rect(byId("root"), 0, 200);
    rect(a, 10, 20);
    return { a, root: byId("root") };
  }

  it("lets through a link whose visible text is its own address", () => {
    const { a, root } = anchorWith("https://good.example/x", "https://good.example/x");
    expect(linkClickNeedsLook(a, root, null, 0)).toBeNull();
  });

  it("shows the normalized address of a link that says something else", () => {
    for (const [href, text, url] of [
      ["https://evil.example/", "https://good.example/", "https://evil.example/"],
      ["https://EVIL.example/a", "docs", "https://evil.example/a"],
    ]) {
      const { a, root } = anchorWith(href, text);
      expect(linkClickNeedsLook(a, root, null, 0), href).toBe(url);
    }
    // A scheme-relative link is resolved against the document, as the click that follows it is.
    const { a, root } = anchorWith("//evil.example/x", "x");
    const resolved = new URL("//evil.example/x", document.baseURI).href;
    expect(resolved.endsWith("//evil.example/x")).toBe(true);
    expect(linkClickNeedsLook(a, root, null, 0)).toBe(resolved);
  });

  it("lets a second click on the same link through, but not a double-click or a late one", () => {
    const { a, root } = anchorWith("https://evil.example/", "https://good.example/");
    const shown = { el: a, row: rowOf(root, a), url: "https://evil.example/", at: 1000 };
    expect(linkClickNeedsLook(a, root, shown, 1000 + LINK_LOOK_MIN_MS)).toBeNull();
    expect(linkClickNeedsLook(a, root, shown, 1000 + LINK_LOOK_MAX_MS)).toBeNull();
    expect(linkClickNeedsLook(a, root, shown, 1000 + LINK_LOOK_MIN_MS - 1)).toBe("https://evil.example/");
    expect(linkClickNeedsLook(a, root, shown, 1000 + LINK_LOOK_MAX_MS + 1)).toBe("https://evil.example/");
    // With no row to go by, another anchor is shown afresh, and so is the same one now pointing elsewhere.
    const other = document.createElement("a");
    expect(linkClickNeedsLook(a, root, { ...shown, row: null, el: other }, 1500)).toBe("https://evil.example/");
    a.setAttribute("href", "https://worse.example/");
    expect(linkClickNeedsLook(a, root, shown, 1500)).toBe("https://worse.example/");
  });

  it("lets a second click through on a re-rendered anchor of the same row and address, not on another row's", () => {
    // A streaming reply's HTML is replaced on every chunk: the row stays, the anchor is a new element.
    document.body.innerHTML =
      `<div id="root"><div data-nav-stop="row" id="r1"><div id="body"><a id="l1" href="https://evil.example/">docs</a></div></div>` +
      `<div data-nav-stop="row" id="r2"><a id="l2" href="https://evil.example/">docs</a></div></div>`;
    const root = byId("root");
    rect(root, 0, 200);
    const first = byId("l1") as HTMLAnchorElement;
    rect(first, 10, 20);
    const shown = { el: first, row: rowOf(root, first), url: "https://evil.example/", at: 1000 };
    byId("body").innerHTML = `<a id="l1b" href="https://evil.example/">docs</a>`;
    const rebuilt = byId("l1b") as HTMLAnchorElement;
    rect(rebuilt, 10, 20);
    expect(rebuilt).not.toBe(first);
    expect(linkClickNeedsLook(rebuilt, root, shown, 2000)).toBeNull();
    // Same address in another row is a different link: shown again.
    const other = byId("l2") as HTMLAnchorElement;
    rect(other, 30, 40);
    expect(linkClickNeedsLook(other, root, shown, 2000)).toBe("https://evil.example/");
    // The same row with another address is shown afresh.
    rebuilt.setAttribute("href", "https://worse.example/");
    expect(linkClickNeedsLook(rebuilt, root, shown, 2000)).toBe("https://worse.example/");
  });

  it("holds a disguised mail address until a second click in the same row", () => {
    const href = "mailto:attacker@example.net?subject=hi";
    const { a, root } = anchorWith(href, "support@example.org");
    expect(linkClickNeedsLook(a, root, null, 1000)).toBe(href);
    const shown = { el: a, row: rowOf(root, a), url: href, at: 1000 };
    expect(linkClickNeedsLook(a, root, shown, 1100)).toBe(href);
    expect(linkClickNeedsLook(a, root, shown, 1400)).toBeNull();
    expect(linkClickNeedsLook(a, root, shown, 6001)).toBe(href);
    expect(linkClickNeedsLook(a, root, { ...shown, row: null, el: document.createElement("a") }, 1500)).toBe(href);
    a.innerHTML = "attacker@example.net<span>?subject=hi</span>";
    expect(linkClickNeedsLook(a, root, null, 1000)).toBe(href);
  });

  it("opens a plain mail address at once with or without its scheme in the text", () => {
    for (const text of ["a@example.com", " mailto:a@example.com "]) {
      const { a, root } = anchorWith("mailto:a@example.com", text);
      expect(linkClickNeedsLook(a, root, null, 0)).toBeNull();
      expect(webLinks(root)).toEqual([]);
    }
  });

  it("leaves a link that is not a web link to Rust's own check", () => {
    for (const href of ["javascript:alert(1)", "ftp://x.example/"]) {
      const { a, root } = anchorWith(href, "click me");
      expect(linkClickNeedsLook(a, root, null, 0), href).toBeNull();
    }
  });

  it("shows the address of a link nobody can see, even when its text is its address", () => {
    const { a, root } = anchorWith("https://evil.example/", "https://evil.example/");
    rect(a, 500, 20); // below the root's own box
    expect(linkClickNeedsLook(a, root, null, 0)).toBe("https://evil.example/");
  });
});

describe("linkOpensAtOnce", () => {
  /** One row in a root whose box is y 0..200, holding one anchor with `text` and `href`. */
  function linkWith(href: string, text: string, box: [number, number] | null = [10, 20]) {
    document.body.innerHTML = `<div id="root"><div data-nav-stop="row" id="row"><a id="link"></a></div></div>`;
    const a = byId("link") as HTMLAnchorElement;
    a.setAttribute("href", href);
    a.textContent = text;
    rect(byId("root"), 0, 200);
    if (box !== null) rect(a, box[0], box[1]);
    const [link] = webLinks(byId("row"));
    return { link, root: byId("root") };
  }

  it("opens with no pick only a link whose visible text is exactly the address it goes to", () => {
    for (const [href, text] of [
      ["https://example.com/a", "https://example.com/a"],
      ["https://example.com", "https://example.com"], // the browser's own `/` for a bare host
      ["https://example.com/a", "  https://example.com/a \n"],
    ]) {
      const { link, root } = linkWith(href, text);
      expect(linkOpensAtOnce(link, root), `${href} / ${JSON.stringify(text)}`).toBe(true);
    }
  });

  it("picks when the text says something else, or says the same thing another way", () => {
    for (const [href, text] of [
      ["https://evil.example/x", "docs"],
      ["https://example.com/x", "https://EXAMPLE.com/x"], // the address that opens is the lower-cased one
      ["https://\u0430pple.com/", "https://\u0430pple.com/"], // looks like apple.com, opens xn--pple-43d.com
      ["https://example.com/a", "https://example.com/a and more"],
      ["https://example.com/a", ""],
    ]) {
      const { link, root } = linkWith(href, text);
      expect(linkOpensAtOnce(link, root), `${href} / ${JSON.stringify(text)}`).toBe(false);
    }
  });

  it("picks a link nobody can see, even when its text is its address", () => {
    // DOMPurify keeps `hidden`: a reply can carry `<a hidden href=U>U</a>`, which must never open unseen.
    const { link, root } = linkWith("https://evil.example/", "https://evil.example/", null);
    expect(linkOpensAtOnce(link, root)).toBe(false);
    rect(link.el, 500, 20); // below the root's own box
    expect(linkOpensAtOnce(link, root)).toBe(false);
    rect(link.el, 10, 20);
    expect(linkOpensAtOnce(link, root)).toBe(true);
  });

  /** As `linkWith`, but the anchor's contents are `html`, as a reply's own inline HTML can make them. */
  function linkWithHtml(href: string, html: string) {
    document.body.innerHTML = `<div id="root"><div data-nav-stop="row" id="row"><a id="link"></a></div></div>`;
    const a = byId("link") as HTMLAnchorElement;
    a.setAttribute("href", href);
    a.innerHTML = html;
    rect(byId("root"), 0, 200);
    rect(a, 10, 20);
    const [link] = webLinks(byId("row"));
    return { link, root: byId("root") };
  }

  it("picks a link whose text hides part of its address in an element a reader cannot see", () => {
    // DOMPurify keeps `hidden` and inline HTML inside link text. `textContent` counts the hidden span, so
    // it equals the real address while the reader is shown `https://good.example/`.
    const { link, root } = linkWithHtml(
      "https://good.example.evil.example/",
      "https://good.example<span hidden>.evil.example</span>/",
    );
    expect(link.el.textContent).toBe(link.url);
    expect(linkOpensAtOnce(link, root)).toBe(false);
  });

  it("picks a link whose text is wrapped in any element: only a run of plain text is what it reads", () => {
    for (const html of [
      "<b>https://example.com/a</b>",
      "<code>https://example.com/a</code>",
      "https://example.com/<span>a</span>",
      "<span hidden></span>https://example.com/a",
    ]) {
      const { link, root } = linkWithHtml("https://example.com/a", html);
      expect(link.el.textContent, html).toBe("https://example.com/a");
      expect(linkOpensAtOnce(link, root), html).toBe(false);
    }
  });

  it("still opens at once a link whose only contents are its text, comments aside (they render nothing)", () => {
    const { link, root } = linkWithHtml("https://example.com/a", "https://example.com/a<!-- no element -->");
    expect(linkOpensAtOnce(link, root)).toBe(true);
  });

  it("picks a link nobody can see, even when its text is its address", () => {
    // DOMPurify keeps `hidden`: a reply can carry `<a hidden href=U>U</a>`, which must never open unseen.
    const { link, root } = linkWith("https://evil.example/", "https://evil.example/", null);
    expect(linkOpensAtOnce(link, root)).toBe(false);
    rect(link.el, 500, 20); // below the root's own box
    expect(linkOpensAtOnce(link, root)).toBe(false);
    rect(link.el, 10, 20);
    expect(linkOpensAtOnce(link, root)).toBe(true);
  });
});

describe("firstShownLine (where a wrapped link's HINT label sits)", () => {
  const line = (top: number, height: number, left = 0, width = 100) =>
    ({ top, bottom: top + height, left, right: left + width, width, height, x: left, y: top }) as DOMRect;
  /** A root 0..200 holding a list 0..100 (which clips) holding an anchor whose line boxes are `lines`. */
  function wrapped(lines: DOMRect[]) {
    document.body.innerHTML = `<div id="root"><div class="message-list" id="list"><div data-nav-stop="row" id="row"><a id="link" href="https://example.com/">x</a></div></div></div>`;
    rect(byId("root"), 0, 200);
    rect(byId("list"), 0, 100);
    const a = byId("link");
    const top = Math.min(...lines.map((l) => l.top));
    const bottom = Math.max(...lines.map((l) => l.bottom));
    rect(a, top, bottom - top);
    a.getClientRects = () => lines as unknown as DOMRectList;
    return { a, root: byId("root") };
  }

  it("is the first line box when it is on screen", () => {
    const first = line(40, 10, 30);
    const { a, root } = wrapped([first, line(50, 10, 0, 60)]);
    expect(firstShownLine(a, root)).toBe(first);
  });

  it("skips a first line scrolled above the list and takes the first one still in it", () => {
    const second = line(-5, 10, 0, 60); // straddles the list's top edge
    const { a, root } = wrapped([line(-15, 10, 30), second, line(5, 10, 0, 40)]);
    expect(firstShownLine(a, root)).toBe(second);
    const off = line(-20, 10, 30);
    const on = line(0, 10, 0, 60);
    const again = wrapped([off, on]);
    expect(firstShownLine(again.a, again.root)).toBe(on);
  });

  it("skips a line the list clips away below it, which the root itself would still contain", () => {
    const shown = line(90, 10, 30);
    const { a, root } = wrapped([shown, line(110, 10, 0, 60)]);
    expect(firstShownLine(a, root)).toBe(shown);
    const gone = line(120, 10);
    const other = wrapped([gone, line(130, 10)]);
    // No line is on screen (the bounding box would not be either): the bounding box is the fallback.
    expect(firstShownLine(other.a, other.root)).toEqual(other.a.getBoundingClientRect());
  });

  it("falls back to the bounding box where the browser reports no line boxes", () => {
    const { a, root } = wrapped([line(40, 10)]);
    a.getClientRects = () => [] as unknown as DOMRectList;
    const box = a.getBoundingClientRect();
    expect(firstShownLine(a, root)).toEqual(box);
  });
});

describe("hintTargets: web links", () => {
  /** `hintDoc()` with `html` put in row r1 just after its code block, each anchor boxed inside the list. */
  function withLinks(html: string, boxes: Record<string, [number, number]>): HTMLElement {
    const root = hintDoc();
    byId("code").insertAdjacentHTML("afterend", html);
    for (const [id, [top, height]] of Object.entries(boxes)) rect(byId(id), top, height);
    return root;
  }

  it("puts a link after its row's code blocks and before its controls", () => {
    const root = withLinks('<p>look at <a id="link" href="https://example.com/a">https://example.com/a</a></p>', {
      link: [50, 10],
    });
    const targets = hintTargets(root);
    expect(targets.map((t) => `${t.kind}:${t.el.id}`)).toEqual([
      "row:r0",
      "row:r1",
      "code:code",
      "link:link",
      "control:approve",
      "control:stop",
    ]);
    expect(targets[3]).toEqual({ kind: "link", el: byId("link"), rowIndex: 1 });
  });

  it("lists it after the code block even when the reply wrote it before", () => {
    const root = hintDoc();
    byId("code").insertAdjacentHTML("beforebegin", '<p><a id="first" href="https://example.com/a">a</a></p>');
    rect(byId("first"), 22, 2);
    expect(hintTargets(root).map((t) => `${t.kind}:${t.el.id}`)).toEqual([
      "row:r0",
      "row:r1",
      "code:code",
      "link:first",
      "control:approve",
      "control:stop",
    ]);
  });

  it("offers every visible copy of an address: the one scrolled away does not take the label from it", () => {
    const root = withLinks(
      '<a id="off" href="https://example.com/a">a</a> <a id="on" href="https://example.com/a">a</a>',
      { off: [150, 10], on: [30, 10] }, // `off` is below the list's viewport (0..100)
    );
    expect(hintTargets(root).filter((t) => t.kind === "link").map((t) => t.el.id)).toEqual(["on"]);
  });

  it("never offers a relative link, the panel's own address, or a script", () => {
    const root = withLinks(
      [
        '<a id="rel" href="docs/a.md">rel</a>',
        '<a id="own" href="https://eitri.invalid/x">own</a>',
        '<a id="js" href="javascript:alert(1)">js</a>',
        '<a id="user" href="https://user:pw@example.com/">user</a>',
        '<a id="plain" href="https://example.com/ok">ok</a>',
      ].join(" "),
      { rel: [30, 5], own: [35, 5], js: [40, 5], user: [45, 5], plain: [50, 5] },
    );
    expect(hintTargets(root).filter((t) => t.kind === "link").map((t) => t.el.id)).toEqual(["plain"]);
  });

  it("does not offer a link that is not inside a conversation row (a stop that is not one has no reply text)", () => {
    document.body.innerHTML = `
      <div id="root"><div class="message-list" id="list"><div data-nav-stop="row" id="r0">prompt</div></div>
      <div data-nav-stop="status" id="status"><a id="stray" href="https://example.com/a">x</a><button id="stop">Stop</button></div></div>`;
    rect(byId("root"), 0, 200);
    rect(byId("list"), 0, 100);
    rect(byId("r0"), 0, 20);
    rect(byId("status"), 150, 40);
    rect(byId("stray"), 150, 10);
    rect(byId("stop"), 160, 20);
    expect(hintTargets(byId("root")).map((t) => `${t.kind}:${t.el.id}`)).toEqual(["row:r0", "control:stop"]);
  });
});

/* What a HINT label is placed on -- the part of its target that shows. `clipBox` is
   the intersection of the root and every clipping ancestor between them; `visibleBox` is the target's
   own box inside it; `hintVisible` is exactly "`visibleBox` is not null", so what gets a label and
   where it goes cannot disagree. */
describe("clipBox and visibleBox", () => {
  const edges = (b: { top: number; bottom: number; left: number; right: number } | null) =>
    b === null ? null : { top: b.top, bottom: b.bottom, left: b.left, right: b.right };

  it("a target fully inside the list: its own box, clipped by the list", () => {
    const root = hintDoc();
    expect(edges(clipBox(byId("code"), root))).toEqual({ top: 0, bottom: 100, left: 0, right: 100 });
    expect(edges(visibleBox(byId("code"), root))).toEqual({ top: 25, bottom: 45, left: 0, right: 100 });
    expect(hintVisible(byId("code"), root)).toBe(true);
  });
  it("a target whose top is scrolled out of the list: the part below the list's top", () => {
    const root = hintDoc();
    rect(byId("code"), -30, 50);
    expect(edges(visibleBox(byId("code"), root))).toEqual({ top: 0, bottom: 20, left: 0, right: 100 });
    expect(hintVisible(byId("code"), root)).toBe(true);
  });
  it("a nested scroller clips too: the clip box is the tool result's box, not the list", () => {
    const root = hintDoc();
    byId("r1").insertAdjacentHTML("beforeend", '<div id="box" style="overflow-y: auto"><pre id="inner">x</pre></div>');
    rect(byId("box"), 50, 30);
    rect(byId("inner"), 40, 100);
    expect(edges(clipBox(byId("inner"), root))).toEqual({ top: 50, bottom: 80, left: 0, right: 100 });
    expect(edges(visibleBox(byId("inner"), root))).toEqual({ top: 50, bottom: 80, left: 0, right: 100 });
    expect(hintVisible(byId("inner"), root)).toBe(true);
    rect(byId("inner"), 90, 5); // inside the list, outside its own scroller
    expect(visibleBox(byId("inner"), root)).toBeNull();
    expect(hintVisible(byId("inner"), root)).toBe(false);
  });
  it("a target entirely outside the list: no box, and not visible", () => {
    const root = hintDoc();
    expect(visibleBox(byId("r2"), root)).toBeNull();
    expect(hintVisible(byId("r2"), root)).toBe(false);
  });
  it("a zero-sized target: no box, and not visible", () => {
    const root = hintDoc();
    rect(byId("stop"), 180, 0);
    expect(visibleBox(byId("stop"), root)).toBeNull();
    expect(hintVisible(byId("stop"), root)).toBe(false);
  });
});

describe("the stop lookup is read once and kept until the stops change", () => {
  function list(): HTMLElement {
    document.body.innerHTML = `
      <div id="root">
        <div data-nav-stop="row" id="a"><p id="text">hello</p></div>
        <div data-nav-stop="row" id="b"></div>
      </div>`;
    return document.getElementById("root")!;
  }

  it("hands back the same rows while nothing that makes a stop changed: text, or a reply re-rendered with no stop in it", () => {
    const root = list();
    const first = conversationRows(root);
    byId("text").textContent = "hello again";
    byId("a").insertAdjacentHTML("beforeend", "<p>more <b>reply</b></p>");
    expect(conversationRows(root)).toBe(first);
    expect(first.map((r) => r.id)).toEqual(["a", "b"]);
  });

  it("sees a row added, removed or renamed at once, in the same task as the change", () => {
    const root = list();
    conversationRows(root);
    root.insertAdjacentHTML("beforeend", '<div data-nav-stop="row" id="c"></div>');
    expect(conversationRows(root).map((r) => r.id)).toEqual(["a", "b", "c"]);
    byId("a").remove();
    expect(conversationRows(root).map((r) => r.id)).toEqual(["b", "c"]);
    byId("b").setAttribute("data-nav-stop", "ended");
    expect(conversationRows(root).map((r) => r.id)).toEqual(["c"]);
  });

  it("still never counts a stop a reply's own HTML put inside a row, however it arrived", () => {
    const root = list();
    conversationRows(root);
    byId("a").insertAdjacentHTML("beforeend", '<div data-nav-stop="row" id="fake"></div>');
    expect(conversationRows(root).map((r) => r.id)).toEqual(["a", "b"]);
    expect(rowIndexOf(root, byId("b"))).toBe(1);
  });

  it("is frozen: a caller cannot change what the next one reads", () => {
    const root = list();
    expect(Object.isFrozen(conversationRows(root))).toBe(true);
  });
});
