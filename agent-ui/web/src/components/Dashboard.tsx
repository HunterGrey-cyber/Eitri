import type { Hello, PermissionModeChoice } from "../types";

/** One row of the empty tab's dashboard (spec §7, `mock: empty.html` C): LazyVim's start screen
 *  (snacks.nvim's dashboard), a centred name and one key per action, in place of the eight resume
 *  rows this screen used to draw. `"resume"` is the newest remembered session -- `EmptyTab`'s own
 *  `r` used to walk all eight; the chooser (`w`/`prefix w`) is where every record still lives. */
export type DashItem = "new" | "resume" | "sessions" | "mode" | "keys";

/** The key each item runs on directly, spec §7's table -- also what `EmptyTab`'s `onKeyDown`
 *  dispatches on. Exported so that file and this one cannot silently disagree about which letter
 *  goes with which item.
 *
 *  V1 S2 (spec §2.4): `mode`'s column reads `⇧Tab`, not a letter -- the dashboard no longer claims
 *  a bare `m` (it collided with a fast typist's ordinary prose, e.g. "make", "mode"). The item is
 *  still reachable by `j`/`k` + Enter, Shift+Tab (`modeKey.ts`'s document-capture router, untouched
 *  here) and `<leader>m`; `EmptyTab` never dispatches on this string, it is display only. */
export const DASH_ITEM_KEY: Record<DashItem, string> = {
  new: "i",
  resume: "r",
  sessions: "w",
  mode: "⇧Tab",
  keys: "?",
};

/** Every item this tab currently offers, in the table's own order (spec §7): `"resume"` only when
 *  a record exists to resume -- an empty `resumableSessions` is the ordinary case for a fresh
 *  workspace, and offering to resume nothing would be a dead key. */
export function dashItems(hello: Hello): DashItem[] {
  const items: DashItem[] = ["new"];
  if (hello.resumableSessions.length > 0) items.push("resume");
  items.push("sessions", "mode", "keys");
  return items;
}

/** The consequence line (spec §7, decision 3: "the choice must say what it does"), fitted to what
 *  `agent/src/permission_policy.rs` and the owner's 2026-09-20 Bypass ruling actually do -- not the
 *  mockup's own wording, which this project has no way to keep in sync with the policy by
 *  construction. A mode this build does not know (neither key present) draws no line at all,
 *  rather than a guess. */
export const MODE_CONSEQUENCE: Record<string, string> = {
  auto: "Reads inside the project run by themselves; edits and other commands ask you.",
  bypass: "Nothing asks: edits and commands run unasked.",
};

const ITEM_ICON: Record<DashItem, string> = {
  new: "+",
  resume: "↺",
  sessions: "☰",
  mode: "",
  keys: "?",
};

/** `~-abbreviates` a Linux/macOS home directory prefix. There is no way for this WebView to read
 *  the real `$HOME` (nothing on the wire carries it, and a browser has no `os.homedir()`) -- this
 *  is the same pattern-match every shell prompt already does for the two conventional prefixes,
 *  not a guarantee it is always somebody's actual home. */
function abbreviateHome(path: string): string {
  return path.replace(/^\/(?:home|Users)\/[^/]+/, "~");
}

/** At 360px (spec §7): the cwd cut in the middle to `~/…/<basename>`, or `…/<basename>` when it
 *  was not home-relative to begin with. */
function narrowCwd(path: string): string {
  const short = abbreviateHome(path);
  const base = short.slice(short.lastIndexOf("/") + 1);
  return short.startsWith("~") ? `~/…/${base}` : `…/${base}`;
}

/** `<cwd, ~-abbreviated> · <backend> · <account>` (spec §7); narrow drops the account and cuts the
 *  cwd, backend stays either way. */
function whereLine(hello: Hello, narrow: boolean): string {
  const cwd = narrow ? narrowCwd(hello.projectDir) : abbreviateHome(hello.projectDir);
  const parts = [cwd, hello.backend];
  if (!narrow && hello.account !== null) parts.push(hello.account);
  return parts.join(" · ");
}

