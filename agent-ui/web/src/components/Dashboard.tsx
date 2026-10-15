import type { EditorLink, Hello, PermissionModeChoice } from "../types";

/** One row of the empty tab's dashboard: LazyVim's start screen
 *  (snacks.nvim's dashboard), a centred name and one key per action, in place of the eight resume
 *  rows this screen used to draw. `"resume"` is the newest remembered session -- `EmptyTab`'s own
 *  `r` used to walk all eight; the chooser (`w`/`prefix w`) is where every record still lives. */
export type DashItem = "new" | "restore" | "resume" | "sessions" | "mode" | "keys";

/** Direct keys for dashboard items, shared with `EmptyTab.onKeyDown` so displayed letters and
 *  dispatch stay in sync. `mode` displays `⇧Tab` only: bare `m` could trigger during typing.
 *  The item remains reachable by navigation and Enter, Shift+Tab through `modeKey.ts`, or
 *  `<leader>m`. */
export const DASH_ITEM_KEY: Record<DashItem, string> = {
  new: "i",
  restore: "s",
  resume: "r",
  sessions: "w",
  mode: "⇧Tab",
  keys: "?",
};

/** Every item this tab currently offers, in the table's own order: `"resume"` only when
 *  a record exists to resume -- an empty `resumableSessions` is the ordinary case for a fresh
 *  workspace, and offering to resume nothing would be a dead key. */
export function dashItems(hello: Hello): DashItem[] {
  const items: DashItem[] = ["new"];
  // The last window's tabs, offered only while Rust says there are some to bring back -- a launch
  // action, so a later empty tab beside live ones never has it (Rust stops sending the offer).
  if ((hello.restore?.labels.length ?? 0) > 0) items.push("restore");
  if (hello.resumableSessions.length > 0) items.push("resume");
  items.push("sessions", "mode", "keys");
  return items;
}

/** Text explaining permission modes. Unknown modes draw no consequence line. In auto mode,
 *  project reads, read-only `Bash`, and `Write`/`Edit`/`NotebookEdit` inside the project
 *  outside protected paths run without a card; other operations ask. This matches
 *  `agent/src/permission_policy.rs`. Bypass runs edits and commands without asking. */
export const MODE_CONSEQUENCE: Record<string, string> = {
  manual: "Safe reads run; every edit and anything else asks.",
  auto: "Edits in this project and safe reads run; anything else asks.",
  bypass: "Nothing asks: edits and commands run unasked.",
};

/** The consequence line for this tab. Manual's line is about Claude's tools, which Eitri's own policy
 *  judges; another agent runs what its own sandbox allows without asking anyone, edits included, so
 *  on its tab manual says only what reaches Eitri, and where the rest is described. */
export function modeConsequence(mode: PermissionModeChoice, agent: string | null, prefix: string): string | undefined {
  if (mode === "manual" && agent !== null) {
    return `Every request ${agent} makes asks; what it runs without asking is in ${prefix} i.`;
  }
  return MODE_CONSEQUENCE[mode];
}

