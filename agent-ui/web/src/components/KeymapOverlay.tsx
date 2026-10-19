import { forwardRef } from "react";
import { browseKeys, CARET_KEYS, INPUT_KEYS, REVIEW_KEYS, VISUAL_KEYS } from "../keymap";
import type { KeyHelp, PanelBinding, PanelTable, PluginKeysHelp } from "../keymap";
import { sequenceTitle } from "../leader";
import { listedSlashCommands } from "../slashCommands";
import type { PermissionModeChoice } from "../types";
import { modeConsequence } from "./Dashboard";

type Props = {
  /** Requested by a backdrop click only -- `App.tsx` owns `?`/`Escape`/`q`, which it intercepts
   *  before `resolveKey` ever runs, and calls this the same way. */
  onClose: () => void;
  /** "Anywhere in the window" and "After <prefix>": `shell`'s keys, from its `keymap` envelope. */
  windowKeys: KeyHelp[];
  prefixKeys: KeyHelp[];
  /** The prefix as a person reads it (`Ctrl+b`), for the last heading. */
  prefixLabel: string;
  /** The panel's own which-key table, for the new "Leader and tab
   *  keys" section -- the same table the leader engine (`../leader`) and `resolveKey` read. */
  panel: PanelTable;
  /** The lines of the user's tmux config the import did not take (`shell`'s `keymap` envelope,
   *  `tmuxSkipped`): where each is, what it says and why. No section when there are none. */
  tmuxSkipped?: KeyHelp[];
  /** A companion window (`App.tsx`: the editor link is set): its prefix section lists only the tab and
   *  panel keys, and says why the layout keys are missing. */
  companion?: boolean;
  /** Whether the Slash commands section is drawn: they are Claude Code's, so another agent's tab has none. */
  claudeSlashCommands?: boolean;
  /** The word for the active tab's agent when it is not Claude (`agentTitle`), for what manual means on its
   *  tab: another agent runs what its own sandbox allows without asking anyone. `null`/absent for Claude. */
  agent?: string | null;
  /** The modes the active tab offers, in cycle order (`tabPermissionModes`): the sheet names only these, so
   *  a tab whose agent has no manual mode is not told about one. Absent reads as all three. */
  modes?: PermissionModeChoice[];
  /** The plugin page that held the keys when `prefix ?` opened this sheet: its section comes first. Its title
   *  and rows are the page's own text, drawn as text nodes only. */
  plugin?: PluginKeysHelp;
};

const ALL_MODES: PermissionModeChoice[] = ["manual", "auto", "bypass"];

/** A plugin page's section when the page has not declared its keys. A page that declared and kept no key
 *  gets an empty section instead: it did register, so this sentence would be untrue of it. */
const PLUGIN_UNDECLARED = "此插件未登记按键";

/** The Typing section's Shift+Tab row, worded for the modes the tab cycles through. */
function cycleRow(row: KeyHelp, modes: PermissionModeChoice[]): KeyHelp {
  if (row.keys !== "Shift+Tab") return row;
  const asks = modes.includes("bypass") ? " (entering bypass asks first)" : "";
  return { ...row, what: `Cycle ${modes.join(" → ")}${asks}` };
}

