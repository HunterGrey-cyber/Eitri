import { forwardRef } from "react";
import { BROWSE_KEYS, INPUT_KEYS } from "../keymap";
import type { KeyHelp, PanelBinding, PanelTable } from "../keymap";
import { sequenceTitle } from "../leader";

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

/**
 * The full `?` keymap: four groups, each a two-column table. The first two come from the one
 * source of truth `BROWSE_KEYS`/`INPUT_KEYS` in `./keymap`, so this list can neither promise a key
 * the panel does not have nor omit one it does. The last two, "Anywhere in the window" and "After
 * <prefix>", come from `shell`'s own `keymap` envelope, generated from the core crate's keymap:
 * nothing on this page can read what
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
      <Section title="This panel" rows={BROWSE_KEYS} />
      <LeaderAndTabKeys panel={panel} />
      <Section
        title="Typing"
        rows={[
          ...INPUT_KEYS,
          { keys: "Ctrl+h / j / k / l", what: "Move between panes (neovibe keeps these)" },
          { keys: prefixLabel, what: "The prefix (neovibe keeps it)" },
        ]}
      />
      <Section title="Anywhere in the window" rows={windowKeys} />
      <Section title={`After ${prefixLabel}`} rows={prefixKeys} />
    </div>
  );
});