function ItemLabel({ item, hello, mode }: { item: DashItem; hello: Hello; mode: PermissionModeChoice }) {
  switch (item) {
    case "new":
      return <>New session</>;
    case "resume": {
      const newest = hello.resumableSessions[0] as Hello["resumableSessions"][number] | undefined;
      const title = newest?.name ?? newest?.title ?? null;
      return (
        <>
          Resume last
          {title !== null && <span className="dash-resume-title"> {title}</span>}
        </>
      );
    }
    case "sessions":
      return <>All sessions</>;
    case "mode":
      return <>Mode: {mode}</>;
    case "keys":
      return <>Keys</>;
  }
}

type Props = {
  hello: Hello;
  /** This tab's own mode (`TabInfo.mode`, not a choice from `hello`) -- the mode a session started
   *  here would actually run in. */
  mode: PermissionModeChoice;
  /** The item under the cursor (`j`/`k` in `EmptyTab`) -- a plain index this component only
   *  reads, never a `nav.ts` stop: no item is ever really DOM-focused (see the item markup's own
   *  comment for why). */
  cursor: number;
  narrow: boolean;
  onItem: (item: DashItem) => void;
  /** V1 P11 (spec §10.1): the window's own prefix chord, as a person reads it (`"Ctrl+b"` stock,
   *  `"Ctrl+a"` when `init.lua` reconfigures it) -- `App.tsx`'s `keymapHelp.prefix`, threaded
   *  through `EmptyTab`. Read live rather than hard-coded so the hint line never lies about a
   *  reconfigured prefix. */
  prefix: string;
};

/** The empty tab's dashboard (panel round 2 plan, Task 12; spec §7): a centred `neovibe`, the
 *  where-line, one row per `dashItems(hello)`, and the selected mode's consequence line.
 *
 * Items carry `data-nav-stop="dash"` for the same CSS/grid symmetry the rest of the panel's rows
 * have, but they are deliberately NOT `nav.ts` stops (`controlsOf`'s selector has no `[role=
 * "button"]` branch, so a `dash` stop with no other control inside it drops out of `stopsIn`) and
 * deliberately NOT `<button>` elements -- `role="button"` with `tabIndex={-1}` instead, so nothing
 * here is ever really DOM-focused. A real, focused `<button>` would eat a `Space` keydown as
 * native activation before it ever reached the leader system (`App.tsx`'s root `onKeyDown`), which
 * is exactly the key this screen's own `j`/`k`/letters/`Enter` must never intercept from. */
export function Dashboard({ hello, mode, cursor, narrow, onItem, prefix }: Props) {
  const items = dashItems(hello);
  const consequence = MODE_CONSEQUENCE[mode];
  return (
    <div className="dashboard">
      <div className="dash-title">neovibe</div>
      <div className="dash-where">{whereLine(hello, narrow)}</div>
      <div className="dash-items">
        {items.map((item, index) => (
          <div
            key={item}
            className="dash-item"
            role="button"
            tabIndex={-1}
            data-nav-stop="dash"
            aria-current={index === cursor ? "true" : undefined}
            onClick={() => onItem(item)}
          >
            <span className="dash-icon" aria-hidden="true">
              {item === "mode" ? (
                <span className="mode-glyph" data-mode-name={mode}>
                  ⏵⏵
                </span>
              ) : (
                ITEM_ICON[item]
              )}
            </span>
            <span className="dash-label">
              <ItemLabel item={item} hello={hello} mode={mode} />
            </span>
            <span className="dash-key">{DASH_ITEM_KEY[item]}</span>
          </div>
        ))}
      </div>
      {consequence !== undefined && <div className="dash-consequence">{consequence}</div>}
      {/* V1 P11 (spec §10.1): one first-run hint line, shown on every empty tab rather than once --
          the empty tab is seen only when nothing else is, so it costs nothing and needs no "first
          run" state file. neovibe's own; the three keys are the audit's §5 item 5. */}
      <div className="dash-hint">
        {`Ctrl+h / Ctrl+l  editor ⇄ chat · i or Ctrl+j  type · ctrl+c  interrupt · ${prefix}  window keys · ?  all keys`}
      </div>
    </div>
  );
}
