import { useEffect, useRef, useState } from "react";
import type { KeyboardEvent, ReactNode, RefObject } from "react";
import type { ChooserEnvelope, PermissionModeChoice, TabId, TabInfo } from "../types";
import { choosable, chooserRows, pickerStep, relativeWhen, resumeMode, tabStateWord } from "../chooser";
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
  /** Wave 3 Task 1: bumped by `App` on `pane_focus` regaining focus, `enter_input` and `arrive`,
   *  so a chooser that lost DOM focus across a GTK round trip gets it back -- the rename input if
   *  one is open, else the filter input if one is open, else the root. Mount counts as a request
   *  too (initial value 0 or otherwise -- the effect below always runs on mount, like the old
   *  mount-only effect it replaces). */
  focusRequest: number;
  /** rc.3 minors (K01): bumped by `App.tsx`'s `dropPendingKeys()` on every cancel route this overlay
   *  never sees as a key of its own -- a focus round trip (`pane_focus`), `arrive`, a HINT, a tab
   *  switch, another overlay. It ends the chooser's own lone-`g` wait, as those routes end the
   *  panel's waiting prefix, count and leader sequence (`Shift+Tab`, which the chooser does see, ends
   *  it in `onKeyDown`). Optional, so this component's own tests need no stand-in. */
  dropKeysRequest?: number;
  onSwitch: (tab: TabId) => void;
  onResume: (providerSessionId: string) => void;
  onNewSession: () => void;
  onCloseTab: (tab: TabId) => void;
  onRenameTab: (tab: TabId, name: string) => void;
  onCycleMode: () => void;
  /** `Shift+Tab` on an open tab's own row (v1-mode fix round 1): toggles THAT tab's mode -- D6 made
   *  an open tab's mode switchable, so the old "fixed for this session" flash was false. Only offered
   *  where Rust would act on it: leaving bypass (any tab, any state), or entering it on the active
   *  tab (Rust asks first; the prompt is answered here, in the chooser). */
  onCycleTabMode: (tab: TabId) => void;
  onLeave: () => void;
  /** The window-close prompt AND (v1, spec §3.4) an open bypass confirm both own every key ahead of
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

/** Wraps the first case-insensitive match of `needle` in `text` with `<mark>` (spec §6.1: "matches
 *  are marked with `<mark>`"). Renders `text` plain when `needle` is empty or does not occur. */
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
 *  `untitled` (muted) when neither exists (spec §6.1). */
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

/** The mode line, above the keys (spec §6.1, decision 3/6): what a resume of the row under the
 *  cursor will run in, and where. An open tab's own mode is no longer "fixed" (v1 D6, fix round 1):
 *  the line says what `Shift+Tab` does to it from here -- see `tabModeRoute`.
 *
 *  O2 (a): the mode key TOGGLES auto and bypass, so the hint says "toggle" (it said "cycle", after
 *  Claude Code's own N-mode footer).
 *
 *  Deliberately NOT `modePill` (`tabs.ts`) -- that helper's own established wording is `⏵⏵ auto on
 *  (shift+tab to toggle)` (`tabs.test.ts`), one word short of what the round-2 spec §6.1 quoted
 *  (`Resume in ⏵⏵ auto mode on (shift+tab to cycle)`, before O2 froze the key as a toggle): the
 *  chooser's own phrasing names the mode as a noun ("auto mode") before saying it is on, which
 *  reads better in a full sentence than the footer pill's terser form does. */
