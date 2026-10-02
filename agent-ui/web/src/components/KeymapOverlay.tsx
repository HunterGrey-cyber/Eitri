import { forwardRef } from "react";
import { browseKeys, CARET_KEYS, INPUT_KEYS, REVIEW_KEYS, VISUAL_KEYS } from "../keymap";
import type { KeyHelp, PanelBinding, PanelTable } from "../keymap";
import { sequenceTitle } from "../leader";
import { listedSlashCommands } from "../slashCommands";

type Props = {
  /** Requested by a backdrop click only -- `App.tsx` owns `?`/`Escape`/`q`, which it intercepts
   *  before `resolveKey` ever runs (spec §3.1, §3.3), and calls this the same way. */
  onClose: () => void;
  /** "Anywhere in the window" and "After <prefix>": `shell`'s keys, from its `keymap` envelope. */
  windowKeys: KeyHelp[];
  prefixKeys: KeyHelp[];
  /** The prefix as a person reads it (`Ctrl+b`), for the last heading. */
  prefixLabel: string;
  /** The panel's own which-key table (panel round 2 plan, Task 8), for the new "Leader and tab
   *  keys" section -- the same table the leader engine (`../leader`) and `resolveKey` read. */
  panel: PanelTable;
  /** The lines of the user's tmux config the import did not take (`shell`'s `keymap` envelope,
   *  `tmuxSkipped`): where each is, what it says and why. No section when there are none. */
  tmuxSkipped?: KeyHelp[];
  /** A companion window (`App.tsx`: the editor link is set): its prefix section lists only the tab and
   *  panel keys, and says why the layout keys are missing. */
  companion?: boolean;
};

/** One of the four groups (spec §3.2), rendered from the same tables `keymap.test.ts` binds to
 *  `resolveKey` both ways -- this component adds no keys of its own, only a title and a layout.
 *  `note` is a sentence under the table, for what is true of the whole group and is not one key's row
 *  (a `<p>`, like the leader and "Selecting" sections' own): a rule `resolveKey` never sees -- `Ctrl+[`
 *  is Esc is one listener on the document (`../ctrlBracket`) -- cannot be a row of a table
 *  `keymap.test.ts` ties to `resolveKey` both ways. A section with no note draws no `<p>`. */
function Section({ title, rows, note }: { title: string; rows: KeyHelp[]; note?: string }) {
  return (
    <section>
      <h2>{title}</h2>
      <table>
        <tbody>
          {rows.map((row) => (
            <tr key={row.keys}>
              <td>
                <kbd className="keycap">{row.keys}</kbd>
              </td>
              <td>{row.what}</td>
            </tr>
          ))}
        </tbody>
      </table>
      {note !== undefined && <p>{note}</p>}
    </section>
  );
}

/** What the first line says about the panel's leader, by `PanelTable.leaderSource` (panel round 2
 *  plan, Task 8, Review Focus 4): the fallback to Space is a fact worth a person reading, not a
 *  silent default. */
function leaderSourceNote(source: PanelTable["leaderSource"]): string {
  switch (source) {
    case "mapleader":
      return "nvim's mapleader";
    case "unset":
      return "mapleader is unset";
    case "unusable":
      return "nvim's mapleader is not usable here";
    case "default":
    default:
      return "default";
  }
}

/** A binding's own row reads its source too (spec §3.6's `defaults < nvim < init.lua`): a default
 *  row names nothing (it is simply what this list already promises), an nvim mapping or an
 *  `init.lua` override says so, so this list can never claim a key the panel does not actually
 *  bind, or hide which layer put it there. */
function bindingSourceSuffix(source: PanelBinding["source"]): string {
  return source === "nvim" ? " (from nvim)" : source === "init.lua" ? " (init.lua)" : "";
}

/** The panel's own leader and tab-key table (panel round 2 plan, Task 8; between "This panel" and
 *  "Anywhere in the window", since these are the same BROWSE keys' own extension): the leader
 *  itself, then every binding as a full key sequence (`sequenceTitle` -- `Space b d`, the same
 *  humanization the which-key box draws), each with its own `desc` and source suffix. */
