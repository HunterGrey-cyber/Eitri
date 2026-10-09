/**
 * Keyboard navigation follows the panel's document structure. `hjkl` uses known DOM order,
 * avoiding guesses about the nearest control based on pixel positions.
 *
 * A stop carries `data-nav-stop`: a conversation row, banner or terminal-handoff area, or a choice
 * row or mode button on the start screen. `j`/`k` walk stops in document order. The bottom band is
 * not a stop: `j` at the last stop stays there. Session details open with `<leader>i`, `prefix i`
 * or a click on the band. Stops without usable controls are skipped except for rows, which hold
 * the conversation cursor for `Enter`/`y` even when they have no button. A stop inside another stop
 * is content (`isOwnStop`), so reply HTML cannot add conversation rows.
 *
 * `h`/`l` walk enabled buttons, inputs, textareas and radios within a stop. Document order applies
 * unless `data-nav-order` overrides it. Permission cards put Approve before Deny and the reason
 * box so the most frequent action is one keypress away.
 *
 * The focused control is the selected control. `Tab`, clicks and `h`/`l` move the same DOM focus,
 * and `Enter`/`Space` activate it natively, so separate selection state cannot disagree with focus.
 */

export const STOP_ATTR = "data-nav-stop";

/** Label alphabet shared with the window-wide `f` HINT. It excludes `f`, which starts HINT.
 *  The `gf` path picker and `gx` link picker reuse it for their letter labels. */
export const HINT_ALPHABET = "asdjklghweruio";

/** A web link as `web_url` (`panel/src/agent_panel.rs`) will re-check it -- `http(s)`
 *  by the WHATWG parser with no base (the parser the browser opens it with, so `https://%65itri.invalid/x`
 *  and `https:\\eitri.invalid\x` are what they really are), no userinfo, a plain `[a-z0-9.-]` host with
 *  no trailing dot that is not the panel's own `eitri.invalid` (where every relative link resolves), and
 *  no backslash left anywhere in the address. Returns the normalized `href` -- what a pick shows and what
 *  `open_url` sends, never the spelling the reply wrote -- or `null`. The host rules are Rust's, restated:
 *  the parser alone keeps `eitri.invalid.`, `a$b.com`, `my_host.x`, `[::1]` and a backslash in a query, and
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
  if (!/^[a-z0-9.-]+$/.test(host) || host.endsWith(".") || host === "eitri.invalid" || url.href.includes("\\")) return null;
  return url.href;
}

/** Links reachable by `gx` and HINT inside `row`, in document order, once per normalized
 *  address. Keep the first anchor for each address and its `webUrl` result, which the picker
 *  shows and `open_url` sends. */
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

/** `gx` opens without a picker only when a visible link's plain text matches its normalized URL.
 *  Compare with the parsed address: Unicode host names may normalize to a different spelling,
 *  which the picker must show before opening. Allow the trailing slash added to a bare host.
 *  Hidden links, links in closed details and links outside the list's visible area require a pick.
 *
 *  Require `childElementCount === 0`: `textContent` includes text in hidden child elements, so
 *  matching it alone could open an address whose full spelling was never visible. Reply classes
 *  and styles are stripped, so plain text nodes cannot hide themselves inside a visible anchor.
 *  Comment nodes render nothing and contribute nothing to `textContent`. Links wrapping elements
 *  such as `b` or `code` require a pick. */
export function linkOpensAtOnce(link: { el: HTMLAnchorElement; url: string }, root: HTMLElement): boolean {
  if (link.el.childElementCount !== 0) return false;
  const text = link.el.textContent?.trim() ?? "";
  return (text === link.url || `${text}/` === link.url) && hintVisible(link.el, root);
}

/** How soon after a click showed a link's address a second click on it may open it. Under the lower bound
 *  the second click is the other half of a double-click, made before the address could be read. */
export const LINK_LOOK_MIN_MS = 400;
export const LINK_LOOK_MAX_MS = 5000;

/** The link whose address the last click showed instead of opening, and when. `row` is the conversation row
 *  the link was in: a streaming reply's HTML is replaced on every chunk, so the anchor element a second click
 *  lands on is never the one the first click did, but its row (keyed by the item) and its address are. */
