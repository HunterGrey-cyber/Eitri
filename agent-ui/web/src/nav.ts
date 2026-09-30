/**
 * Keyboard reach for every control in the panel, by structure rather than by geometry
 * (2026-09-19). The owner wanted every button reachable "直观，而且符合直觉", and chose `hjkl` over
 * geometric spatial navigation after the trade-off was laid out: the panel is a vertical document,
 * so its structure is known exactly, whereas "the nearest button in that direction" would have to
 * be inferred from pixels and would surprise.
 *
 * The model:
 *
 * - A **stop** is anything carrying `data-nav-stop`: a conversation row (`"row"`), a banner, the
 *   terminal-handoff area; on the start screen, each choice row and each mode button. The bottom
 *   band is not one since C1c (v1 spec §3.4): `j` on the last stop goes nowhere, and the session's
 *   details are `<leader>i`, `prefix i` or a click on the band. `j`/`k` walk stops in document
 *   order, top to bottom. A stop with no usable control is
 *   skipped, except a row: rows carry the conversation cursor and are worth landing on for
 *   `Enter`/`y` even when they hold no button. **A stop inside another stop is content, not a
 *   stop** (`isOwnStop` below): that is what keeps a model reply's own HTML from adding rows.
 * - A stop's **controls** are its enabled buttons, inputs, textareas and radios. `h`/`l` walk them
 *   left to right. Order is document order unless a control sets `data-nav-order`, which the
 *   permission card uses to put Approve before Deny before the reason box: the most frequent action
 *   first, one keypress away.
 * - The **selected control is simply the focused one.** No parallel selection state that could
 *   disagree with DOM focus: `Tab`, a click and `h`/`l` all move the same thing, and `Enter`/`Space`
 *   activate it natively.
 */

export const STOP_ATTR = "data-nav-stop";

/** The label alphabet `neovibe_core::hint` uses for the window-wide `f` HINT (no `f` itself, since
 *  that key starts the HINT). N2's `gf` path picker (`components/PathPick.tsx`) reuses it for its
 *  own letter-per-path footer list, and so does `gx`'s link picker (`components/LinkPick.tsx`, v1 picks,
 *  Task 8) -- the panel has never needed its own copy of this until now. */
export const HINT_ALPHABET = "asdjklghweruio";

/** R6 (v1 picks, Task 8): a web link as `web_url` (`shell/src/agent_panel.rs`) will re-check it -- `http(s)`
 *  by the WHATWG parser with no base (the parser the browser opens it with, so `https://%6eeovibe.invalid/x`
 *  and `https:\\neovibe.invalid\x` are what they really are), no userinfo, a plain `[a-z0-9.-]` host with
 *  no trailing dot that is not the panel's own `neovibe.invalid` (where every relative link resolves), and
 *  no backslash left anywhere in the address. Returns the normalized `href` -- what a pick shows and what
 *  `open_url` sends, never the spelling the reply wrote -- or `null`. The host rules are Rust's, restated:
 *  the parser alone keeps `neovibe.invalid.`, `a$b.com`, `my_host.x`, `[::1]` and a backslash in a query, and
 *  Rust refuses all of them, so a letter offered for one could only fail. */
export function webUrl(href: string | null): string | null {
  let url: URL;
  try {
    url = new URL(href ?? "");
  } catch {
    return null;
  }
  if ((url.protocol !== "https:" && url.protocol !== "http:") || url.username !== "" || url.password !== "") return null;
  const host = url.hostname;
  if (!/^[a-z0-9.-]+$/.test(host) || host.endsWith(".") || host === "neovibe.invalid" || url.href.includes("\\")) return null;
  return url.href;
}

/** R6: the links `gx` and HINT reach inside `row`, in document order, once per normalized address (the first
 *  anchor that names it), each with that address (`webUrl`) -- what the pick shows and `open_url` sends. */
export function webLinks(row: HTMLElement): { el: HTMLAnchorElement; url: string }[] {
  const seen = new Set<string>();
  const out: { el: HTMLAnchorElement; url: string }[] = [];
  for (const el of row.querySelectorAll<HTMLAnchorElement>("a[href]")) {
    const url = webUrl(el.getAttribute("href"));
    if (url === null || seen.has(url)) continue;
    seen.add(url);
    out.push({ el, url });
  }
  return out;
}

