import { useEffect, useRef, useState } from "react";
import type { KeyboardEvent } from "react";
import type { TabId, TabInfo } from "../types";
import { markerGlyph } from "../tabs";
import { NO_TEXT_ASSIST } from "../textField";

type Props = {
  tabs: TabInfo[];
  active: TabId;
  renaming: { tab: TabId; initial: string } | null;
  /** Wave 3 Task 1: bumped by `App` on `pane_focus` regaining focus, so a rename field that lost
   *  DOM focus across a GTK round trip gets it back. `focus()` only -- a re-focus must not
   *  re-select the text the user may already be part way through typing. */
  focusRequest: number;
  onSelect: (tab: TabId) => void;
  onRenameCommit: (name: string) => void;
  onRenameCancel: () => void;
};

/** An IME's own Enter/Esc: `isComposing`, or the legacy 229 some WebKit builds still report. */
function composing(event: KeyboardEvent): boolean {
  return event.nativeEvent.isComposing || event.keyCode === 229;
}

/** tmux's window list, at the top of the panel (spec §3.3). One nav stop: `k` from the first row
 *  reaches it, `h`/`l` move between tabs, `Enter` (a focused button's click) selects. HINT labels
 *  each tab as a control. `App` renders it only with two or more tabs, or during a rename. */
export function TabBar({ tabs, active, renaming, focusRequest, onSelect, onRenameCommit, onRenameCancel }: Props) {
  const activeRef = useRef<HTMLElement | null>(null);
  useEffect(() => {
    activeRef.current?.scrollIntoView({ block: "nearest", inline: "nearest" });
  }, [active, tabs.length]);
  return (
    <div className="tab-bar" data-nav-stop="tabs" role="tablist" aria-label="Session tabs">
      {tabs.map((tab) => {
        const isActive = tab.id === active;
        const classes = ["tab", isActive ? "tab-active" : "", tab.marker === "ended" ? "tab-ended" : ""].filter(Boolean).join(" ");
        if (renaming !== null && renaming.tab === tab.id) {
          return (
            <RenameField
              key={tab.id}
              number={tab.number}
              initial={renaming.initial}
              focusRequest={focusRequest}
              onCommit={onRenameCommit}
              onCancel={onRenameCancel}
            />
          );
        }
        const glyph = markerGlyph(tab.marker, tab.pending);
        return (
          <button
            key={tab.id}
            type="button"
            role="tab"
            aria-selected={isActive}
            className={classes}
            ref={isActive ? (el) => (activeRef.current = el) : undefined}
            onClick={() => onSelect(tab.id)}
          >
            {tab.label}
            {glyph !== "" && ` ${glyph}`}
            {tab.marker === "working" && <span className="tab-working" aria-label="working" />}
          </button>
        );
      })}
    </div>
  );
}

function RenameField({
  number,
  initial,
  focusRequest,
  onCommit,
  onCancel,
}: {
  number: number;
  initial: string;
  focusRequest: number;
  onCommit: (name: string) => void;
  onCancel: () => void;
}) {
  const [value, setValue] = useState(initial);
  const ref = useRef<HTMLInputElement>(null);
  useEffect(() => {
    ref.current?.focus();
    ref.current?.select();
  }, []);
  // Wave 3 Task 1: re-focus only, on every later request -- `select()` here would clobber a
  // half-typed name each time `pane_focus` regains focus.
  useEffect(() => {
    if (focusRequest > 0) ref.current?.focus();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [focusRequest]);
  return (
    <span className="tab tab-active tab-renaming">
      {number}{" "}
      <input
        ref={ref}
        {...NO_TEXT_ASSIST}
        className="tab-rename"
        aria-label={`Rename tab ${number}`}
        value={value}
        onChange={(e) => setValue(e.target.value)}
        onKeyDown={(event) => {
          if (composing(event)) return;
          if (event.key === "Enter") {
            event.preventDefault();
            event.stopPropagation();
            onCommit(value);
          } else if (event.key === "Escape") {
            event.preventDefault();
            event.stopPropagation();
            onCancel();
          }
        }}
      />
    </span>
  );
}
