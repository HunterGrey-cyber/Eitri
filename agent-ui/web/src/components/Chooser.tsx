import { useEffect, useLayoutEffect, useRef, useState } from "react";
import type { KeyboardEvent, ReactNode, RefObject } from "react";
import type { ChooserEnvelope, PermissionModeChoice, TabId, TabInfo } from "../types";
import { choosable, chooserRows, pickerStep, relativeWhen, resumeMode, rowKey, tabStateWord } from "../chooser";
import type { ChooserRow } from "../chooser";
import { shortId } from "./SessionRow";
import { isShiftTab, modeFixedMessage, modeKeyRoute } from "../modeKey";
import { isModifierKey } from "../typingGuard";
import { NO_TEXT_ASSIST } from "../textField";

type Props = {
  envelope: ChooserEnvelope;
  tabs: TabInfo[];
  active: TabId | null;
  defaultMode: PermissionModeChoice;
  projectDir: string;
  newTabChord: string;
  /** Bumped by `App` on regained pane focus, `enter_input` and `arrive` to restore chooser focus.
   *  Focus the rename input if open, else the filter input if open, else the root.
   *  Mount also focuses, regardless of the initial counter value. */
  focusRequest: number;
  /** Bumped by `dropPendingKeys()` for cancellation routes this chooser never receives as keys:
   *  focus changes, arrival, HINT, tab switches, and other overlays. Clears its lone-g wait along
   *  with the panel's prefix, count, and leader sequence. Shift+Tab clears the wait in `onKeyDown`.
   *  Optional for callers that need no external cancellation. */
  dropKeysRequest?: number;
  onSwitch: (tab: TabId) => void;
  onResume: (providerSessionId: string) => void;
  onNewSession: () => void;
  onCloseTab: (tab: TabId) => void;
  onRenameTab: (tab: TabId, name: string) => void;
  onCycleMode: () => void;
  /** Shift+Tab on an open tab's row toggles that tab's mode where Rust allows it. Leaving bypass
   *  works on any tab in any state. Entering bypass requires the active tab and asks for
   *  confirmation, answered in the chooser. */
  onCycleTabMode: (tab: TabId) => void;
  onLeave: () => void;
  /** The window-close prompt AND an open bypass confirm both own every key ahead of
   *  everything else here too -- lifted out of `App.tsx` for the same reason `EmptyTab` takes it.
   *  Tried first in `onKeyDown`, ahead of even this component's own Shift+Tab (`cycleMode`): a
   *  `y`/`Y`/anything-else answering the prompt must never also move the chooser's cursor or choose
   *  a row. */
  answerConfirm: (event: KeyboardEvent<HTMLDivElement>) => boolean;
};

/** An IME's own Enter/Esc: `isComposing`, or the legacy 229 some WebKit builds still report. */
function composing(event: KeyboardEvent): boolean {
  return event.nativeEvent.isComposing || event.keyCode === 229;
}

function projectName(dir: string): string {
  const trimmed = dir.replace(/\/+$/, "");
  const segment = trimmed.slice(trimmed.lastIndexOf("/") + 1);
  return segment === "" ? dir : segment;
}

/** Wraps the first case-insensitive match of `needle` in `text` with `<mark>`.
 *  Renders plain text when `needle` is empty or absent from `text`. */
function Highlighted({ text, needle }: { text: string; needle: string }): ReactNode {
  if (needle === "") return text;
  const idx = text.toLowerCase().indexOf(needle);
  if (idx === -1) return text;
  return (
    <>
      {text.slice(0, idx)}
      <mark>{text.slice(idx, idx + needle.length)}</mark>
      {text.slice(idx + needle.length)}
    </>
  );
}

/** Row 1's leading text: the record's name (bold) then its title (muted), or just the title, or
 *  `untitled` (muted) when neither exists. */