function LeaderAndTabKeys({ panel }: { panel: PanelTable }) {
  return (
    <section>
      <h2>Leader and tab keys</h2>
      <p>
        leader: {panel.leaderLabel} ({leaderSourceNote(panel.leaderSource)})
      </p>
      <table>
        <tbody>
          {panel.bindings.map((binding) => (
            <tr key={binding.keys.join(" ")}>
              <td>
                <kbd className="keycap">{sequenceTitle(panel, binding.keys)}</kbd>
              </td>
              <td>
                {binding.desc}
                {bindingSourceSuffix(binding.source)}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </section>
  );
}

/** Whether `key` (`"v"`/`"V"`) reaches `resolveKey`'s VISUAL branch at all, or is claimed first by
 *  the leader engine (`App.tsx`, ahead of every call to `resolveKey`) -- the leader itself, or a
 *  binding's first key, the same two ways `startSequence` (`../leader`) can claim any key: its own
 *  `step` matches on `keys[0]`, not on `keys.length === 1` (fix round 1, reviewer finding, minor --
 *  a two-key binding such as `["v", "x"]` armed a sequence on `v` exactly the same as a single-key
 *  one would, and this note still called it free). */
function visualKeyClaim(panel: PanelTable, key: string): "leader" | "binding" | null {
  if (key === panel.leader) return "leader";
  return panel.bindings.some((b) => b.keys[0] === key) ? "binding" : null;
}

/** Visual-mode spec §2 (revised for 3a): what the "Selecting" section says about how `v`/`V` are
 *  actually reached, once a configured leader or panel binding may have taken one or both (D2).
 *  Only `v` gates CARET's own entry now -- `V` still reaches the region directly (O8's kept
 *  default), so losing it alone costs nothing the region's own `v`/`V` cannot still reach. Fix
 *  round 2 (reviewer finding, minor): the note names what actually took the key -- "is your leader"
 *  only when it IS the leader; a plain panel binding on it is "bound elsewhere", never a leader the
 *  user does not have. */
function visualEntryNote(panel: PanelTable): string {
  const vClaim = visualKeyClaim(panel, "v");
  const capitalVClaim = visualKeyClaim(panel, "V");
  if (vClaim === null && capitalVClaim === null) {
    // v1 trial seam review finding 3 (2026-09-28): Ctrl+e/Ctrl+y scroll the region instead of
    // ending it now (CARET_KEYS/VISUAL_KEYS' own new row), so "any other key leaves" needs its
    // one exception named here too.
    return "Any other key leaves (Ctrl+e/Ctrl+y scroll instead). The panel is read-only; Ctrl+g opens the row in nvim for search, text objects, registers.";
  }
  if (vClaim !== null && capitalVClaim !== null) {
    return "v and V are both bound elsewhere in this panel, so nothing here reaches CARET or VISUAL.";
  }
  if (vClaim !== null) {
    return vClaim === "leader" ? "v is your leader: V selects lines, Esc there gives the caret" : "v is bound elsewhere in this panel: V selects lines, Esc there gives the caret";
  }
  return capitalVClaim === "leader"
    ? "V is your leader: v reaches the caret, then V selects lines"
    : "V is bound elsewhere in this panel: v reaches the caret, then V selects lines";
}

/** One `<table>` of `KeyHelp` rows, under its own `<h3>` sub-heading -- the building block `Selecting`
 *  uses twice below, once for CARET and once for VISUAL, so the two groups are never merged into one
 *  table a reader (or a test) cannot tell apart. */
function KeyTable({ heading, rows }: { heading: string; rows: KeyHelp[] }) {
  return (
    <>
      <h3>{heading}</h3>
      <table>
        <tbody>
          {rows.map((row) => (
            <tr key={row.keys}>
              <td>
                <kbd className="keycap">{row.keys}</kbd>
              </td>
              <td>{row.what}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </>
  );
}

/** Visual-mode spec §2 (revised for 3a): one section, "Selecting (v)", between "This panel" and
 *  "Leader and tab keys" -- CARET's table then VISUAL's (D2/CARET_KEYS/VISUAL_KEYS, the same
 *  source `keymap.test.ts` ties to `resolveKey`'s region branches both ways). Fix round 2 (reviewer
 *  finding, minor): these used to be ONE table with no sub-heading, so a "gg"/"Esc" pair that looks
 *  the same in both modes (or any two rows sharing a key) rendered as 21 indistinguishable rows with
 *  two conflicting `Esc` lines and no way to tell which mode either belonged to. Two separate tables,
 *  each headed by its own mode name, so CARET's rows and VISUAL's rows are never merged. */
function Selecting({ panel }: { panel: PanelTable }) {
  return (
    <section>
      <h2>Selecting (v)</h2>
      <KeyTable heading="CARET" rows={CARET_KEYS} />
      <KeyTable heading="VISUAL" rows={VISUAL_KEYS} />
      <p>{visualEntryNote(panel)}</p>
    </section>
  );
}

/** Spec §9.2 (P10): "The `?` overlay gets a 'Slash commands' section listing the **works** row." A
 *  plain list rather than the two-column `Section` layout above -- nothing here is a keybinding, and
 *  a bare `<ul>` keeps this section out of every existing `tr`-counting test in this file's own
 *  test suite (`KeymapOverlay.test.tsx`'s "lists every row..."). Names come from
 *  `../slashCommands`'s own table (Task 10's real-CLI record), in that table's order, less what Enter
 *  holds back: `/config` is left out (`listedSlashCommands`; the v1-ui GUI pass, 2026-09-27, saw the
 *  held-back flash send the reader here to find `/config`). `/model` reads plainly now (owner trial
 *  item 2, 2026-09-28): a bare `/model` sends too, and opens a picker (`../SlashPicker`). */
function SlashCommands() {
  return (
    <section>
      <h2>Slash commands</h2>
      <ul>
        {listedSlashCommands().map((name) => (
          <li key={name}>
            <code>/{name}</code>
          </li>
        ))}
      </ul>
    </section>
  );
}

/**
 * The full `?` keymap (spec §3, plus §9.2's Slash commands section): the two key-table groups come
 * from the one source of truth `BROWSE_KEYS`/`INPUT_KEYS` in `./keymap` -- so this list can neither
 * promise a key the panel does not have (§3.3's `resolveKey` -> table direction) nor omit one it
 * does (the reverse direction). "Anywhere in the window" and "After <prefix>" come from `shell`'s
 * own `keymap` envelope (keymap spec §2.9), generated from `eitri_core::keymap`: nothing on this
 * page can read what GTK binds. It draws only; `App.tsx` decides when it is open, swallows every key
 * while it is (so `a`/`d` cannot reach a card hidden underneath -- spec §3.1) and scrolls it on
 * `j`/`k` through the forwarded ref. The one thing this component decides for itself is a click on
 * its own backdrop, which spec §3.1 also calls a close ("点击表外"): `event.target ===
 * event.currentTarget` is exactly a click that landed on this element and not on anything it
 * contains, so a click inside a table (reading a row, selecting text) never fires it.
 */
export const KeymapOverlay = forwardRef<HTMLDivElement, Props>(function KeymapOverlay(
  { onClose, windowKeys, prefixKeys, prefixLabel, panel, tmuxSkipped, companion = false },
  ref,
) {
  return (
    <div
      className="keymap-overlay"
      role="dialog"
      aria-label="Keys"
      ref={ref}
      onClick={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      {/* V1 C1 (spec §3.1): `Ctrl+j` here is Rust's mirror (`install_module_nav`) claiming the chord
          ahead of `resolveKey`, never `resolveKey` itself -- so it is spliced in here rather than
          added to `BROWSE_KEYS`, which `keymap.test.ts` ties to `resolveKey` both ways and would fail
          on a key that table never claims (`BROWSE_KEYS <-> resolveKey`, forward direction). The
          "Typing" section below already does the same for its own two GTK-decided rows. R9 (v1 picks
          Task 9): "`Ctrl+[` is Esc" is one document listener (`../ctrlBracket`), not a key `resolveKey`
          sees, so it is this group's `note` -- not a row of `BROWSE_KEYS` either. */}
      <Section
        title="This panel"
        rows={[...browseKeys(prefixLabel), { keys: "Ctrl+j", what: "Type (the box below)" }]}
        note="Ctrl+[ is Esc everywhere in this panel."
      />
      <Selecting panel={panel} />
      {/* The review overlay (`c`): its keys are `./review`'s, not `resolveKey`'s, so they are a section of
          their own, tied to that table both ways by `review.test.ts`. */}
      <Section
        title="Review (c)"
        rows={REVIEW_KEYS}
        note="Opened with c in BROWSE. It owns every key while it is open and changes nothing on disk."
      />
      <LeaderAndTabKeys panel={panel} />
      {/* R13 (v1 picks Task 11, owner decision d): what Enter sends beyond the typed text is one sentence
          about Enter, not a key of its own -- `Composer` sends it and `resolveKey` never sees it, so it
          is this group's `note`, not a row of `INPUT_KEYS` (tied to `COMPOSER_CHORDS` both ways). It reads
          off the code, not the brief: `compose_turn_text` (core/src/editor_context/compose.rs) adds the
          editor's file name -- or, while a Visual selection is live, those lines -- and `feed.rs` drops
          nvim's cursor `line`, so "not its text or cursor" is true; the caps are the nvim snippet's own
          (`MAX_SELECTION_LINES = 400`, `CONTENT_LIMIT = 2000`, nvim_editor_context.lua); and a turn the
          CLI runs as one of its own local commands goes without the block (`CLI_LOCAL_COMMANDS`, whose
          `/model` and `/effort` are the two this panel itself sends bare). The dashboard says the short
          form of the same fact (`Dashboard.tsx`'s `.dash-context`). */}
      <Section
        title="Typing"
        note="Enter also sends the editor's file name — not its text or cursor — or the lines selected there in Visual mode (up to 400 lines and 2000 characters). /model, /effort and the CLI's other local commands go without it."
        rows={[
          ...INPUT_KEYS,
          // Fix round 1 (reviewer finding): narrowed from "Ctrl+h / j / k / l" -- `j`/`k` are no
          // longer plain pane motion while typing (the two rows below claim them instead), so
          // listing them here too said two different things about the same key in the same section.
          { keys: "Ctrl+h / l", what: "Move between panes (Eitri keeps these)" },
          { keys: prefixLabel, what: "The prefix (Eitri keeps it)" },
          // V1 C1 (spec §3.1, §3.5): Rust's mirror (`install_module_nav`) claims these two ahead of
          // `Composer` itself, the same reason the two rows above are spliced in here rather than
          // added to `INPUT_KEYS` -- that constant is tied to `COMPOSER_CHORDS` both ways
          // (`composerKeys.test.ts`, `Composer.test.tsx`) and would fail on a key `Composer` never
          // handles.
          { keys: "Ctrl+k", what: "Back to browsing (as Esc)" },
          { keys: "Ctrl+j", what: "The module below" },
        ]}
      />
      <SlashCommands />
      <Section title="Anywhere in the window" rows={windowKeys} />
      <Section
        title={`After ${prefixLabel}`}
        rows={prefixKeys}
        note={companion ? "A companion window has no layout keys: your window manager arranges windows." : undefined}
      />
      {tmuxSkipped !== undefined && tmuxSkipped.length > 0 && (
        <Section
          title="Skipped from tmux"
          rows={tmuxSkipped}
          note={'The rows marked (tmux) above came from your tmux config; these lines did not. keymap.from_tmux = "off" in init.lua turns the import off.'}
        />
      )}
    </div>
  );
});
