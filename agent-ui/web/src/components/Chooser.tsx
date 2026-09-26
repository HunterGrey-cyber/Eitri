import { useEffect, useRef, useState } from "react";
import type { KeyboardEvent, ReactNode, RefObject } from "react";
import type { BackendKind, ChooserEnvelope, PermissionModeChoice, TabId, TabInfo } from "../types";
import { choosable, chooserRows, relativeWhen, resumeMode, tabStateWord } from "../chooser";
import type { ChooserRow } from "../chooser";
import { shortId } from "./SessionRow";

type Props = {
  envelope: ChooserEnvelope;
  tabs: TabInfo[];
  active: TabId | null;
  defaultMode: PermissionModeChoice;
  backend: BackendKind;
  projectDir: string;
  newTabChord: string;
  /** Bumped by `App` on regained pane focus, `enter_input` and `arrive` to restore chooser focus.
   *  Focus the rename input if open, else the filter input if open, else the root.
   *  Mount also focuses, regardless of the initial counter value. */
  focusRequest: number;
  onSwitch: (tab: TabId) => void;
  onResume: (providerSessionId: string) => void;
  onNewSession: () => void;
  onCloseTab: (tab: TabId) => void;
  onRenameTab: (tab: TabId, name: string) => void;
  onCycleMode: () => void;
  onLeave: () => void;
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
 *  An open tab's mode is fixed for its session. Uses "auto mode" in the full sentence rather
 *  than the shorter wording from `modePill`. */
function modeLine(row: ChooserRow, active: TabInfo | null, defaultMode: PermissionModeChoice): ReactNode {
  if (row.kind === "tab") {
    const mode = row.info?.mode ?? "auto";
    return (
      <>
        Tab {tabNumber(row.tab, row.info)} runs in <ModeGlyph mode={mode} /> {mode} mode on · fixed
      </>
    );
  }
  const { mode } = resumeMode(active, defaultMode);
  const verb = row.kind === "new" ? "Start" : "Resume";
  return (
    <>
      {verb} in <ModeGlyph mode={mode} /> {mode} mode on (shift+tab to cycle)
    </>
  );
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
  backend,
  projectDir,
  newTabChord,
  focusRequest,
  onSwitch,
  onResume,
  onNewSession,
  onCloseTab,
  onRenameTab,
  onCycleMode,
  onLeave,
}: Props) {
  const [filter, setFilter] = useState("");
  const [filtering, setFiltering] = useState(false);
  const [renaming, setRenaming] = useState<{ tab: TabId; value: string } | null>(null);
  // Keep the flash inside the chooser: this full overlay covers the window's footer, so `showFlash`
  // there would be invisible while it is open.
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

  function choose(row: ChooserRow | undefined) {
    if (row === undefined || !choosable(row)) return;
    if (row.kind === "new") onNewSession();
    else if (row.kind === "tab") onSwitch(row.tab.tab);
    else onResume(row.record.providerSessionId);
  }

  function cycleMode(row: ChooserRow | undefined) {
    if (row === undefined) return;
    if (row.kind === "tab") showFlash("mode is fixed for this session");
    else onCycleMode();
  }

  // A lone `g` waits for the second `g`. Any other key clears the wait and then performs its
  // normal action.
  const pendingG = useRef(false);
  function onKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    if (composing(event)) return;
    // Claim Shift+Tab even in the filter or rename input. The document-capture router sends it here
    // while the chooser is open, and `cycleMode` selects the row-dependent action. Handle it
    // before the text-input guard because Shift+Tab types no text.
    if (event.shiftKey && event.key === "Tab") {
      event.preventDefault();
      event.stopPropagation();
      cycleMode(current);
      return;
    }
    if (filtering || renaming !== null) return;
    const key = event.key;
    if (key === "g" || key === "G") {
      event.preventDefault();
      event.stopPropagation();
      if (key === "G") setCursor(Math.max(rows.length - 1, 0));
      else if (pendingG.current) setCursor(0);
      pendingG.current = key === "g" && !pendingG.current;
      return;
    }
    pendingG.current = false;
    if (event.ctrlKey && key === "r") {
      event.preventDefault();
      event.stopPropagation();
      if (current?.kind === "tab") setRenaming({ tab: current.tab.tab, value: current.tab.label });
      return;
    }
    if (!["j", "k", "Enter", "x", "/", "Escape", "q"].includes(key)) return;
    event.preventDefault();
    event.stopPropagation();
    if (key === "j") setCursor((c) => Math.min(c + 1, rows.length - 1));
    else if (key === "k") setCursor((c) => Math.max(c - 1, 0));
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
                  backend={backend}
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
  backend,
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
  backend: BackendKind;
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
    return (
      <div className={classes} onClick={onClick}>
        {current ? cursorSign : <span className={sign === null ? "chooser-sign" : `chooser-sign chooser-sign-${sign}`} />}
        <span className="chooser-line1">
          {isRenaming ? (
            <input
              ref={renameRef}
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
            {row.tab.marker === "needs_input" && `⚑${row.tab.pending > 1 ? row.tab.pending : ""} · `}
            {word}
          </span>
        </span>
        <span className="chooser-line2 chooser-muted">
          <ModeGlyph mode={mode} /> {mode} · {backend}
          {title !== null && ` · ${title}`}
        </span>
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
        {record.heldElsewhere ? "open in another window, can't be resumed here" : `sidecar · ${shortId(record.providerSessionId)}`}
      </span>
    </div>
  );
}
