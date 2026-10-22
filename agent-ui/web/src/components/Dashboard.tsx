import type { EditorLink, Hello, PermissionModeChoice } from "../types";

/** One row of the empty tab's dashboard: LazyVim's start screen
 *  (snacks.nvim's dashboard), a centred name and one key per action, in place of the eight resume
 *  rows this screen used to draw. `"resume"` is the newest remembered session -- `EmptyTab`'s own
 *  `r` used to walk all eight; the chooser (`w`/`prefix w`) is where every record still lives. */
export type DashItem = "new" | "restore" | "resume" | "sessions" | "mode" | "keys";

/** The key each item runs on directly -- also what `EmptyTab`'s `onKeyDown`
 *  dispatches on. Exported so that file and this one cannot silently disagree about which letter
 *  goes with which item.
 *
 *  `mode`'s column reads `⇧Tab`, not a letter -- the dashboard no longer claims
 *  a bare `m` (it collided with a fast typist's ordinary prose, e.g. "make", "mode"). The item is
 *  still reachable by `j`/`k` + Enter, Shift+Tab (`modeKey.ts`'s document-capture router, untouched
 *  here) and `<leader>m`; `EmptyTab` never dispatches on this string, it is display only. */
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
 *  workspace, and offering to resume nothing would be a dead key. The mode is not an item: the status
 *  band under this screen already names it and Shift+Tab changes it there, so a row for it only said
 *  the same thing twice. */
export function dashItems(hello: Hello): DashItem[] {
  const items: DashItem[] = ["new"];
  // The last window's tabs, offered only while Rust says there are some to bring back -- a launch
  // action, so a later empty tab beside live ones never has it (Rust stops sending the offer).
  if ((hello.restore?.labels.length ?? 0) > 0) items.push("restore");
  if (hello.resumableSessions.length > 0) items.push("resume");
  items.push("sessions", "keys");
  return items;
}

/** The consequence line (a choice must say what it does), fitted to what
 *  `agent/src/permission_policy.rs` and bypass mode actually do -- not a
 *  mockup's wording, which this project has no way to keep in sync with the policy by
 *  construction. A mode this build does not know (neither key present) draws no line at all,
 *  rather than a guess.
 *
 *  `auto`'s line says what the mode is: Claude Code's acceptEdits fast path, where
 *  `Write`/`Edit`/`NotebookEdit` inside the project, outside its protected paths, run with no card,
 *  as project reads and read-only `Bash` already did; anything else is a card. That is what
 *  `agent/src/permission_policy.rs` does in this build. The key and the name "auto" are kept, so
 *  nobody relearns them. `bypass`'s line is untouched. */
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

/** `<cwd, ~-abbreviated> · <account>` (the line used to read
 *  `<cwd> · <backend> · <account>`, but the backend reads "sidecar" in every release build and
 *  carries nothing; it lives only in `prefix i` / `<leader>i`). narrow
 *  drops the account and cuts the cwd. */
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
  /** Companion mode (`App.tsx`'s `editorLink`, `null` in the one-window mode): the start screen then
   *  speaks for a window that holds only this panel. Optional so a caller with no editor beside it
   *  needs no stand-in. */
  editorLink?: EditorLink | null;
  /** The word for the agent this tab runs when it is not Claude (`agentTitle`), named on the start
   *  screen so an empty Codex tab is not mistaken for a Claude one. `null`/absent for Claude. */
  agent?: string | null;
};

/** The empty tab's dashboard a centred question, the
 *  where-line and one row per `dashItems(hello)`, each with its key -- and nothing else. What a mode
 *  does, what a message carries from the editor and the window's keys are all in the `?` sheet, which
 *  the last row opens; only a companion window with no editor attached says one more line, because
 *  that is a problem to fix, not a fact to learn.
 *
 * Items carry `data-nav-stop="dash"` for the same CSS/grid symmetry the rest of the panel's rows
 * have, but they are deliberately NOT `nav.ts` stops (`controlsOf`'s selector has no `[role=
 * "button"]` branch, so a `dash` stop with no other control inside it drops out of `stopsIn`) and
 * deliberately NOT `<button>` elements -- `role="button"` with `tabIndex={-1}` instead, so nothing
 * here is ever really DOM-focused. A real, focused `<button>` would eat a `Space` keydown as
 * native activation before it ever reached the leader system (`App.tsx`'s root `onKeyDown`), which
 * is exactly the key this screen's own `j`/`k`/letters/`Enter` must never intercept from. */
export function Dashboard({ hello, mode, cursor, narrow, onItem, editorLink = null, agent = null }: Props) {
  return (
    <div className="dashboard">
      <DashboardHead hello={hello} narrow={narrow} agent={agent} />
      <DashboardItems hello={hello} mode={mode} cursor={cursor} onItem={onItem} editorLink={editorLink} agent={agent} />
    </div>
  );
}

/** The dashboard's name and where-line. `EmptyTab` draws it above its box, which it keeps in one place in
 *  the tree whatever the screen shows (see `EmptyTab`'s own note on the start screen's box). */
export function DashboardHead({ hello, narrow, agent = null }: { hello: Hello; narrow: boolean; agent?: string | null }) {
  return (
    <>
      <div className="dash-title">What shall we work on?</div>
      <div className="dash-where">{whereLine(hello, narrow, agent)}</div>
    </>
  );
}

/** The dashboard's rows and, in a companion window with no editor, its one line under them. With
 *  `boxIsNew`, the New session row is not drawn here: the box above the rows is that row (`EmptyTab`), and
 *  the cursor's index still counts it, so `j`/`k` and Enter mean what they did. */
export function DashboardItems({
  hello,
  mode,
  cursor,
  onItem,
  editorLink = null,
  agent = null,
  boxIsNew = false,
}: Omit<Props, "narrow"> & { boxIsNew?: boolean }) {
  const items = dashItems(hello);
  return (
    <>
      <div className="dash-items">
        {items.map((item, index) =>
          boxIsNew && item === "new" ? null : (
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
                {ITEM_ICON[item]}
              </span>
              <span className="dash-label">
                <ItemLabel item={item} hello={hello} mode={mode} agent={agent} />
              </span>
              <span className="dash-key">{DASH_ITEM_KEY[item]}</span>
            </div>
          ),
        )}
      </div>
      {editorLink !== null && editorLink.state !== "attached" && (
        <div className="dash-context">No editor is attached: run :EitriPanel in nvim to send its file with each message.</div>
      )}
    </>
  );
}