function RecordLead({ name, title, needle }: { name: string | null; title: string | null; needle: string }): ReactNode {
  if (name !== null) {
    return (
      <>
        <strong>
          <Highlighted text={name} needle={needle} />
        </strong>
        {title !== null && (
          <span className="chooser-muted">
            {" "}
            <Highlighted text={title} needle={needle} />
          </span>
        )}
      </>
    );
  }
  if (title !== null) return <Highlighted text={title} needle={needle} />;
  return <span className="chooser-muted">untitled</span>;
}

/** The leading number an open tab's `ChooserTab.label` carries (`"<n> <name>"`, `tabs::label`),
 *  preferring the joined `TabInfo.number` when one is present. */
function tabNumber(tab: { tab: TabId; label: string }, info: TabInfo | null): number {
  if (info !== null) return info.number;
  return Number(tab.label.split(" ", 1)[0]) || 0;
}

/** Shows the mode and location for starting or resuming the current row above the keys.
 *  For open tabs, `tabModeRoute` determines what `Shift+Tab` can do from here.
 *  The mode key toggles auto and bypass, so the hint says "toggle".
 *  Uses "auto mode" in the full sentence rather than the shorter wording from `modePill`. */
function modeLine(row: ChooserRow | undefined, active: TabInfo | null, defaultMode: PermissionModeChoice): ReactNode {
  // A filter with no matches leaves `row` undefined. Return no mode line instead of reading
  // `row.kind` and unmounting the panel. `keysLine` handles the same missing row.
  if (row === undefined) return null;
  if (row.kind === "tab") {
    const mode = row.info?.mode ?? "auto";
    const route = tabModeRoute(row, active);
    const tail =
      route === "toggle" ? "(shift+tab to toggle)" : route === "ended" ? "· the session has ended" : "· switch to it to change";
    return (
      <>
        Tab {tabNumber(row.tab, row.info)} runs in <ModeGlyph mode={mode} /> {mode} mode on {tail}
      </>
    );
  }
  const { mode } = resumeMode(active, defaultMode);
  const verb = row.kind === "new" ? "Start" : "Resume";
  return (
    <>
      {verb} in <ModeGlyph mode={mode} /> {mode} mode on (shift+tab to toggle)
    </>
  );
}

/** What `Shift+Tab` on an open tab's row does, matching what Rust's
 *  `TabSet::cycle_mode` would do with it: leaving bypass works on any tab in any state; entering it
 *  is refused on an ended/failed tab (`modeKeyRoute`'s "fixed") and on a tab that is not the one on
 *  screen -- both said here instead of sending a
 *  command Rust would only refuse into a banner hidden under this overlay. */
function tabModeRoute(row: Extract<ChooserRow, { kind: "tab" }>, active: TabInfo | null): "toggle" | "ended" | "elsewhere" {
  const info = row.info;
  const route = modeKeyRoute({ confirmOpen: false, chooserOpen: false, tabState: info?.state ?? null, tabMode: info?.mode ?? null });
  if (route === "fixed") return "ended";
  if (info?.mode === "bypass") return "toggle";
  return active !== null && row.tab.tab === active.id ? "toggle" : "elsewhere";
}

/** The `⏵⏵` glyph, split into its own span so the font stack and colour rules stay
 *  scoped to `.mode-glyph` alone -- the same span `StatusBand` already draws it with. */
function ModeGlyph({ mode }: { mode: PermissionModeChoice }) {
  return (
    <span className="mode-glyph" data-mode-name={mode}>
      ⏵⏵
    </span>
  );
}

/** The keys line, worded for the row under the cursor: only an open tab offers
 *  `ctrl+r rename` and `x close tab`, and only an open tab reads `enter switch` rather than
 *  `enter resume`. */
function keysLine(row: ChooserRow | undefined): string {
  const parts = [row?.kind === "tab" ? "enter switch" : "enter resume", "/ filter"];
  if (row?.kind === "tab") parts.push("ctrl+r rename", "x close tab");
  parts.push("shift+tab mode", "esc");
  return parts.join(" · ");
}