/** R6, Review Focus 3: `gx` opens with no pick only a link a reader can see goes exactly where its text says.
 *  Compared with the NORMALIZED address, not the raw `href`: an anchor whose text and `href` both spell
 *  `https://apple.com/` with a Cyrillic "a" (U+0430) reads as apple.com and opens `xn--pple-43d.com`, which
 *  only the pick shows. A bare host's own `/` is allowed for (`https://example.com` opens
 *  `https://example.com/`); an upper-case host or a `www.` autolink picks, which costs a keypress and never
 *  opens the wrong page. An anchor nobody can see (a reply's `hidden`, a closed `<details>`, scrolled out of
 *  the list) never opens with no pick.
 *
 *  **And only a link whose contents are plain text** (fix round 1, review and Codex): `textContent` counts
 *  what an element inside the anchor does not show -- the sanitizer keeps `hidden` and inline HTML in link
 *  text, so `<a href=".../good.example.evil.example/">https://good.example<span hidden>.evil.example</span>/</a>`
 *  READS `https://good.example/` and has a `textContent` equal to its real address. Text nodes are always
 *  drawn when their anchor is (a reply's `class` and `style` are stripped, so nothing it writes hides a text
 *  node by itself), so `childElementCount === 0` is exactly "what is drawn is `textContent`" -- marked's
 *  autolink and a bare-URL link both are. A comment node renders nothing and is not in `textContent`. A link
 *  wrapping `<b>` or `<code>` picks, which costs one keypress and never opens an address unseen. */
export function linkOpensAtOnce(link: { el: HTMLAnchorElement; url: string }, root: HTMLElement): boolean {
  if (link.el.childElementCount !== 0) return false;
  const text = link.el.textContent?.trim() ?? "";
  return (text === link.url || `${text}/` === link.url) && hintVisible(link.el, root);
}

/** Whether `el` is, or sits inside, a control a key press ACTIVATES by default -- a `<button>`, a
 *  `<summary>`, a link, or anything wearing `role="button"`.
 *
 *  This is the second half of "the panel does not own every keystroke inside its own subtree", and
 *  it is not a theoretical one. Enter's default action on a focused `<button>` *is* its activation
 *  click; there is no separate click event to let through. So a keydown handler that claims Enter
 *  from any non-editable target and calls `preventDefault()` does not merely also do something
 *  else -- it silently DELETES the button's activation.
 *
 *  **Observed on an installed build before it was fixed** (2026-09-18, the owner: "approve 现在没有
 *  键位能够触及好像"). Tab-to-Approve then Enter is the only keyboard route to a permission
 *  decision today -- the spec's `a`/`d` allow/deny keys belong to a later sub-project and are
 *  deliberately not in this keyboard skeleton -- and this panel's own `onKeyDown` had taken it
 *  away. Before this branch nothing listened for Enter at all, so the route worked; the branch
 *  created the hole and the fix restores exactly what was there, rather than pulling the later
 *  sub-project's keys forward to paper over it.
 *
 *  The same applies to Space on a button and to Enter on `<summary>` (the generic tool card's
 *  disclosure, `toolRegistry.tsx`), which is why this is a selector over activatable controls and
 *  not a special case for Enter. `closest`, not a tag check: a real click target is usually a
 *  `<strong>`/`<span>` INSIDE the button, and focus-then-Enter dispatches the keydown at the
 *  button itself -- both have to bail.
 *
 *  Moved here from `App.tsx` (fix round 1, panel round 2 plan Task 12+13, reviewer finding: Space
 *  never reached the leader engine on the empty tab's dashboard, because the leader engine lived
 *  only in `App.tsx` and could not be imported into `EmptyTab.tsx` without a circular import back
 *  into the component that renders it). `nav.ts` already sits below both without importing either. */
export function isActivatableControl(el: EventTarget | null): el is HTMLElement {
  if (!(el instanceof HTMLElement)) return false;
  return el.closest("button, summary, a[href], [role=button]") !== null;
}

const CONTROL_SELECTOR =
  'button:not(:disabled), input:not(:disabled), textarea:not(:disabled), [role="radio"]:not([aria-disabled="true"])';

/** Moves `index` by `delta` within `[min, length - 1]`, clamped rather than wrapped. Holding `j` at
 *  the bottom of a list must not jump to the top: that loses your place. */
export function clampStep(length: number, index: number, delta: number, min = 0): number {
  if (length === 0) return min;
  return Math.max(min, Math.min(length - 1, index + delta));
}

/** The enabled controls inside `stop`, left to right. A stop that is itself a control (a start
 *  screen mode button, a choice row) is its own only control. */