export interface ShownLink {
  el: HTMLAnchorElement;
  row: HTMLElement | null;
  url: string;
  at: number;
}

/** Whether `anchor` is the link `shown` showed: the same conversation row when it has one (the anchor itself
 *  may have been rebuilt by a re-render of the reply), else the same element. */
function sameLink(shown: ShownLink, anchor: HTMLAnchorElement, root: HTMLElement): boolean {
  const row = rowOf(root, anchor);
  return row !== null && shown.row !== null ? shown.row === row : shown.el === anchor;
}

/** What a click (or Enter, which clicks) on `anchor` must do before the browser follows it. `null` lets it
 *  through: a link `linkOpensAtOnce` accepts, a second click on the link whose address the previous click
 *  showed (between `LINK_LOOK_MIN_MS` and `LINK_LOOK_MAX_MS` after it), or a link that is not a web link at
 *  all (Rust's own scheme check refuses everything but web and mail). Otherwise the address the browser would
 *  open, normalized, to be shown instead of opening: a reply's `<a href="https://evil">https://good</a>` must not
 *  launch the browser on one click. Judged on the anchor's RESOLVED `href`, which is what the click navigates
 *  to: a scheme-relative `//host/x` is a web link although `webUrl` of its attribute is not. */
export function linkClickNeedsLook(
  anchor: HTMLAnchorElement,
  root: HTMLElement,
  shown: ShownLink | null,
  now: number,
): string | null {
  const url = webUrl(anchor.href);
  if (url === null) return null;
  if (linkOpensAtOnce({ el: anchor, url }, root)) return null;
  if (shown !== null && shown.url === url && sameLink(shown, anchor, root)) {
    const since = now - shown.at;
    if (since >= LINK_LOOK_MIN_MS && since <= LINK_LOOK_MAX_MS) return null;
  }
  return url;
}

/** Whether `el` is inside a control a key activates by default: a button, summary, link or
 *  element with `role="button"`.
 *
 *  Calling `preventDefault()` on Enter for a focused button suppresses its activation click.
 *  Tab-to-Approve then Enter must keep working so permission decisions remain keyboard reachable.
 *  Space on buttons and Enter on tool-card summaries need the same protection.
 *
 *  `closest` also handles targets inside a button, such as a strong or span element, while
 *  keyboard activation targets the button itself. Both must bypass the panel's key handler.
 *
 *  Keeping this helper in `nav.ts` lets `App.tsx` and `EmptyTab.tsx` share the leader engine's
 *  control check without importing one component into the other and creating a circular import. */
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
 * **A stop is never inside another stop**. An element carrying
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

/** What `ownStops` found under one root, kept until the tree changes in a way that could change it. */
type StopCache = { observer: MutationObserver; stops: HTMLElement[] | null; rows: HTMLElement[] | null };
const stopCaches = new WeakMap<HTMLElement, StopCache>();

/** Whether a mutation could change which elements are stops: a `data-nav-stop` written or removed, or an
 *  element carrying one (or holding one) put into the tree or taken out of it. Text changes, and a reply's
 *  HTML re-rendered with no stop in it, cannot. */
function touchesStops(record: MutationRecord): boolean {
  if (record.type === "attributes") return true;
  const carries = (node: Node) =>
    node instanceof Element && (node.hasAttribute(STOP_ATTR) || node.querySelector(`[${STOP_ATTR}]`) !== null);
  for (const node of record.addedNodes) if (carries(node)) return true;
  for (const node of record.removedNodes) if (carries(node)) return true;
  return false;
}

/** The cache for `root`, emptied first if the tree changed since it was filled. The pending records are
 *  taken here, synchronously, rather than left to the observer's callback: a lookup in the same task as a
 *  commit (a layout effect, a key handler right after a render) must never see the list from before it. */