function modeLine(row: ChooserRow | undefined, active: TabInfo | null, defaultMode: PermissionModeChoice): ReactNode {
  // K03 (kbux 2026-09-29, S07.4b): a filter matching nothing leaves no row under the cursor, and this read
  // `row.kind` on `undefined` -- the throw unmounted the whole panel. No row, no line (`keysLine` below
  // takes the same `undefined`).
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

/** What `Shift+Tab` on an open tab's row does (v1 D6, fix round 1), matching what Rust's
 *  `TabSet::cycle_mode` would do with it: leaving bypass works on any tab in any state; entering it
 *  is refused on an ended/failed tab (`modeKeyRoute`'s "fixed") and on a tab that is not the one on
 *  screen (spec §3.3, "tab N is not the one on screen") -- both said here instead of sending a
 *  command Rust would only refuse into a banner hidden under this overlay. */
function tabModeRoute(row: Extract<ChooserRow, { kind: "tab" }>, active: TabInfo | null): "toggle" | "ended" | "elsewhere" {
  const info = row.info;
  const route = modeKeyRoute({ confirmOpen: false, chooserOpen: false, tabState: info?.state ?? null, tabMode: info?.mode ?? null });
  if (route === "fixed") return "ended";
  if (info?.mode === "bypass") return "toggle";
  return active !== null && row.tab.tab === active.id ? "toggle" : "elsewhere";
}

/** The `⏵⏵` glyph, split into its own span (spec §9) so the font stack and colour rules stay
 *  scoped to `.mode-glyph` alone -- the same span `StatusBand` already draws it with. */
function ModeGlyph({ mode }: { mode: PermissionModeChoice }) {
  return (
    <span className="mode-glyph" data-mode-name={mode}>
      ⏵⏵
    </span>
  );
}

/** The keys line, worded for the row under the cursor (spec §6.1): only an open tab offers
 *  `ctrl+r rename` and `x close tab`, and only an open tab reads `enter switch` rather than
 *  `enter resume`. */
function keysLine(row: ChooserRow | undefined): string {
  const parts = [row?.kind === "tab" ? "enter switch" : "enter resume", "/ filter"];
  if (row?.kind === "tab") parts.push("ctrl+r rename", "x close tab");
  parts.push("shift+tab mode", "esc");
  return parts.join(" · ");
}

/** `prefix w` (spec §3.6). Owns every key while open, like the `?` overlay. Copies Claude Code's
 *  `/resume` picker (two-line rows, `/` to filter, `ctrl+r to rename`) with tmux `choose-tree`'s
 *  open-windows-first order (spec §6). */
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

  // Defect 5 (ruling 37): starts on the active tab's row, not row 1.
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

  /** Wave 3 Task 1: replaces the old mount-only focus effect (`focusRequest` starts at some value
   *  either way, so mount still focuses). Re-focuses whichever of the rename input, the filter
   *  input or the root currently has the live control, on every bump from `App` -- not just once.
   *  No `select()` here (that would clobber a half-typed rename/filter value on a mere re-focus);
   *  the `[renaming]` effect below still selects, since ITS trigger really is a fresh rename. */
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

  // Spec §6.2's `gg` (r2-gui GUI pass, 2026-09-26). R1 (v1 picks, K01): a lone `g` waits for its next key
  // with no timeout, and that key either completes `gg` or cancels the wait -- swallowed, nothing runs,
  // as vim's `nv_g_cmd` does for a `g` no command follows. It used to drop the `g` and then do what the
  // key does, so `gx` asked to close a tab and `gEnter` chose a row. A bare modifier is not "the next
  // key" (the Shift of a `G`).
  const pendingG = useRef(false);
  // rc.3 minors (K01): the window's own cancel routes (`dropKeysRequest`'s doc). Also runs on mount,
  // where there is nothing waiting yet.
  useEffect(() => {
    pendingG.current = false;
  }, [dropKeysRequest]);
  function onKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    if (composing(event)) return;
    // v1, spec §3.4: a bypass (or window-close) confirm owns every key ahead of everything else in
    // here too, even the filter/rename inputs and this component's own Shift+Tab below -- `j`/`k`/
    // `Enter` must never move the cursor or choose a row while it is answering a y/n instead.
    if (answerConfirm(event)) return;
    // Wave 4 Task 1: Shift+Tab is claimed here even while the filter or rename input is focused --
    // App.tsx's document-capture router (`modeKey.ts`) already routed here (`route === "overlay"`)
    // because the chooser is open, and this component's own row-dependent choice (`cycleMode`) still
    // decides between `cycle_mode` and `cycle_default_mode`. Ahead of the filtering/renaming guard,
    // deliberately: that guard exists to stop navigation keys from fighting a typed value, but
    // Shift+Tab types nothing into either box.
    if (isShiftTab(event)) {
      event.preventDefault();
      event.stopPropagation();
      // K01: Shift+Tab is a key after a waiting `g`, and (unlike a bare modifier) it does something
      // here -- so it ends the wait, as it drops the panel's own pending keys (`App.tsx`).
      pendingG.current = false;
      cycleMode(current);
      return;
    }
    if (filtering || renaming !== null) return;
    // R12: `↓` and `Ctrl+n` are `j`, `↑` and `Ctrl+p` are `k`, and take the whole path below -- a pending
    // `g` and the allow-list included.
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
            // R12: the four list keys move the list from the filter box too, and it keeps the keys --
            // `j`/`k` are typed text in here, so these are the only way to move while filtering.
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
  // Spec §6.1: the current row's sign cell is the panel's cursor, a solid block holding `›`, in
  // place of whatever sign the row has (r2-gui GUI pass, 2026-09-26: it was never drawn).
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
    // v1 trial item 1 (owner: "⏵⏵ auto · sidecar，这个东西应该出现在all session的选择上吗，是参考了别人的
    // 设计，还是我们自己的设计失误" -- our own design mistake, not copied): bypass is the one mode worth
    // a glance here (auto is the default and carries nothing new); the backend name is gone
    // entirely (decision 6: it lives only in `prefix i` / `<leader>i`, `DetailPopover`).
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
            {/* An empty (not-started) tab's own label already says "new" (`tabs::label`); its state
                word would only repeat it (v1 trial item 1). */}
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
        {/* v1 trial item 1: no longer "sidecar · <id>" -- the backend carries nothing (every
            release build reads "sidecar"), the short id is what tells an untitled record apart. */}
        {record.heldElsewhere ? "open in another window, can't be resumed here" : shortId(record.providerSessionId)}
      </span>
    </div>
  );
}