export function controlsOf(stop: HTMLElement): HTMLElement[] {
  if (stop.matches(CONTROL_SELECTOR)) return [stop];
  const found = Array.from(stop.querySelectorAll<HTMLElement>(CONTROL_SELECTOR));
  const order = (el: HTMLElement) => Number(el.dataset.navOrder ?? Number.POSITIVE_INFINITY);
  // A stable sort, so controls without `data-nav-order` keep document order among themselves.
  return found
    .map((el, i) => ({ el, i }))
    .sort((a, b) => order(a.el) - order(b.el) || a.i - b.i)
    .map(({ el }) => el);
}

/**
 * **A stop is never inside another stop** (v1 hardening, ruling R2). An element carrying
 * `data-nav-stop` inside a stop is that stop's CONTENT, not a stop of its own.
 *
 * The panel's own stops never nest: every conversation row is a direct child of `.message-list`,
 * and the banners, the activity line, the handoff area and the empty tab's items sit beside them.
 * What can nest is model content: an assistant reply is rendered as HTML inside its row
 * (`MessageList`'s `renderMarkdown`, the one raw-HTML sink in this panel), so a reply carrying
 * `<div data-nav-stop="row" hidden>` put extra "rows" in a subtree-wide `querySelectorAll`. Every
 * row below them then sat at a DOM index that no longer matched its timeline index, and `a` on the
 * card the cursor visibly showed answered the card above it (the verdict's probe: `echo` shown,
 * `rm -rf` approved). Whatever the sanitizer strips, a reply can only ever add elements INSIDE its
 * own row, so this rule excludes all of them by structure.
 *
 * The walk stops at `root`: a stop is judged only by its ancestors inside the region being walked.
 */
function isOwnStop(root: HTMLElement, el: HTMLElement): boolean {
  for (let a = el.parentElement; a !== null && a !== root; a = a.parentElement) {
    if (a.hasAttribute(STOP_ATTR)) return false;
  }
  return true;
}

/** Every stop under `root`, in document order, whether or not it is worth landing on. */
function ownStops(root: HTMLElement): HTMLElement[] {
  return Array.from(root.querySelectorAll<HTMLElement>(`[${STOP_ATTR}]`)).filter((stop) => isOwnStop(root, stop));
}

/** The conversation rows under `root`, in document order: index `i` here is timeline index `i`.
 *  Only real rows -- a `data-nav-stop="row"` inside another stop (a reply's own HTML) is never
 *  one. Every site that turns the cursor into an element goes through this (ruling R2). */
export function conversationRows(root: HTMLElement): HTMLElement[] {
  return ownStops(root).filter((stop) => stop.getAttribute(STOP_ATTR) === "row");
}

/** The stop `el` sits in (or is), or `null`: the OUTERMOST `data-nav-stop` between `el` and
 *  `root`, so an element inside a reply's own HTML belongs to the reply's row, never to a "stop"
 *  the reply drew. */
export function stopOf(root: HTMLElement, el: Element): HTMLElement | null {
  let found: HTMLElement | null = null;
  for (let a: Element | null = el; a !== null && a !== root; a = a.parentElement) {
    if (a instanceof HTMLElement && a.hasAttribute(STOP_ATTR)) found = a;
  }
  return found;
}

/** The conversation row `el` sits in (or is), or `null` when it is in none. */
export function rowOf(root: HTMLElement, el: Element): HTMLElement | null {
  const stop = stopOf(root, el);
  return stop !== null && stop.getAttribute(STOP_ATTR) === "row" ? stop : null;
}

/** Every stop under `root` worth landing on, in document order. */
export function stopsIn(root: HTMLElement): HTMLElement[] {
  return ownStops(root).filter((stop) => stop.getAttribute(STOP_ATTR) === "row" || controlsOf(stop).length > 0);
}

/** The stop that currently holds the keyboard: the one containing the focused element, if focus
 *  is inside one; otherwise the conversation row at `cursor`, if there is one. `null` on a start
 *  screen before anything has been selected.
 *
 *  `rows` is `conversationRows(root)` when the caller already holds it (`countedStop`), so the row
 *  lookup costs no second scan of the whole tree; every other caller leaves it out. */
export function currentStop(
  root: HTMLElement,
  cursor: number | null,
  rows?: readonly HTMLElement[],
): HTMLElement | null {
  const active = document.activeElement;
  if (active instanceof HTMLElement && active !== root && root.contains(active)) {
    const stop = stopOf(root, active);
    if (stop !== null) return stop;
  }
  if (cursor === null) return null;
  return (rows ?? conversationRows(root))[cursor] ?? null;
}