function stopCache(root: HTMLElement): StopCache {
  let cache = stopCaches.get(root);
  if (cache === undefined) {
    const fresh: StopCache = {
      observer: new MutationObserver((records) => {
        if (records.some(touchesStops)) {
          fresh.stops = null;
          fresh.rows = null;
        }
      }),
      stops: null,
      rows: null,
    };
    fresh.observer.observe(root, { childList: true, subtree: true, attributes: true, attributeFilter: [STOP_ATTR] });
    stopCaches.set(root, fresh);
    cache = fresh;
  }
  if (cache.observer.takeRecords().some(touchesStops)) {
    cache.stops = null;
    cache.rows = null;
  }
  return cache;
}

/** Every stop under `root`, in document order, whether or not it is worth landing on.
 *
 *  Read from the tree once and kept until the tree changes in a way that could change it (`stopCache`):
 *  a `j`/`k` press used to query the whole panel three times, about 6,000 elements on a long conversation.
 *  The answer is the same structural one either way (`isOwnStop`), which is what keeps `a` on the card the
 *  cursor shows. The array is shared and frozen: callers read it, never change it. */
function ownStops(root: HTMLElement): HTMLElement[] {
  const cache = stopCache(root);
  if (cache.stops === null) {
    cache.stops = Object.freeze(
      Array.from(root.querySelectorAll<HTMLElement>(`[${STOP_ATTR}]`)).filter((stop) => isOwnStop(root, stop)),
    ) as HTMLElement[];
  }
  return cache.stops;
}

/** The conversation rows under `root`, in document order: index `i` here is timeline index `i`.
 *  Only real rows -- a `data-nav-stop="row"` inside another stop (a reply's own HTML) is never
 *  one. Every site that turns the cursor into an element goes through this. The same
 *  array is returned until the stops change (`ownStops`), so a caller can tell a changed list by
 *  identity. */