const ITEM_ICON: Record<DashItem, string> = {
  new: "+",
  restore: "↻",
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

/** At 360px: the cwd cut in the middle to `~/…/<basename>`, or `…/<basename>` when it
 *  was not home-relative to begin with. */
function narrowCwd(path: string): string {
  const short = abbreviateHome(path);
  const base = short.slice(short.lastIndexOf("/") + 1);
  return short.startsWith("~") ? `~/…/${base}` : `…/${base}`;
}

/** Working directory with the home path abbreviated to `~`, followed by the optional account.
 *  Narrow mode shortens the directory and omits the account. */
function whereLine(hello: Hello, narrow: boolean, agent: string | null): string {
  const cwd = narrow ? narrowCwd(hello.projectDir) : abbreviateHome(hello.projectDir);
  const parts = [cwd];
  // The account is the Claude account; another agent's tab names the agent instead.
  if (agent !== null) parts.push(agent);
  else if (!narrow && hello.account !== null) parts.push(hello.account);
  return parts.join(" · ");
}

/** How many characters of one tab's name the restore line shows, and how many names. */
const RESTORE_NAME_CHARS = 20;
const RESTORE_NAMES_SHOWN = 3;

/** `3 tabs: api, docs, tests`, names cut to fit and a trailing `…` when there are more than are shown;
 *  `3 tabs, 1 in bypass: …` when some of them were, so the count never reads as a fourth name. The line
 *  is one row of a narrow panel, and CSS cuts what is still too long. */
function restoreSummary(offer: NonNullable<Hello["restore"]>): string {
  const cut = (name: string) => (name.length > RESTORE_NAME_CHARS ? `${name.slice(0, RESTORE_NAME_CHARS - 1)}…` : name);
  const names = offer.labels.slice(0, RESTORE_NAMES_SHOWN).map(cut);
  if (offer.labels.length > RESTORE_NAMES_SHOWN) names.push("…");
  const count = offer.labels.length;
  const bypass = offer.bypass > 0 ? `, ${offer.bypass} in bypass` : "";
  return `${count} ${count === 1 ? "tab" : "tabs"}${bypass}: ${names.join(", ")}`;
}

function ItemLabel({ item, hello, mode, agent }: { item: DashItem; hello: Hello; mode: PermissionModeChoice; agent: string | null }) {
  switch (item) {
    case "new":
      return <>{agent === null ? "New session" : `New ${agent} session`}</>;
    case "restore":
      return (
        <>
          Restore last session
          {hello.restore && <span className="dash-resume-title"> ({restoreSummary(hello.restore)})</span>}
        </>
      );
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
  /** The window's readable prefix chord, passed from `App.tsx`'s `keymapHelp.prefix` through
   *  `EmptyTab`. Read it live so the hint reflects any reconfiguration in `init.lua`, such as
   *  Ctrl+a instead of the default Ctrl+b. */
  prefix: string;
  /** Companion mode (`App.tsx`'s `editorLink`, `null` in the one-window mode): the start screen then
   *  speaks for a window that holds only this panel. Optional so a caller with no editor beside it
   *  needs no stand-in. */
  editorLink?: EditorLink | null;
  /** The word for the agent this tab runs when it is not Claude (`agentTitle`), named on the start
   *  screen so an empty Codex tab is not mistaken for a Claude one. `null`/absent for Claude. */
  agent?: string | null;
};

/** The empty tab's dashboard: a centred `Eitri`, the
 *  where-line, one row per `dashItems(hello)`, the selected mode's consequence line, what every
 *  message also carries from the editor, and the first-run hint.
 *
 * Items carry `data-nav-stop="dash"` for the same CSS/grid symmetry the rest of the panel's rows
 * have, but they are deliberately NOT `nav.ts` stops (`controlsOf`'s selector has no `[role=
 * "button"]` branch, so a `dash` stop with no other control inside it drops out of `stopsIn`) and
 * deliberately NOT `<button>` elements -- `role="button"` with `tabIndex={-1}` instead, so nothing
 * here is ever really DOM-focused. A real, focused `<button>` would eat a `Space` keydown as
 * native activation before it ever reached the leader system (`App.tsx`'s root `onKeyDown`), which
 * is exactly the key this screen's own `j`/`k`/letters/`Enter` must never intercept from. */
export function Dashboard({ hello, mode, cursor, narrow, onItem, prefix, editorLink = null, agent = null }: Props) {
  const items = dashItems(hello);
  const consequence = modeConsequence(mode, agent, prefix);
  return (
    <div className="dashboard">
      <div className="dash-title">Eitri</div>
      <div className="dash-where">{whereLine(hello, narrow, agent)}</div>
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
              <ItemLabel item={item} hello={hello} mode={mode} agent={agent} />
            </span>
            <span className="dash-key">{DASH_ITEM_KEY[item]}</span>
          </div>
        ))}
      </div>
      {consequence !== undefined && <div className="dash-consequence">{consequence}</div>}
      {/* Explain editor context where a session starts. `compose_turn_text` appends the file name
          or live Visual selection to panel turns; it sends neither the cursor position nor the
          whole file's text. Show the hint at every width and in every mode. CLI-local commands such
          as `/model` and `/effort` omit this context, as the `?` overlay's Typing note explains.
          Use the same tokens as `.dash-consequence`. */}
      {/* Companion form: the editor is another window, so the line says which file travels and, with no
          editor attached, how to attach one. `editorLink` is `null` in the one-window mode, where
          Rust never sends one. */}
      <div className="dash-context">
        {editorLink === null
          ? "Each message also sends the editor's file name, or the lines you selected there in Visual mode."
          : editorLink.state === "attached"
            ? "Each message also sends the file open in your nvim, or the lines you selected there in Visual mode."
            : "No editor is attached: run :EitriPanel in nvim to send its file with each message."}
      </div>
      {/* Show the key hint on every empty tab. It occupies otherwise empty space and needs no
          persistent first-run state. */}
      <div className="dash-hint">
        {editorLink === null
          ? `Ctrl+h / Ctrl+l  editor ⇄ chat · i or Ctrl+j  type · ctrl+c  interrupt · ${prefix}  window keys · ?  all keys`
          : `Ctrl+h / Ctrl+l  leave the panel · i or Ctrl+j  type · ctrl+c  interrupt · ${prefix}  tab keys · ?  all keys`}
      </div>
    </div>
  );
}