/** Where `j` (+1) or `k` (-1) goes from the current stop. With nothing current (a start screen
 *  nobody has moved on yet), both land on the first stop.
 *
 *  `stops` defaults to a fresh `stopsIn(root)` (a DOM query). A caller that already holds that list
 *  may pass it; a counted repeat no longer calls this per step at all (`countedStop`, below, whose
 *  doc says why passing `stops` alone did not bound it). */
export function nextStop(
  root: HTMLElement,
  cursor: number | null,
  delta: 1 | -1,
  stops: HTMLElement[] = stopsIn(root),
): HTMLElement | null {
  if (stops.length === 0) return null;
  const current = currentStop(root, cursor);
  const index = current === null ? -1 : stops.indexOf(current);
  if (index === -1) return stops[0];
  return stops[clampStep(stops.length, index, delta)];
}

/** The index of `stop` among the conversation rows (`conversationRows`), or `null` if it is not
 *  one -- including an element that merely carries `data-nav-stop="row"` inside another stop. */
export function rowIndexOf(root: HTMLElement, stop: HTMLElement): number | null {
  if (stop.getAttribute(STOP_ATTR) !== "row") return null;
  const index = conversationRows(root).indexOf(stop);
  return index === -1 ? null : index;
}

/** Where a counted `j`/`k` (`3j`, `1000j`) lands, and where it started. */
export interface CountedLanding {
  /** The stop the walk ends on. */
  stop: HTMLElement;
  /** Its index among the conversation rows, or `null` when it is not a row (a banner, Stop). */
  row: number | null;
  /** The stop the walk started from (`currentStop` before the first step), or `null`. */
  from: HTMLElement | null;
}

/**
 * A counted `j` (+1) or `k` (-1): `times` steps of `nextStop`, each from the stop the previous step
 * landed on. The walk ends early on a stop that is not a conversation row (a banner, Stop while a
 * turn runs) -- the same place a single `j`/`k` onto it would leave the keys -- and at a boundary,
 * where a step makes no progress (`clampStep` clamps rather than failing there). `null` when the
 * region has no stop at all.
 *
 * **One scan of the tree, whatever `times` is** (v1 audit R4, P2-A4 round 2). The first fix fetched
 * `stopsIn` once but still called `nextStop` per step, and each step went back to the tree twice:
 * `nextStop` -> `currentStop` -> `conversationRows`, then `rowIndexOf` -> `conversationRows`.
 * `1000j` from the first of 1,001 rows was 2,001 full-tree queries and about 2.3 s in jsdom (the
 * Codex whole-branch review). Here the stops, the rows and each row's index are read once and the
 * walk moves along the index, so a step costs no DOM work at all.
 *
 * Every step after the first starts from where the walk is, never from `document.activeElement`.
 * Per-step `currentStop` re-read focus each time, so with focus on a control inside a stop (after
 * `l` onto a card's Approve) every step re-started from that control's stop and `3j` moved once.
 */
export function countedStop(
  root: HTMLElement,
  cursor: number | null,
  delta: 1 | -1,
  times: number,
): CountedLanding | null {
  const own = ownStops(root);
  const rows = own.filter((stop) => stop.getAttribute(STOP_ATTR) === "row");
  const rowIndex = new Map(rows.map((row, i) => [row, i] as const));
  const stops = own.filter((stop) => rowIndex.has(stop) || controlsOf(stop).length > 0);
  if (stops.length === 0) return null;
  const from = currentStop(root, cursor, rows);
  let index = from === null ? -1 : stops.indexOf(from);
  let landed: HTMLElement | null = null;
  for (let n = 0; n < times; n++) {
    const next = index === -1 ? 0 : clampStep(stops.length, index, delta);
    // No progress: already at the boundary. The first step still lands (on the stop it started
    // from), which is what tells the caller `j` could not move.
    if (landed !== null && next === index) break;
    landed = stops[next];
    index = next;
    if (!rowIndex.has(landed)) break;
  }
  if (landed === null) return null;
  return { stop: landed, row: rowIndex.get(landed) ?? null, from };
}

/**
 * Where `h` (-1) or `l` (+1) goes inside `stop`. Returns the control to focus, or `"stop"` to hand
 * focus back to the stop itself.
 *
 * Only a row can be "back to the stop", because only a row has a cursor to return to: from its
 * first control, `h` leaves the controls and the row cursor holds the keys again. Every other stop
 * is reached by focusing a control, so its first control is as far left as it goes.
 */