/** `prefix w`. Owns every key while open, like the `?` overlay. Copies Claude Code's
 *  `/resume` picker (two-line rows, `/` to filter, `ctrl+r to rename`) with tmux `choose-tree`'s
 *  open-windows-first order. */
export function Chooser({
  envelope,
  tabs,
  active,
  defaultMode,
  projectDir,
  newTabChord,
  focusRequest,
  dropKeysRequest,
  onSwitch,
  onResume,
  onNewSession,
  onCloseTab,
  onRenameTab,
  onCycleMode,
  onCycleTabMode,
  onLeave,
  answerConfirm,
}: Props) {
  const [filter, setFilter] = useState("");
  const [filtering, setFiltering] = useState(false);
  const [renaming, setRenaming] = useState<{ tab: TabId; value: string } | null>(null);
  // A local flash: why `Shift+Tab` on an open tab's row did nothing (`tabModeRoute`). Not the
  // window's own `showFlash`/footer -- the chooser is a full overlay drawn over it (`.chooser` is
  // `position: absolute; inset: 0`), so that flash would be invisible while this is open.
  const [flash, setFlash] = useState<{ text: string; seq: number } | null>(null);
  const flashSeq = useRef(0);
  useEffect(() => {
    if (flash === null) return;
    const seq = flash.seq;
    const timer = setTimeout(() => setFlash((f) => (f?.seq === seq ? null : f)), 2000);
    return () => clearTimeout(timer);
  }, [flash]);
  function showFlash(text: string) {
    flashSeq.current += 1;
    setFlash({ text, seq: flashSeq.current });
  }

  // Defect 5: starts on the active tab's row, not row 1.
  const [cursor, setCursor] = useState(() => {
    const rows = chooserRows(envelope, tabs, "");
    return Math.max(0, rows.findIndex((r) => r.kind === "tab" && r.tab.tab === active));
  });
  const rootRef = useRef<HTMLDivElement>(null);
  const filterRef = useRef<HTMLInputElement>(null);
  const renameRef = useRef<HTMLInputElement>(null);
  const needle = filter.trim().toLowerCase();
  const rows = chooserRows(envelope, tabs, filter);
  const current: ChooserRow | undefined = rows[Math.min(cursor, rows.length - 1)];
  // The cursor is an index into the filtered rows, and a new envelope (titles read after the chooser opened)
  // can change which rows a filter matches: then the cursor goes back to the row it was on, by what the row
  // is, so Enter never chooses a row the user did not move to. A row that left the list leaves the index.
  const anchor = useRef<{ envelope: ChooserEnvelope; key: string | null }>({ envelope, key: null });
  useLayoutEffect(() => {
    let key = current === undefined ? null : rowKey(current);
    if (anchor.current.envelope !== envelope && anchor.current.key !== null) {
      const at = rows.findIndex((r) => rowKey(r) === anchor.current.key);
      if (at >= 0) {
        if (at !== Math.min(cursor, rows.length - 1)) setCursor(at);
        key = anchor.current.key;
      }
    }
    anchor.current = { envelope, key };
  });
  const activeInfo = tabs.find((t) => t.id === active) ?? null;

  /** Restores focus on mount and each `focusRequest` change to the active rename input,
   *  filter input or root. Do not call `select()` here: regaining focus must preserve an
   *  unfinished value and its selection. The `[renaming]` effect selects on a fresh rename. */
  useEffect(() => {
    if (renaming !== null) renameRef.current?.focus();
    else if (filtering) filterRef.current?.focus();
    else rootRef.current?.focus();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [focusRequest]);
  useEffect(() => {
    if (filtering) filterRef.current?.focus();
    else if (renaming === null) rootRef.current?.focus();
  }, [filtering, renaming]);
  useEffect(() => {
    if (renaming !== null) {
      renameRef.current?.focus();
      renameRef.current?.select();
    }
  }, [renaming]);
  useEffect(() => {
    rootRef.current?.querySelector(".chooser-row.current")?.scrollIntoView({ block: "nearest" });
  }, [cursor, filter]);

  /** One step of the list cursor, clamped to the rows: an empty list clamps to 0 (a `-1` left no row
   *  under the cursor once the filter was cleared again). */
  function moveCursor(step: 1 | -1) {
    setCursor((c) => Math.max(0, Math.min(c + step, rows.length - 1)));
  }

  function choose(row: ChooserRow | undefined) {
    if (row === undefined || !choosable(row)) return;
    if (row.kind === "new") onNewSession();
    else if (row.kind === "tab") onSwitch(row.tab.tab);
    else onResume(row.record.providerSessionId);
  }

  function cycleMode(row: ChooserRow | undefined) {
    if (row === undefined) return;
    if (row.kind !== "tab") {
      onCycleMode();
      return;
    }
    const route = tabModeRoute(row, activeInfo);
    if (route === "toggle") onCycleTabMode(row.tab.tab);
    else if (route === "ended") showFlash(modeFixedMessage("r"));
    else showFlash(`switch to tab ${tabNumber(row.tab, row.info)} to change its mode`);
  }

  // A lone `g` waits without a timeout. The next key completes `gg` or is swallowed to cancel the
  // wait, so `gx` cannot close a tab and `gEnter` cannot choose a row. Bare modifiers keep
  // waiting.
  const pendingG = useRef(false);
  // Clear the pending `g` on the window's cancellation routes. This also runs on mount, when
  // nothing is waiting.
  useEffect(() => {
    pendingG.current = false;
  }, [dropKeysRequest]);
  function onKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    if (composing(event)) return;
    // A bypass or window-close confirmation takes every key before chooser navigation, filter or
    // rename inputs, and Shift+Tab. Answering a y/n prompt must never move the cursor or choose
    // a row.
    if (answerConfirm(event)) return;
    // Claim Shift+Tab even in the filter or rename input. The document-capture router sends it here
    // while the chooser is open, and `cycleMode` selects the row-dependent action. Handle it
    // before the text-input guard because Shift+Tab types no text.
    if (isShiftTab(event)) {
      event.preventDefault();
      event.stopPropagation();
      // Shift+Tab performs an action here, so it ends a pending `g` wait. A bare modifier leaves
      // the wait intact.
      pendingG.current = false;
      cycleMode(current);
      return;
    }
    if (filtering || renaming !== null) return;
    // Map Down and Ctrl+n to `j`, and Up and Ctrl+p to `k`, through the full handling below,
    // including a pending `g` and the allow-list.
    const step = pickerStep(event);
    const key = step === null ? event.key : step > 0 ? "j" : "k";
    if (pendingG.current) {
      if (isModifierKey(event.key)) return;
      pendingG.current = false;
      event.preventDefault();
      event.stopPropagation();
      if (key === "g") setCursor(0);
      return;
    }
    // A `g` held with Ctrl, Alt or Meta is a chord for something else: it arms no `gg` wait, which would
    // swallow the next plain key (rc.3 minors review, the same rule as the panel's own g/z/[/]).
    if (key === "g" && (event.ctrlKey || event.altKey || event.metaKey)) return;
    if (key === "g" || key === "G") {
      event.preventDefault();
      event.stopPropagation();
      if (key === "G") setCursor(Math.max(rows.length - 1, 0));
      else pendingG.current = true;
      return;
    }
    if (event.ctrlKey && key === "r") {
      event.preventDefault();
      event.stopPropagation();
      if (current?.kind === "tab") setRenaming({ tab: current.tab.tab, value: current.tab.label });
      return;
    }
    if (!["j", "k", "Enter", "x", "/", "Escape", "q"].includes(key)) return;
    event.preventDefault();
    event.stopPropagation();
    if (key === "j") moveCursor(1);
    else if (key === "k") moveCursor(-1);
    else if (key === "Enter") choose(current);
    else if (key === "x") {
      if (current?.kind === "tab") onCloseTab(current.tab.tab);
      // else: `x` is only bound on an open tab's row (the keys line says so); do nothing.
    } else if (key === "/") setFiltering(true);
    else if (key === "Escape" || key === "q") onLeave();
  }

  const shownCount = rows.filter((r) => r.kind !== "new").length;
  const totalCount = envelope.open.length + envelope.records.length;
  const project = projectName(projectDir);

  return (
    <div className="chooser" role="dialog" aria-label="Choose a session" tabIndex={-1} ref={rootRef} onKeyDown={onKeyDown}>
      <div className="chooser-header">
        <span className="chooser-title">Sessions</span>
        <span className="chooser-project">{project}</span>
        <span className="chooser-counts">
          {needle === "" ? `${envelope.open.length} open · ${envelope.records.length} earlier` : `${shownCount} of ${totalCount}`}
        </span>
      </div>
      {filtering || filter !== "" ? (
        <input
          ref={filterRef}
          {...NO_TEXT_ASSIST}
          className="chooser-filter"
          aria-label="Filter sessions"
          placeholder="filter by name or title"
          value={filter}
          onChange={(e) => {
            setFilter(e.target.value);
            setCursor(0);
          }}
          onKeyDown={(event) => {
            if (composing(event)) return;
            // The four list-motion keys keep focus in the filter box while moving the list. `j`/`k`
            // remain typed text here, so they cannot navigate while filtering.
            const step = pickerStep(event);
            if (step !== null) {
              event.preventDefault();
              event.stopPropagation();
              moveCursor(step);
              return;
            }
            if (event.key === "Enter" || event.key === "Escape") {
              event.preventDefault();
              event.stopPropagation();
              if (event.key === "Escape") setFilter("");
              setFiltering(false);
            }
          }}
        />
      ) : null}
      <ul className="chooser-list">
        {(() => {
          let group: "open" | "record" | null = null;
          const items: ReactNode[] = [];
          rows.forEach((row, index) => {
            const nextGroup = row.kind === "record" ? "record" : "open";
            if (nextGroup !== group) {
              items.push(
                <li key={`group-${nextGroup}`} className="chooser-group" aria-hidden="true">
                  {nextGroup === "open" ? "Open in this window" : `Earlier in ${project}`}
                </li>,
              );
            }
            group = nextGroup;
            items.push(
              <li key={`row-${index}`}>
                <ChooserRowItem
                  row={row}
                  current={index === cursor}
                  needle={needle}
                  newTabChord={newTabChord}
                  renaming={renaming}
                  renameRef={renameRef}
                  onClick={() => choose(row)}
                  onRenameChange={(value) => setRenaming((r) => (r === null ? r : { ...r, value }))}
                  onRenameCommit={() => {
                    if (renaming !== null) onRenameTab(renaming.tab, renaming.value);
                    setRenaming(null);
                  }}
                  onRenameCancel={() => setRenaming(null)}
                />
              </li>,
            );
          });
          return items;
        })()}
        {rows.length === 0 && <li className="chooser-empty">nothing matches</li>}
      </ul>
      <div className="chooser-mode-line">{flash !== null ? flash.text : modeLine(current, activeInfo, defaultMode)}</div>
      <div className="chooser-keys">{keysLine(current)}</div>
    </div>
  );
}

