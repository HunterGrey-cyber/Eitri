import type { EditorLink, Hello, PermissionModeChoice } from "../types";

/** One row of the empty tab's dashboard (spec §7, `mock: empty.html` C): LazyVim's start screen
 *  (snacks.nvim's dashboard), a centred name and one key per action, in place of the eight resume
 *  rows this screen used to draw. `"resume"` is the newest remembered session -- `EmptyTab`'s own
 *  `r` used to walk all eight; the chooser (`w`/`prefix w`) is where every record still lives. */
export type DashItem = "new" | "restore" | "resume" | "sessions" | "mode" | "keys";

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
  restore: "s",
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
  // The last window's tabs, offered only while Rust says there are some to bring back -- a launch
  // action, so a later empty tab beside live ones never has it (Rust stops sending the offer).
  if ((hello.restore?.labels.length ?? 0) > 0) items.push("restore");
  if (hello.resumableSessions.length > 0) items.push("resume");
  items.push("sessions", "mode", "keys");
  return items;
}

/** The consequence line (spec §7, decision 3: "the choice must say what it does"), fitted to what
 *  `agent/src/permission_policy.rs` and the owner's 2026-09-20 Bypass ruling actually do -- not the
 *  mockup's own wording, which this project has no way to keep in sync with the policy by
 *  construction. A mode this build does not know (neither key present) draws no line at all,
 *  rather than a guess.
 *
 *  **v1 trial item 4C (owner trial feedback §4, §4b, "C: say what the mode is"):** `auto`'s line
 *  describes Claude Code's acceptEdits fast path, item 4A: `Write`/`Edit`/`NotebookEdit` inside the
 *  project, outside its protected paths, run with no card, as project reads and read-only `Bash`
 *  already did; anything else is a card. That is what `agent/src/permission_policy.rs` does in this
 *  build -- 4A merged into `fix/v1-trial` (`5559877`); the line was written ahead of it, on the
 *  owner's instruction, and the two disagreed only until then (dated record, 2026-09-28 (v1 trial,
 *  item 4C) and (v1 trial, whole-branch review fixes)). The key and the name "auto" are unchanged
 *  (owner: "The key stays and its name stays 'auto'"). `bypass`'s line is untouched. */
export const MODE_CONSEQUENCE: Record<string, string> = {
  auto: "Edits in this project and safe reads run; anything else asks.",
  bypass: "Nothing asks: edits and commands run unasked.",
};

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

/** At 360px (spec §7): the cwd cut in the middle to `~/…/<basename>`, or `…/<basename>` when it
 *  was not home-relative to begin with. */
function narrowCwd(path: string): string {
  const short = abbreviateHome(path);
  const base = short.slice(short.lastIndexOf("/") + 1);
  return short.startsWith("~") ? `~/…/${base}` : `…/${base}`;
}

/** `<cwd, ~-abbreviated> · <account>` (v1 trial item 1, correcting spec §7: the line used to read
 *  `<cwd> · <backend> · <account>`, but the backend reads "sidecar" in every release build and
 *  carries nothing -- decision 6 already said it lives only in `prefix i` / `<leader>i`). narrow
 *  drops the account and cuts the cwd. */
function whereLine(hello: Hello, narrow: boolean): string {
  const cwd = narrow ? narrowCwd(hello.projectDir) : abbreviateHome(hello.projectDir);
  const parts = [cwd];
  if (!narrow && hello.account !== null) parts.push(hello.account);
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

function ItemLabel({ item, hello, mode }: { item: DashItem; hello: Hello; mode: PermissionModeChoice }) {
  switch (item) {
    case "new":
      return <>New session</>;
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
  /** V1 P11 (spec §10.1): the window's own prefix chord, as a person reads it (`"Ctrl+b"` stock,
   *  `"Ctrl+a"` when `init.lua` reconfigures it) -- `App.tsx`'s `keymapHelp.prefix`, threaded
   *  through `EmptyTab`. Read live rather than hard-coded so the hint line never lies about a
   *  reconfigured prefix. */
  prefix: string;
  /** Companion mode (`App.tsx`'s `editorLink`, `null` in the one-window mode): the start screen then
   *  speaks for a window that holds only this panel. Optional so a caller with no editor beside it
   *  needs no stand-in. */
  editorLink?: EditorLink | null;
};

/** The empty tab's dashboard (panel round 2 plan, Task 12; spec §7): a centred `Eitri`, the
 *  where-line, one row per `dashItems(hello)`, the selected mode's consequence line, what every
 *  message also carries from the editor (v1 picks Task 11), and the first-run hint.
 *
 * Items carry `data-nav-stop="dash"` for the same CSS/grid symmetry the rest of the panel's rows
 * have, but they are deliberately NOT `nav.ts` stops (`controlsOf`'s selector has no `[role=
 * "button"]` branch, so a `dash` stop with no other control inside it drops out of `stopsIn`) and
 * deliberately NOT `<button>` elements -- `role="button"` with `tabIndex={-1}` instead, so nothing
 * here is ever really DOM-focused. A real, focused `<button>` would eat a `Space` keydown as
 * native activation before it ever reached the leader system (`App.tsx`'s root `onKeyDown`), which
 * is exactly the key this screen's own `j`/`k`/letters/`Enter` must never intercept from. */
export function Dashboard({ hello, mode, cursor, narrow, onItem, prefix, editorLink = null }: Props) {
  const items = dashItems(hello);
  const consequence = MODE_CONSEQUENCE[mode];
  return (
    <div className="dashboard">
      <div className="dash-title">Eitri</div>
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
      {/* R13 (docs/superpowers/plans/2026-09-29-v1-picks.md, Task 11; owner decision d): what rides with
          every message, said where the mode is chosen and a session starts. `compose_turn_text`
          (core/src/editor_context/compose.rs) appends the editor's file name -- or, while a Visual
          selection is live, those lines -- to each turn the panel sends. It is never the cursor: nvim
          reports the cursor `line` and `feed.rs` parses it and drops it (the report this came from said
          "cursor"; R13 corrects it), and the file's text is never sent either. So this names those two and
          nothing more. Unconditional -- it is true of every mode and every width, so an unknown mode still
          gets it; the exception, a turn the CLI runs as one of its own local commands (`/model`,
          `/effort`: `CLI_LOCAL_COMMANDS`), goes without it, and the `?` overlay's "Typing" note says so
          where there is room for it. Same tokens as `.dash-consequence`. */}
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
      {/* V1 P11 (spec §10.1): one first-run hint line, shown on every empty tab rather than once --
          the empty tab is seen only when nothing else is, so it costs nothing and needs no "first
          run" state file. Eitri's own; the three keys are the audit's §5 item 5. */}
      <div className="dash-hint">
        {editorLink === null
          ? `Ctrl+h / Ctrl+l  editor ⇄ chat · i or Ctrl+j  type · ctrl+c  interrupt · ${prefix}  window keys · ?  all keys`
          : `Ctrl+h / Ctrl+l  leave the panel · i or Ctrl+j  type · ctrl+c  interrupt · ${prefix}  tab keys · ?  all keys`}
      </div>
    </div>
  );
}
