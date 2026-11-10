import { forwardRef, useLayoutEffect, useRef } from "react";
import type { Ref } from "react";
import { isPanelCommand, browseKeys, CARD_KEYS, CARET_KEYS, INPUT_KEYS, QUESTION_KEYS, REVIEW_KEYS, TASKS_KEYS, VISUAL_KEYS } from "../keymap";
import type { KeyHelp, PanelBinding, PanelTable, PluginKeysHelp } from "../keymap";
import { sequenceTitle } from "../leader";
import { listedSlashCommands } from "../slashCommands";
import type { PageLine, PermissionModeChoice } from "../types";
import type { RemoteConnectedDevice } from "../bridge";
import { revealHidden } from "../revealHidden";
import { Revealed } from "./Revealed";
import { modeConsequence } from "./Dashboard";
import { DEVICE_LIST_KEYS, DEVICE_LIST_NOTE } from "../DeviceList";

type Props = {
  pages?: PageLine[];
  /** Requested by a backdrop click only -- `App.tsx` owns `?`/`Escape`/`q`, which it intercepts
   *  before `resolveKey` ever runs, and calls this the same way. */
  onClose: () => void;
  /** "Anywhere in the window" and "After <prefix>": `shell`'s keys, from its `keymap` envelope. */
  windowKeys: KeyHelp[];
  prefixKeys: KeyHelp[];
  /** The prefix as a person reads it (`Ctrl+b`), for the last heading. */
  prefixLabel: string;
  /** The panel's own which-key table, for the "Leader and tab
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
  /** The phones connected to this desk now (remote control): named first, with the grant each has and the `Host` it
   *  came through, since the band's phone mark only counts them. No section when there are none. */
  phones?: RemoteConnectedDevice[];
  /** Remote control is on in this window: the device list's keys get a section. */
  deviceList?: boolean;
  /** Open at the window prefix keys when no plugin section takes precedence. */
  startAt?: "prefix";
  /** Otherwise open at the section of the mode `F1` was pressed in (`card`: card mode's own keys). */
  openAt?: "browse" | "input" | "selecting" | "question" | "card";
  /** Which opening request `startAt` belongs to. A different number while the sheet is up positions it again;
   *  the same number never does, so ordinary re-renders leave the reader's scroll alone. */
  startRequest?: number;
};

/** The connected phones, one line each. Names and hosts are the phone's and the network's words: drawn as text with
 *  their hidden characters escaped. */