export function nextControl(stop: HTMLElement, delta: 1 | -1): HTMLElement | "stop" | null {
  const controls = controlsOf(stop);
  if (controls.length === 0) return null;
  const active = document.activeElement;
  const index = active instanceof HTMLElement ? controls.indexOf(active) : -1;
  const isRow = stop.getAttribute(STOP_ATTR) === "row";
  if (index === -1) return delta === 1 ? controls[0] : isRow ? null : controls[0];
  if (index === 0 && delta === -1) return isRow ? "stop" : controls[0];
  return controls[clampStep(controls.length, index, delta)];
}

/** One conversation item, as far as `a`/`d` need to know. */
export type AnswerableItem =
  | { kind: "permission"; toolUseId: string | null }
  | { kind: "tool"; toolUseId: string }
  | { kind: "other" };

/**
 * The timeline index of the permission card `a`/`d` should answer from `cursor`, or `null`.
 *
 * The UI spec (§4.3): the cursor is on the permission request, **or on the tool call it gates**. A
 * pending card is anchored directly after its tool call (`timeline.ts`), so from a tool row the
 * card is the first permission item after it that names the same `toolUseId`. Any other row has
 * no card to answer, and `a`/`d` there do nothing, rather than reaching for the nearest card:
 * approving the wrong call is exactly what a keyboard shortcut must not make easy.
 *
 * v1 S4 (spec `2026-09-27-v1-ui-design.md` §2.2): this is the ONLY rule `a`/`d`/`D` follow. There is
 * no "exactly one card anywhere, answered from any row" exception any more -- vim's operators act on
 * what the cursor is on (`:h operator`), and R32 already lands an arrival on the oldest card, so
 * the common case still needs no movement. With no target the panel says so (F13's flash) rather
 * than doing nothing silently.
 */
export function permissionTarget(items: AnswerableItem[], cursor: number): number | null {
  const here = items[cursor];
  if (here === undefined) return null;
  if (here.kind === "permission") return cursor;
  if (here.kind !== "tool" || here.toolUseId === "") return null;
  for (let i = cursor + 1; i < items.length; i++) {
    const item = items[i];
    if (item.kind === "permission" && item.toolUseId === here.toolUseId) return i;
    if (item.kind !== "permission") break;
  }
  return null;
}

/** One panel target of the global `f` HINT (spec: docs/superpowers/specs/2026-09-19-global-hint-design.md
 *  §2.2): a conversation row, a fenced code block inside one (which remembers its row, because
 *  landing on it moves the row cursor there), a web link inside one (v1 picks, Task 8, R6: the anchor,
 *  which likewise remembers its row -- landing focuses it and never clicks it), or an enabled control
 *  from `controlsOf`. The list is frozen at `hint_collect` and `shell` addresses its entries by index
 *  from then on. `rowIndex` is the row's index at that moment only; landing re-reads it from the
 *  element, because rows can be inserted above it while the labels are up. */
export type HintTarget =
  | { kind: "row"; el: HTMLElement; rowIndex: number }
  | { kind: "control"; el: HTMLElement }
  | { kind: "code"; el: HTMLElement; rowIndex: number }
  | { kind: "link"; el: HTMLAnchorElement; rowIndex: number }
  | { kind: "composer"; el: HTMLElement };

/** Marks the composer as a HINT target (spec §2.4, "面板输入框"). The composer is in no
 *  `data-nav-stop` -- `j`/`k` never walk into it, `i` and `Ctrl+l` are its routes -- and in BROWSE it
 *  is a plain `div`, which `CONTROL_SELECTOR` does not match, so without this marker HINT could never
 *  reach it (found in the whole-branch review). `Composer` sets it only while INPUT can be entered. */
export const HINT_COMPOSER_ATTR = "data-hint-composer";

/** The elements known to scroll their own content (index.css): the conversation's list, the tab
 *  bar (sideways, session tabs Task 11), and the launch chooser's list (Task 12). Named here as
 *  well as read off the computed style below, because jsdom loads no stylesheet and the tests must
 *  see these three. `.session-choice` is gone -- the old resumable-session picker it named was
 *  replaced by `EmptyTab` (F3) and the launch chooser (`.chooser-list`). */
const SCROLL_CONTAINERS = ".message-list, .tab-bar, .chooser-list";

