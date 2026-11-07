import type { EditorLink, Hello, PermissionModeChoice } from "../types";

/** LazyVim-style dashboard rows: a centred name and one key per action. `"resume"` is the newest
 *  remembered session; the chooser (`w`/`prefix w`) lists every record. */
export type DashItem = "new" | "restore" | "resume" | "sessions" | "project" | "mode" | "keys";

/** The item's direct key, shared with `EmptyTab` so rendering and dispatch cannot disagree.
 *  The mode key is display-only: a bare `m` collides with prose such as "make" and "mode".
 *  Shift+Tab is claimed by `modeKey.ts`'s document-capture router; `<leader>m` also changes mode. */
export const DASH_ITEM_KEY: Record<DashItem, string> = {
  new: "i",
  restore: "s",
  resume: "r",
  sessions: "w",
  project: "p",
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
  items.push("sessions");
  if (hello.canChooseProject) items.push("project");
  items.push("keys");
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

/** One line-drawn glyph per item, in the row's own colour (`currentColor`), so every theme draws them in its own
 *  ink and none depends on a font having the character. Drawn at 16px on a 16-unit grid with a 1px stroke and every
 *  straight line centred on a half-pixel and ending on a whole one (butt caps), so at 1x each line covers whole
 *  device pixels instead of blurring across two.
 *  `"mode"` is never a row and has none. */
function ItemIcon({ item }: { item: DashItem }) {
  const paths: Record<DashItem, string> = {
    new: "M7.5 3v9M3 7.5h9",
    restore: "M2.5 5.5h8v7h-8zM4.5 6V3.5h9v7H11",
    resume: "M3.5 8.5a5 5 0 1 0 1.46-3.54L3.5 6.5M3.5 3v3.5H7M8.5 5v3.5l2 1.5",
    sessions: "M6 4.5h8M6 8.5h8M6 12.5h8M2 4.5h2M2 8.5h2M2 12.5h2",
    project: "M1.5 4.5h5l2-2h6v10h-13z",
    mode: "",
    keys: "M2.5 4.5h12v8h-12zM4 7.5h1M6 7.5h1M8 7.5h1M10 7.5h1M12 7.5h1M5 10.5h7",
  };
  return (
    <svg className="dash-icon" viewBox="0 0 16 16" aria-hidden="true" focusable="false">
      <path d={paths[item]} />
    </svg>
  );
}

/** Eitri's logo, the Gutter E, as a line drawing in the theme's own ink: the window's frame, the editor block,
 *  the narrow column to its right and the status line along the foot drawn as outlines, and the gutter bar
 *  with its three strokes -- the E -- filled in the scheme's accent, the one colour on the screen. Drawn on a
 *  40-unit grid at 40px, so one unit is one pixel: outlines are 1px strokes on half-pixels and the filled
 *  parts sit on whole pixels, and at 1x nothing blurs across two. The full-colour logo is
 *  `packaging/icons/eitri.svg`; this is the same layout in fewer inks. */
export function EitriMark() {
  return (
    <svg className="dash-mark" viewBox="0 0 40 40" aria-hidden="true" focusable="false">
      <rect className="dash-mark-frame" x="3.5" y="4.5" width="33" height="31" rx="3" />
      <rect className="dash-mark-editor" x="6.5" y="7.5" width="19" height="18" rx="1.5" />
      <rect className="dash-mark-column" x="28.5" y="7.5" width="5" height="18" rx="1.5" />
      <path className="dash-mark-rule" d="M4 28.5h32M11.5 29v6" />
      <path className="dash-mark-e" d="M9 10h14v3H12v2h8v3h-8v2h11v3H9z" />
    </svg>
  );
}

/** When a record was last opened, as a clock time on its own day ("14:05") and a month and day before it
 *  ("Oct 27"). The start screen is meant to sit still, and nothing re-renders it as time passes, so it says a
 *  time that stays true rather than "12 min ago", which would still say so hours later. `null` for a stamp
 *  that is not a positive millisecond count a date can hold, so the row draws nothing rather than an empty or raw stamp. */
export function lastOpenedStamp(nowMs: number, stamp: string): string | null {
  const millis = Number(stamp);
  if (stamp.trim() === "" || !Number.isFinite(millis) || millis <= 0) return null;
  const then = new Date(millis);
  if (Number.isNaN(then.getTime())) return null;
  const now = new Date(nowMs);
  const sameDay =
    then.getFullYear() === now.getFullYear() && then.getMonth() === now.getMonth() && then.getDate() === now.getDate();
  if (sameDay) {
    return `${String(then.getHours()).padStart(2, "0")}:${String(then.getMinutes()).padStart(2, "0")}`;
  }
  return then.toLocaleDateString("en-US", { month: "short", day: "numeric" });
}

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

/** `<cwd, ~-abbreviated> · <account>`; backend details live in `prefix i` / `<leader>i`.
 *  Narrow drops the account and cuts the cwd. */
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

/** `3 tabs · api, docs, tests`, names cut to fit and a trailing `…` when there are more than are shown;
 *  `3 tabs, 1 in bypass · …` when some of them were, so the count never reads as a fourth name. The line
 *  is one row of a narrow panel, and CSS cuts what is still too long. */
export function restoreSummary(offer: NonNullable<Hello["restore"]>): string {
  const cut = (name: string) => (name.length > RESTORE_NAME_CHARS ? `${name.slice(0, RESTORE_NAME_CHARS - 1)}…` : name);
  const names = offer.labels.slice(0, RESTORE_NAMES_SHOWN).map(cut);
  if (offer.labels.length > RESTORE_NAMES_SHOWN) names.push("…");
  const count = offer.labels.length;
  const bypass = offer.bypass > 0 ? `, ${offer.bypass} in bypass` : "";
  return `${count} ${count === 1 ? "tab" : "tabs"}${bypass} · ${names.join(", ")}`;
}

function ItemLabel({ item, hello, mode, agent }: { item: DashItem; hello: Hello; mode: PermissionModeChoice; agent: string | null }) {
  switch (item) {
    case "new":
      return <>{agent === null ? "New session" : `New ${agent} session`}</>;
    case "restore":
      return (
        <>
          Restore last session
          {hello.restore && (
            <>
              {" "}
              <span className="dash-detail">{restoreSummary(hello.restore)}</span>
            </>
          )}
        </>
      );
    case "resume": {
      const newest = hello.resumableSessions[0] as Hello["resumableSessions"][number] | undefined;
      const title = newest?.name ?? newest?.title ?? null;
      return (
        <>
          Resume last
          {title !== null && (
            <>
              {" "}
              <span className="dash-detail">{title}</span>
            </>
          )}
        </>
      );
    }
    case "sessions":
      return <>All sessions</>;
    case "project":
      return <>Open another project</>;
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
  paneFocused?: boolean;
  entryKey?: string | null;
  /** Companion mode (`App.tsx`'s `editorLink`, `null` in the one-window mode): the start screen then
   *  speaks for a window that holds only this panel. Optional so a caller with no editor beside it
   *  needs no stand-in. */
  editorLink?: EditorLink | null;
  /** The word for the agent this tab runs when it is not Claude (`agentTitle`), named on the start
   *  screen so an empty Codex tab is not mistaken for a Claude one. `null`/absent for Claude. */
  agent?: string | null;
};

/** The empty tab's dashboard: Eitri's mark, a centred question, the
 *  where-line and one row per `dashItems(hello)`, each with its key. What a mode
 *  does, what a message carries from the editor and the window's keys are all in the `?` sheet, which
 *  the last row opens. While the editor has the keys, a pointer under the rows names the key that
 *  brings them here; a companion window with no editor attached says how to attach one.
 *
 * Items carry `data-nav-stop="dash"` for the same CSS/grid symmetry the rest of the panel's rows
 * have, but they are deliberately NOT `nav.ts` stops (`controlsOf`'s selector has no `[role=
 * "button"]` branch, so a `dash` stop with no other control inside it drops out of `stopsIn`) and
 * deliberately NOT `<button>` elements -- `role="button"` with `tabIndex={-1}` instead, so nothing
 * here is ever really DOM-focused. A real, focused `<button>` would eat a `Space` keydown as
 * native activation before it ever reached the leader system (`App.tsx`'s root `onKeyDown`), which
 * is exactly the key this screen's own `j`/`k`/letters/`Enter` must never intercept from. */
export function Dashboard({ hello, mode, cursor, narrow, onItem, paneFocused, entryKey, editorLink = null, agent = null }: Props) {
  return (
    <div className="dashboard">
      <DashboardHead hello={hello} narrow={narrow} agent={agent} />
      <DashboardItems
        hello={hello}
        mode={mode}
        cursor={cursor}
        narrow={narrow}
        onItem={onItem}
        paneFocused={paneFocused}
        entryKey={entryKey}
        editorLink={editorLink}
        agent={agent}
      />
    </div>
  );
}

/** The dashboard's mark, question and where-line, one block. `EmptyTab` draws it above its box, which it keeps in one place in
 *  the tree whatever the screen shows (see `EmptyTab`'s own note on the start screen's box). */
export function DashboardHead({ hello, narrow, agent = null }: { hello: Hello; narrow: boolean; agent?: string | null }) {
  return (
    <div className="dash-head">
      <EitriMark />
      <div className="dash-title">What shall we work on?</div>
      <div className="dash-where">{whereLine(hello, narrow, agent)}</div>
    </div>
  );
}

/** The dashboard's rows, their entry-key pointer and the companion window's editor notice. With
 *  `boxIsNew`, the New session row is not drawn here: the box above the rows is that row (`EmptyTab`), and
 *  the cursor's index still counts it, so `j`/`k` and Enter mean what they did.
 *
 *  Resume last says when that session was last opened (`lastOpenedStamp` over the record's `updatedAt`),
 *  right-aligned before its key; a narrow panel leaves it out so the title keeps the room. */
export function DashboardItems({
  hello,
  mode,
  cursor,
  onItem,
  editorLink = null,
  agent = null,
  boxIsNew = false,
  narrow = false,
  paneFocused = true,
  entryKey,
}: Omit<Props, "narrow"> & { boxIsNew?: boolean; narrow?: boolean }) {
  const items = dashItems(hello);
  const newest = hello.resumableSessions[0] as Hello["resumableSessions"][number] | undefined;
  const resumeWhen = narrow || newest === undefined ? null : lastOpenedStamp(Date.now(), newest.updatedAt);
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
              <ItemIcon item={item} />
              <span className="dash-label">
                <ItemLabel item={item} hello={hello} mode={mode} agent={agent} />
              </span>
              {item === "resume" && resumeWhen !== null && <span className="dash-when">{resumeWhen}</span>}
              <kbd className="dash-key">{DASH_ITEM_KEY[item]}</kbd>
            </div>
          ),
        )}
        <div className="dash-entry-slot">
          {!paneFocused && entryKey != null && <div className="dash-entry-hint">{entryKey} to use these keys</div>}
        </div>
      </div>
      {hello.restore?.cutNote && <div className="dash-context dash-cut-note">{hello.restore.cutNote}</div>}
      {editorLink !== null && editorLink.state !== "attached" && (
        <div className="dash-context">No editor is attached: run :EitriPanel in nvim to send its file with each message.</div>
      )}
    </>
  );
}