function ChooserRowItem({
  row,
  current,
  needle,
  newTabChord,
  renaming,
  renameRef,
  onClick,
  onRenameChange,
  onRenameCommit,
  onRenameCancel,
}: {
  row: ChooserRow;
  current: boolean;
  needle: string;
  newTabChord: string;
  renaming: { tab: TabId; value: string } | null;
  renameRef: RefObject<HTMLInputElement>;
  onClick: () => void;
  onRenameChange: (value: string) => void;
  onRenameCommit: () => void;
  onRenameCancel: () => void;
}) {
  const classes = ["chooser-row", current ? "current" : "", choosable(row) ? "" : "unchoosable"].filter(Boolean).join(" ");
  // The current row replaces its usual sign with the panel cursor: a solid block containing `›`.
  const cursorSign = <span className="chooser-sign">›</span>;
  if (row.kind === "new") {
    return (
      <div className={classes} onClick={onClick}>
        {current ? cursorSign : <span className="chooser-sign" />}
        <span className="chooser-line1">
          <span className="chooser-lead">+ New session</span>
          <span className="chooser-right chooser-muted">{newTabChord}</span>
        </span>
      </div>
    );
  }
  if (row.kind === "tab") {
    const isRenaming = renaming !== null && renaming.tab === row.tab.tab;
    const word = tabStateWord(row.info);
    const sign = word === "running" ? "running" : word === "done, unread" ? "unread" : null;
    const mode = row.info?.mode ?? "auto";
    const title = row.info?.title ?? null;
    // Bypass is the one mode worth a glance here; auto is the default and carries nothing new. The
    // backend name is not drawn here at all: it lives only in `prefix i` / `<leader>i`
    // (`DetailPopover`).
    const bypass = mode === "bypass";
    return (
      <div className={classes} onClick={onClick}>
        {current ? cursorSign : <span className={sign === null ? "chooser-sign" : `chooser-sign chooser-sign-${sign}`} />}
        <span className="chooser-line1">
          {isRenaming ? (
            <input
              ref={renameRef}
              {...NO_TEXT_ASSIST}
              className="chooser-rename"
              aria-label={`Rename tab ${row.tab.tab}`}
              value={renaming!.value}
              onClick={(e) => e.stopPropagation()}
              onChange={(e) => onRenameChange(e.target.value)}
              onKeyDown={(event) => {
                if (event.nativeEvent.isComposing || event.keyCode === 229) return;
                if (event.key === "Enter") {
                  event.preventDefault();
                  event.stopPropagation();
                  onRenameCommit();
                } else if (event.key === "Escape") {
                  event.preventDefault();
                  event.stopPropagation();
                  onRenameCancel();
                }
              }}
            />
          ) : (
            <span className="chooser-lead">
              <Highlighted text={row.tab.label} needle={needle} />
            </span>
          )}
          <span className="chooser-right">
            {row.tab.marker === "needs_input" && `⚑${row.tab.pending > 1 ? row.tab.pending : ""} `}
            {/* A not-started tab's label already says new (`tabs::label`), so its state word would repeat it. */}
            {word !== "new" && word}
          </span>
        </span>
        {(bypass || title !== null) && (
          <span className="chooser-line2 chooser-muted">
            {/* The same `.mode-glyph[data-mode-name="bypass"]` token the band and the mode line
                above already colour bypass with (`--nv-error`) -- no new colour invented. */}
            {bypass && (
              <>
                <ModeGlyph mode="bypass" /> bypass
              </>
            )}
            {bypass && title !== null && " · "}
            {title}
          </span>
        )}
      </div>
    );
  }
  const record = row.record;
  return (
    <div className={classes} onClick={onClick}>
      {current ? cursorSign : <span className="chooser-sign">{record.heldElsewhere ? "⊘" : ""}</span>}
      <span className="chooser-line1">
        <span className="chooser-lead">
          <RecordLead name={record.name} title={record.title} needle={needle} />
        </span>
        <span className="chooser-right">{relativeWhen(Date.now(), record.updatedAt)}</span>
      </span>
      <span className="chooser-line2 chooser-muted">
        {/* The short id distinguishes untitled sessions. The backend name adds no distinction in
            release builds. */}
        {record.heldElsewhere ? "open in another window, can't be resumed here" : shortId(record.providerSessionId)}
      </span>
    </div>
  );
}