export function conversationRows(root: HTMLElement): HTMLElement[] {
  const cache = stopCache(root);
  if (cache.rows === null) {
    cache.rows = Object.freeze(ownStops(root).filter((stop) => stop.getAttribute(STOP_ATTR) === "row")) as HTMLElement[];
  }
  return cache.rows;
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
 * A counted `j` (+1) or `k` (-1) takes `times` steps, each from the previous landing.
 * The walk stops at a non-row stop or a clamped boundary, matching where single steps stop.
 * Returns `null` when the region has no stops.
 *
 * Read the stops, rows and row indices once. Each step then moves along the index without DOM
 * queries, so large counts do not trigger repeated full-tree scans.
 *
 * Steps after the first start from the walk's position rather than `document.activeElement`.
 * Otherwise focus inside a stop would restart every step there and make `3j` move only once.
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

/** The timeline index of the permission card `a`/`d` should answer from `cursor`, or `null`. The
 * cursor must be on the card or the tool call it gates. From a tool row, choose the matching
 * `toolUseId` among the permission items directly following it. Other rows have no target,
 * preventing a shortcut from approving an unrelated call. This is also the targeting rule for
 * `D`, even when only one card exists. Arrival moves to the oldest card; with no target, the
 * panel shows a message. */
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

/** A panel target of global `f` HINT: a conversation row, a code block or web link inside a row,
 *  an enabled control from `controlsOf`, or a marked composer. Code and link targets remember
 *  their row so landing moves the row cursor. Landing on a link focuses it without clicking it.
 *  The list is frozen at `hint_collect`, and `shell` addresses entries by index afterward.
 *  `rowIndex` is only the index at collection time. Landing reads it from the element again
 *  because rows can be inserted above it while labels are displayed. */
export type HintTarget =
  | { kind: "row"; el: HTMLElement; rowIndex: number }
  | { kind: "control"; el: HTMLElement }
  | { kind: "code"; el: HTMLElement; rowIndex: number }
  | { kind: "link"; el: HTMLAnchorElement; rowIndex: number }
  | { kind: "composer"; el: HTMLElement };

/** Marks the composer as a HINT target. The composer is in no
 *  `data-nav-stop` -- `j`/`k` never walk into it, `i` and `Ctrl+l` are its routes -- and in BROWSE it
 *  is a plain `div`, which `CONTROL_SELECTOR` does not match, so without this marker HINT could never
 *  reach it. `Composer` sets it only while INPUT can be entered. */
export const HINT_COMPOSER_ATTR = "data-hint-composer";

/** The elements known to scroll their own content (index.css): the conversation's list, the tab
 *  bar, and the launch chooser's list. Named here as
 *  well as read off the computed style below, because jsdom loads no stylesheet and the tests must
 *  see these three. `.session-choice` is gone -- the old resumable-session picker it named was
 *  replaced by `EmptyTab` (F3) and the launch chooser (`.chooser-list`). */
const SCROLL_CONTAINERS = ".message-list, .tab-bar, .chooser-list";

function clips(el: HTMLElement): boolean {
  if (el.matches(SCROLL_CONTAINERS)) return true;
  const style = getComputedStyle(el);
  return [style.overflow, style.overflowX, style.overflowY].some((v) => v !== "" && v !== "visible");
}

/** An axis-aligned box in viewport pixels: what `getBoundingClientRect` gives, reduced to its edges. */
export type Box = { top: number; bottom: number; left: number; right: number; width: number; height: number };

/** The overlap of two boxes, or `null` when they share no area (touching edges share none). */
function overlap(a: Box, b: DOMRect | Box): Box | null {
  const top = Math.max(a.top, b.top);
  const bottom = Math.min(a.bottom, b.bottom);
  const left = Math.max(a.left, b.left);
  const right = Math.min(a.right, b.right);
  if (bottom <= top || right <= left) return null;
  return { top, bottom, left, right, width: right - left, height: bottom - top };
}

function boxOf(r: DOMRect): Box {
  return { top: r.top, bottom: r.bottom, left: r.left, right: r.right, width: r.right - r.left, height: r.bottom - r.top };
}

/** The area where `el` can show inside `root`: intersect the root box with every ancestor
 *  between them that clips overflow, including nested scroll containers. Return `null` when the
 *  intersection is empty. */
export function clipBox(el: HTMLElement, root: HTMLElement): Box | null {
  let clip: Box | null = boxOf(root.getBoundingClientRect());
  for (let a = el.parentElement; a !== null && a !== root && clip !== null; a = a.parentElement) {
    if (clips(a)) clip = overlap(clip, a.getBoundingClientRect());
  }
  return clip;
}

/** The part of `el` visible inside `root`: intersect its box with `clipBox`, or return `null` if
 *  none shows or the element has no box. Place HINT labels on this area so a partially
 *  scrolled-out target keeps its label inside the visible region. */
export function visibleBox(el: HTMLElement, root: HTMLElement): Box | null {
  const box = el.getBoundingClientRect();
  if (box.width === 0 || box.height === 0) return null;
  const clip = clipBox(el, root);
  return clip === null ? null : overlap(clip, box);
}

/** Whether `el` has a non-zero visible box inside `root` and every clipping ancestor.
 *  This is exactly `visibleBox(el, root) !== null`, so target selection and label placement use
 *  the same clipped area even within nested scrollers. */
export function hintVisible(el: HTMLElement, root: HTMLElement): boolean {
  return visibleBox(el, root) !== null;
}

/** `hintVisible`'s test for one box of `el`'s (one line box of a wrapped link, `firstShownLine`): non-zero,
 *  and meeting the clip box `el` shows in. */
function boxShows(box: DOMRect, el: HTMLElement, root: HTMLElement): boolean {
  if (box.width === 0 || box.height === 0) return false;
  const clip = clipBox(el, root);
  return clip !== null && overlap(clip, box) !== null;
}

/** Place a wrapped link's HINT label on its first visible line box, using `hintVisible`'s test
 *  on each line. Its first line may be above the list while later lines still show. Fall back to
 *  the bounding box if no line box shows or none is reported, as in jsdom. A partly visible line
 *  counts; `HintLayer` clamps the label into the clip box. */
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
  // Once, not per stop: a row's index is its place in this list.
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
      // Inspect each anchor directly. `webLinks` deduplicates addresses before checking visibility,
      // so an off-screen first copy could leave a visible copy without a label.
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