function clips(el: HTMLElement): boolean {
  if (el.matches(SCROLL_CONTAINERS)) return true;
  const style = getComputedStyle(el);
  return [style.overflow, style.overflowX, style.overflowY].some((v) => v !== "" && v !== "visible");
}

function intersects(a: DOMRect, b: DOMRect): boolean {
  return a.left < b.right && a.right > b.left && a.top < b.bottom && a.bottom > b.top;
}

/** Whether `el` shows on screen inside `root`: it has a non-zero box, and that box intersects the
 *  root and every ancestor between them that clips its overflow (spec §2.2, "visible inside its own
 *  scroll container"). Every ancestor, not only the nearest: a nested scroller's content is only on
 *  screen where each of them lets it through. */
export function hintVisible(el: HTMLElement, root: HTMLElement): boolean {
  return boxShows(el.getBoundingClientRect(), el, root);
}

/** `hintVisible`'s test for one box of `el`'s: non-zero, and inside the root and every clipping ancestor. */
function boxShows(box: DOMRect, el: HTMLElement, root: HTMLElement): boolean {
  if (box.width === 0 || box.height === 0) return false;
  for (let a = el.parentElement; a !== null && a !== root; a = a.parentElement) {
    if (clips(a) && !intersects(box, a.getBoundingClientRect())) return false;
  }
  return intersects(box, root.getBoundingClientRect());
}

/** Where a HINT label for a link that WRAPS belongs (v1 picks, Task 8, fix round 1, Codex): the first of
 *  its line boxes that is on screen -- `hintVisible`'s own test, line by line -- and not simply its first.
 *  A link is a target while any part of it shows, and one whose first line has scrolled above the list
 *  but whose last line has not would otherwise be labelled where nothing is drawn. With no line box on
 *  screen, or none reported (jsdom lays nothing out), the bounding box, as before. A first line that only
 *  PARTLY shows is still the first one on screen: its label may hang past the edge by its own height, as a
 *  control at the edge of the list already does. */
export function firstShownLine(el: HTMLElement, root: HTMLElement): DOMRect {
  for (const line of Array.from(el.getClientRects())) {
    if (boxShows(line, el, root)) return line;
  }
  return el.getBoundingClientRect();
}

/** Visible HINT targets under `root`, in document order: for each stop (row or other), the stop
 *  itself (if it's a row), then its code blocks, then its web links (a row's only), then its controls;
 *  for a non-row stop, its controls only. The composer, when marked (`HINT_COMPOSER_ATTR`), takes its
 *  place in document order among the stops. "Visible" is `hintVisible` above. */
export function hintTargets(root: HTMLElement): HintTarget[] {
  const targets: HintTarget[] = [];
  // The composer is in no stop; a marker inside one is a reply's own HTML, not the composer.
  const markers = Array.from(root.querySelectorAll<HTMLElement>(`[${HINT_COMPOSER_ATTR}]`));
  const composerEl = markers.find((el) => stopOf(root, el) === null) ?? null;
  let composer = composerEl !== null && hintVisible(composerEl, root) ? composerEl : null;
  // Once, not per stop: a row's index is its place in this list (ruling R2).
  const rows = conversationRows(root);
  for (const stop of stopsIn(root)) {
    if (composer !== null && composer.compareDocumentPosition(stop) & Node.DOCUMENT_POSITION_FOLLOWING) {
      targets.push({ kind: "composer", el: composer });
      composer = null;
    }
    const found = rows.indexOf(stop);
    const rowIndex = found === -1 ? null : found;
    if (rowIndex !== null && hintVisible(stop, root)) targets.push({ kind: "row", el: stop, rowIndex });
    if (rowIndex !== null) {
      for (const block of stop.querySelectorAll<HTMLElement>("pre.code-block")) {
        if (hintVisible(block, root)) targets.push({ kind: "code", el: block, rowIndex });
      }
      // R6: per anchor, not through `webLinks`, which drops a repeated address before it looks at whether
      // that copy is on screen -- a reply naming one address twice, the first copy scrolled off, would
      // leave the visible copy without a label.
      for (const el of stop.querySelectorAll<HTMLAnchorElement>("a[href]")) {
        if (webUrl(el.getAttribute("href")) !== null && hintVisible(el, root)) targets.push({ kind: "link", el, rowIndex });
      }
    }
    for (const control of controlsOf(stop)) {
      if (hintVisible(control, root)) targets.push({ kind: "control", el: control });
    }
  }
  if (composer !== null) targets.push({ kind: "composer", el: composer });
  return targets;
}
