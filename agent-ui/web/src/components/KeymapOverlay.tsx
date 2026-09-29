import { forwardRef } from "react";
import { BROWSE_KEYS, CARET_KEYS, INPUT_KEYS, VISUAL_KEYS } from "../keymap";
import type { KeyHelp, PanelBinding, PanelTable } from "../keymap";
import { sequenceTitle } from "../leader";
import { listedSlashCommands } from "../slashCommands";

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
};

/** One of the four groups, rendered from the same tables `keymap.test.ts` binds to
 *  `resolveKey` both ways -- this component adds no keys of its own, only a title and a layout. */
function Section({ title, rows }: { title: string; rows: KeyHelp[] }) {
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
 * own `keymap` envelope, generated from the core crate's keymap: nothing on this page can read what
 * GTK binds. It draws only; `App.tsx` decides when it is open, swallows every key while it is (so
 * `a`/`d` cannot reach a card hidden underneath) and scrolls it on `j`/`k` through the forwarded
 * ref. The one thing this component decides for itself is a click on its own backdrop, which also
 * closes it: `event.target === event.currentTarget` is exactly a click that landed on this element
 * and not on anything it contains, so a click inside a table (reading a row, selecting text) never
 * fires it.
 */
export const KeymapOverlay = forwardRef<HTMLDivElement, Props>(function KeymapOverlay(
  { onClose, windowKeys, prefixKeys, prefixLabel, panel },
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
      {/* V1 C1: `Ctrl+j` here is Rust's mirror (`install_module_nav`) claiming the chord
          ahead of `resolveKey`, never `resolveKey` itself -- so it is spliced in here rather than
          added to `BROWSE_KEYS`, which `keymap.test.ts` ties to `resolveKey` both ways and would fail
          on a key that table never claims (`BROWSE_KEYS <-> resolveKey`, forward direction). The
          "Typing" section below already does the same for its own two GTK-decided rows. */}
      <Section title="This panel" rows={[...BROWSE_KEYS, { keys: "Ctrl+j", what: "Type (the box below)" }]} />
      <Selecting panel={panel} />
      <LeaderAndTabKeys panel={panel} />
      <Section
        title="Typing"
        rows={[
          ...INPUT_KEYS,
          // The pane-motion row lists only Ctrl+h/l. Ctrl+j/k have their own meanings in the rows
          // below, so including them here would give conflicting descriptions.
          { keys: "Ctrl+h / l", what: "Move between panes (neovibe keeps these)" },
          { keys: prefixLabel, what: "The prefix (neovibe keeps it)" },
          // V1 C1: Rust's mirror (`install_module_nav`) claims these two ahead of
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
      <Section title={`After ${prefixLabel}`} rows={prefixKeys} />
    </div>
  );
});