/** One of the four groups, rendered from the same tables `keymap.test.ts` binds to
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
          {/* Indexed: a plugin page may list the same keys on two rows (one per context). */}
          {rows.map((row, index) => (
            <tr key={`${index}:${row.keys}`}>
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

/** Explain `PanelTable.leaderSource` on the first line so a fallback to Space is visible to the
 *  user. */
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

/** A binding's own row reads its source too: a default
 *  row names nothing (it is simply what this list already promises), an nvim mapping or an
 *  `init.lua` override says so, so this list can never claim a key the panel does not actually
 *  bind, or hide which layer put it there. */
function bindingSourceSuffix(source: PanelBinding["source"]): string {
  return source === "nvim" ? " (from nvim)" : source === "init.lua" ? " (init.lua)" : "";
}

/** The panel's leader and tab bindings appear between "This panel" and "Anywhere in the window".
 *  Show the leader, then each full key sequence using `sequenceTitle`, with its `desc` and
 *  source suffix, matching the which-key box. */
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

/** Whether the leader engine claims `v` or `V` before `resolveKey` can enter VISUAL. A key is
 *  claimed when it is the leader or the first key of any binding. `startSequence` matches
 *  `keys[0]`, so a two-key binding claims its first key just like a single-key binding. */
function visualKeyClaim(panel: PanelTable, key: string): "leader" | "binding" | null {
  if (key === panel.leader) return "leader";
  return panel.bindings.some((b) => b.keys[0] === key) ? "binding" : null;
}

/** Describe how to reach CARET and VISUAL when the leader or a panel binding claims `v` or `V`.
 *  `v` enters CARET and `V` enters the region directly, where `v`/`V` can still change
 *  selection. Call a key the leader only when it is the leader; describe an ordinary panel
 *  binding as bound elsewhere. */
function visualEntryNote(panel: PanelTable): string {
  const vClaim = visualKeyClaim(panel, "v");
  const capitalVClaim = visualKeyClaim(panel, "V");
  if (vClaim === null && capitalVClaim === null) {
    // Ctrl+e/Ctrl+y scroll the region without leaving it, so the note must name this exception
    // to "any other key leaves".
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

/** The "Selecting (v)" section sits between "This panel" and "Leader and tab keys". Show
 *  CARET_KEYS and VISUAL_KEYS in separate tables, each headed by its mode name, so shared keys
 *  such as `gg` and `Esc` retain their mode-specific meanings. */
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

/** Slash commands use a plain list because they are commands rather than keybindings. This also
 *  keeps them out of key-table row counts. Names come from `../slashCommands` in table order.
 *  `listedSlashCommands` omits `/config`. Bare `/model` can be sent and opens `../SlashPicker`. */
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
 * The full `?` keymap: the two key-table groups come from the one source of truth
 * `BROWSE_KEYS`/`INPUT_KEYS` in `./keymap`, so this list can neither promise a key the panel does
 * not have nor omit one it does. "Anywhere in the window" and "After <prefix>" come from `shell`'s
 * own `keymap` envelope, generated from `eitri_core::keymap`: nothing on this page can read what
 * GTK binds. It draws only; `App.tsx` decides when it is open, swallows every key while it is (so
 * `a`/`d` cannot reach a card hidden underneath) and scrolls it on `j`/`k` through the forwarded
 * ref. The one thing this component decides for itself is a click on its own backdrop, which also
 * closes it: `event.target === event.currentTarget` is exactly a click that landed on this element
 * and not on anything it contains, so a click inside a table (reading a row, selecting text) never
 * fires it.
 */
export const KeymapOverlay = forwardRef<HTMLDivElement, Props>(function KeymapOverlay(
  { onClose, windowKeys, prefixKeys, prefixLabel, panel, tmuxSkipped, companion = false, claudeSlashCommands = true, agent = null, modes = ALL_MODES, plugin },
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
      {/* Rust's `install_module_nav` claims `Ctrl+j` before `resolveKey`, so add it here rather
          than to `BROWSE_KEYS`, which tests tie to `resolveKey` in both directions. The Typing
          section likewise adds GTK-handled keys separately. `Ctrl+[` is handled by the
          `../ctrlBracket` document listener, so describe it in the note rather than as a
          `BROWSE_KEYS` row. The note also explains each permission mode; the status band names
          the active mode. */}
      {/* The page the keys were in when the sheet was asked for comes first: those are the keys the reader
          was just using. Its title and rows are the page's own text, so they go through `Section`, which
          draws text nodes only. */}
      {plugin !== undefined && (
        <Section title={plugin.title} rows={plugin.rows} note={plugin.declared ? undefined : PLUGIN_UNDECLARED} />
      )}
      <Section
        title="This panel"
        rows={[...browseKeys(prefixLabel), { keys: "Ctrl+j", what: "Type (the box below)" }]}
        note={`Ctrl+[ is Esc everywhere in this panel. Modes (Shift+Tab): ${modes
          .map((mode) => `${mode} — ${modeConsequence(mode, agent, prefixLabel)}`)
          .join(" ")}`}
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
      {/* Describe the context Enter sends in this group's note. `Composer` sends it; it is not
          an `INPUT_KEYS` binding handled by `resolveKey`. `compose_turn_text` adds the editor's
          file name or its live Visual selection. `feed.rs` omits the cursor line. Selection
          limits are `MAX_SELECTION_LINES = 400` and `CONTENT_LIMIT = 2000`.
          `CLI_LOCAL_COMMANDS`, including bare `/model` and `/effort`, omit the context block. */}
      <Section
        title="Typing"
        note="Enter also sends the editor's file name — not its text or cursor — or the lines selected there in Visual mode (up to 400 lines and 2000 characters). /model, /effort and the CLI's other local commands go without it."
        rows={[
          ...INPUT_KEYS.map((row) => cycleRow(row, modes)),
          // The pane-motion row lists only Ctrl+h/l. Ctrl+j/k have their own meanings in the rows
          // below, so including them here would give conflicting descriptions.
          { keys: "Ctrl+h / l", what: "Move between panes (Eitri keeps these)" },
          { keys: prefixLabel, what: "The prefix (Eitri keeps it)" },
          // V1 C1: Rust's mirror (`install_module_nav`) claims these two ahead of
          // `Composer` itself, the same reason the two rows above are spliced in here rather than
          // added to `INPUT_KEYS` -- that constant is tied to `COMPOSER_CHORDS` both ways
          // (`composerKeys.test.ts`, `Composer.test.tsx`) and would fail on a key `Composer` never
          // handles.
          { keys: "Ctrl+k", what: "Back to browsing (as Esc)" },
          { keys: "Ctrl+j", what: "The module below" },
        ]}
      />
      {claudeSlashCommands && <SlashCommands />}
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