function PhonesSection({ phones }: { phones: RemoteConnectedDevice[] }) {
  return (
    <section>
      <h2>Phones connected</h2>
      <table>
        <tbody>
          {phones.map((phone) => (
            <tr key={phone.device}>
              <td>
                <Revealed pieces={revealHidden(phone.name)} />
              </td>
              <td>
                {phone.grant} · <Revealed pieces={revealHidden(phone.host)} />
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      <p>These phones can watch this desk{phones.some((p) => p.grant === "watch+steer") ? ", and some can send messages" : ""}. `eitri remote revoke` takes one out.</p>
    </section>
  );
}

const ALL_MODES: PermissionModeChoice[] = ["manual", "auto", "bypass"];

/** The keys that open the device list as a person reads them (`Space r d`), or what to bind when none does. */
function deviceListKey(panel: PanelTable): string {
  const binding = panel.bindings.find((b) => b.action === "remote.devices");
  return binding === undefined ? "bind remote.devices in init.lua's panel table" : sequenceTitle(panel, binding.keys);
}

/** A plugin page's section when the page has not declared its keys. A page that declared and kept no key
 *  gets an empty section instead: it did register, so this sentence would be untrue of it. */
const PLUGIN_UNDECLARED = "This plugin has not declared its keys";

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
function Section({ title, rows, note, headingRef }: { title: string; rows: KeyHelp[]; note?: string; headingRef?: Ref<HTMLHeadingElement> }) {
  return (
    <section>
      <h2 ref={headingRef}>{title}</h2>
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

/** Keep unclaimed built-in keys and add the user bindings for a named motion. */
function effectiveRow(row: KeyHelp, panel: PanelTable): KeyHelp {
  if (row.motion === undefined) return row;
  const keys = row.keys.split(" / ").filter((key) => {
    const tokens = key.length === 2 ? Array.from(key) : [key];
    return !panel.bindings.some((b) => b.action !== row.motion && b.keys.length === tokens.length
      && b.keys.every((token, i) => token === tokens[i]));
  });
  for (const binding of panel.bindings) {
    if (binding.action !== row.motion) continue;
    const key = binding.keys.length === 2 && ["g", "z", "[", "]"].includes(binding.keys[0])
      ? binding.keys.join("") : sequenceTitle(panel, binding.keys);
    if (!keys.some((existing) => existing === key || existing === binding.keys.join(""))) keys.push(key);
  }
  return { ...row, keys: keys.join(" / ") || "Unbound" };
}

/** What the first line says about the panel's leader, by `PanelTable.leaderSource`: the fallback to Space is a fact
 *  worth a person reading, not a silent default. */
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

/** A binding's own row reads its source too (`defaults < nvim < init.lua`): a default
 *  row names nothing (it is simply what this list already promises), an nvim mapping or an
 *  `init.lua` override says so, so this list can never claim a key the panel does not actually
 *  bind, or hide which layer put it there. */
function bindingSourceSuffix(source: PanelBinding["source"]): string {
  return source === "nvim" ? " (from nvim)" : source === "init.lua" ? " (init.lua)" : "";
}

/** The panel's own leader and tab-key table (between "This panel" and
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
          {panel.bindings.filter(isPanelCommand).map((binding) => (
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
 *  `step` matches on `keys[0]`, not on `keys.length === 1` (a two-key binding such as `["v", "x"]` armed a sequence on `v` exactly the same as a single-key
 *  one would, and this note still called it free). */
function visualKeyClaim(panel: PanelTable, key: string): "leader" | "binding" | null {
  if (key === panel.leader) return "leader";
  return panel.bindings.some((b) => b.keys[0] === key) ? "binding" : null;
}

/** What the "Selecting" section says about how `v`/`V` are
 *  actually reached, once a configured leader or panel binding may have taken one or both.
 *  Only `v` gates CARET's own entry now -- `V` still reaches the region directly, so losing it alone costs nothing the region's own `v`/`V` cannot still reach. The
 *  note names what actually took the key -- "is your leader"
 *  only when it IS the leader; a plain panel binding on it is "bound elsewhere", never a leader the
 *  user does not have. */
function visualEntryNote(panel: PanelTable): string {
  const vClaim = visualKeyClaim(panel, "v");
  const capitalVClaim = visualKeyClaim(panel, "V");
  if (vClaim === null && capitalVClaim === null) {
    // Ctrl+e/Ctrl+y scroll the region instead of
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

/** Selecting shows CARET then VISUAL under separate headings. Shared keys such as gg and Esc
 *  have different meanings in the two modes, so one merged table is ambiguous. */
function Selecting({ panel, headingRef }: { panel: PanelTable; headingRef?: Ref<HTMLHeadingElement> }) {
  return (
    <section>
      <h2 ref={headingRef}>Selecting (v)</h2>
      <KeyTable heading="CARET" rows={CARET_KEYS} />
      <KeyTable heading="VISUAL" rows={VISUAL_KEYS} />
      <p>{visualEntryNote(panel)}</p>
    </section>
  );
}

/** Slash commands that work, in `listedSlashCommands` order, with held-back commands such as
 *  `/config` omitted. A plain list stays outside the key-table row counts in `KeymapOverlay.test.tsx`.
 *  A bare `/model` sends and opens a picker (`../slashPicker`). */
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

/** The key tables share the panel's bindings; the window and prefix rows come from the shell, since
 *  this page cannot read GTK's bindings. App owns the keyboard and scrolls this box through the ref,
 *  so reading the sheet never acts on a card underneath. Only a click on the backdrop closes it here:
 *  a click inside a table must still allow reading a row or selecting its text. */
export const KeymapOverlay = forwardRef<HTMLDivElement, Props>(function KeymapOverlay(
  { pages = [], onClose, windowKeys, prefixKeys, prefixLabel, panel, tmuxSkipped, companion = false, claudeSlashCommands = true, agent = null, modes = ALL_MODES, plugin, phones, deviceList = false, startAt, openAt, startRequest = 0 },
  ref,
) {
  const prefixHeading = useRef<HTMLHeadingElement>(null);
  const cardHeading = useRef<HTMLHeadingElement>(null);
  const browseHeading = useRef<HTMLHeadingElement>(null);
  const inputHeading = useRef<HTMLHeadingElement>(null);
  const selectingHeading = useRef<HTMLHeadingElement>(null);
  const questionHeading = useRef<HTMLHeadingElement>(null);
  // Positioned once per request, not per render: a re-render must not undo the reader's own scrolling,
  // but a new request that arrives while the sheet is up (prefix ? over the chat's ?) moves it.
  const positionedFor = useRef<number | null>(null);
  useLayoutEffect(() => {
    if (positionedFor.current === startRequest) return;
    positionedFor.current = startRequest;
    if (startAt === "prefix" && plugin === undefined) {
      prefixHeading.current?.scrollIntoView({ block: "start" });
    } else if (openAt !== undefined && plugin === undefined) {
      const heading = { browse: browseHeading, input: inputHeading, selecting: selectingHeading, question: questionHeading, card: cardHeading }[openAt];
      heading.current?.scrollIntoView({ block: "start" });
    }
  }, [startRequest]);
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
      <p>
        Scroll this sheet: j/k or ↓/↑; Ctrl+d/u half a page; {prefixLabel === "Ctrl+b" ? "Ctrl+f" : "Ctrl+f/b"},
        {" "}PageDown/PageUp or Space a page{prefixLabel === "Ctrl+b" ? " (Ctrl+b is the prefix)" : ""};
        gg/G top/bottom. ? / Esc / q close.
      </p>
      {/* Rust's mirror claims Ctrl+j ahead of resolveKey, so it is spliced in rather than added to
          BROWSE_KEYS, which keymap.test.ts ties to resolveKey both ways. Typing does the same for
          its GTK-decided rows. Ctrl+[ is a document listener, so it belongs in the note rather than
          that table. Mode consequences live here; the status band names the current mode. */}
      {phones !== undefined && phones.length > 0 && <PhonesSection phones={phones} />}
      {/* The page the keys were in when the sheet was asked for comes first: those are the keys the reader
          was just using. Its title and rows are the page's own text, so they go through `Section`, which
          draws text nodes only. */}
      {plugin !== undefined && (
        <Section title={plugin.title} rows={plugin.rows} note={plugin.declared ? undefined : PLUGIN_UNDECLARED} />
      )}
      <Section
        title="This panel"
        headingRef={browseHeading}
        rows={[...browseKeys(prefixLabel).map((row) => effectiveRow(row, panel)), { keys: "Ctrl+j", what: "Type (the box below)" }]}
        note={`Ctrl+[ is Esc everywhere in this panel. Modes (Shift+Tab): ${modes
          .map((mode) => `${mode} — ${modeConsequence(mode, agent, prefixLabel)}`)
          .join(" ")}`}
      />
      {/* Card mode's keys, `cardKey`'s (`../cardMode`), tied to it by `keymap.test.ts`. */}
      <Section
        title="Answering a card"
        headingRef={cardHeading}
        rows={CARD_KEYS}
        note="Card mode: Alt-n / Alt-p from browsing or typing, or Enter on a card's row (on its own). A digit answers only once the card has been on screen in card mode for a moment, and any other key in between makes it wait again. A card never takes card mode by itself."
      />
      {pages.length > 0 && (
        <section>
          <h2>Pages</h2>
          <ul>
            {pages.map((page) => (
              <li key={page.id}>{page.title} <code>:Eitri plugin load {page.id}</code></li>
            ))}
          </ul>
        </section>
      )}
      <Selecting panel={panel} headingRef={selectingHeading} />
      {/* The review overlay (`c`): its keys are `./review`'s, not `resolveKey`'s, so they are a section of
          their own, tied to that table both ways by `review.test.ts`. */}
      <Section
        title="Review (c)"
        rows={REVIEW_KEYS}
        note="Opened with c in BROWSE. It owns every key while it is open and changes nothing on disk."
      />
      <LeaderAndTabKeys panel={panel} />
      {deviceList && (
        <Section
          title={`Phones (${deviceListKey(panel)})`}
          rows={DEVICE_LIST_KEYS}
          note={`The remote-control device list. ${DEVICE_LIST_NOTE}`}
        />
      )}
      {/* What Enter sends beyond the typed text is one sentence
          about Enter, not a key of its own -- `Composer` sends it and `resolveKey` never sees it, so it
          is this group's `note`, not a row of `INPUT_KEYS` (tied to `COMPOSER_CHORDS` both ways). It reads
          off the code: `compose_turn_text` (core/src/editor_context/compose.rs) adds the
          editor's file name -- or, while a Visual selection is live, those lines -- and `feed.rs` drops
          nvim's cursor `line`, so "not its text or cursor" is true; the caps are the nvim snippet's own
          (`MAX_SELECTION_LINES = 400`, `CONTENT_LIMIT = 2000`, nvim_editor_context.lua); and a turn the
          CLI runs as one of its own local commands goes without the block (`CLI_LOCAL_COMMANDS`, whose
          `/model` and `/effort` are the two this panel itself sends bare). This is the one place the panel
          says it. */}
      <Section
        title="Typing"
        headingRef={inputHeading}
        note="Enter also sends the editor's file name — not its text or cursor — or the lines selected there in Visual mode (up to 400 lines and 2000 characters). /model, /effort and the CLI's other local commands go without it."
        rows={[
          ...INPUT_KEYS.map((row) => cycleRow(row, modes)),
          // Narrowed from "Ctrl+h / j / k / l" -- `j`/`k` are no
          // longer plain pane motion while typing (the two rows below claim them instead), so
          // listing them here too said two different things about the same key in the same section.
          { keys: "Ctrl+h / l", what: "Move between panes (Eitri keeps these)" },
          { keys: prefixLabel, what: "The prefix (Eitri keeps it)" },
          // Rust's mirror (`install_module_nav`) claims these two ahead of
          // `Composer` itself, the same reason the two rows above are spliced in here rather than
          // added to `INPUT_KEYS` -- that constant is tied to `COMPOSER_CHORDS` both ways
          // (`composerKeys.test.ts`, `Composer.test.tsx`) and would fail on a key `Composer` never
          // handles.
          { keys: "Ctrl+k", what: "Back to browsing (as Esc)" },
          { keys: "Ctrl+j", what: "The module below" },
        ]}
      />
      {claudeSlashCommands && <SlashCommands />}
      {/* Claude Code's background tasks list and its question dock own their keys while they are up, like the
          review overlay; both exist only on Claude tabs, so they follow the slash commands. */}
      {claudeSlashCommands && (
        <Section
          title="Background tasks (/tasks)"
          rows={TASKS_KEYS}
          note="Opened with /tasks (or /bashes), /workflows, the band's background count, the tasks line, or Enter on a running background card. <leader>k asks before stopping everything left running."
        />
      )}
      {claudeSlashCommands && (
        <Section
          title="Claude's question"
          headingRef={questionHeading}
          rows={QUESTION_KEYS}
          note="While Claude asks, the question takes the box's place; only keys you press answer it."
        />
      )}
      <Section title="Anywhere in the window" rows={windowKeys} />
      <Section
        title={`After ${prefixLabel}`}
        headingRef={prefixHeading}
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
