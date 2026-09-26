import { useEffect, useRef, useState } from "react";
import type { KeyboardEvent } from "react";
import type { ChooserEnvelope, TabId } from "../types";
import { choosable, chooserRows, rowText } from "../chooser";
import type { ChooserRow } from "../chooser";

type Props = {
  envelope: ChooserEnvelope;
  active: TabId | null;
  onSwitch: (tab: TabId) => void;
  onResume: (providerSessionId: string) => void;
  onCloseTab: (tab: TabId) => void;
  onLeave: (launch: boolean) => void;
};

function composing(event: KeyboardEvent): boolean {
  return event.nativeEvent.isComposing || event.keyCode === 229;
}

/** `prefix w` (spec §3.6). Owns every key while open, like the `?` overlay. */
export function Chooser({ envelope, active, onSwitch, onResume, onCloseTab, onLeave }: Props) {
  const [filter, setFilter] = useState("");
  const [filtering, setFiltering] = useState(false);
  // Defect 5 (ruling 37): starts on the active tab's row, not row 1.
  const [cursor, setCursor] = useState(() => {
    const rows = chooserRows(envelope, "");
    return Math.max(0, rows.findIndex((r) => r.kind === "tab" && r.tab.tab === active));
  });
  const rootRef = useRef<HTMLDivElement>(null);
  const filterRef = useRef<HTMLInputElement>(null);
  const rows = chooserRows(envelope, filter);
  const current: ChooserRow | undefined = rows[Math.min(cursor, rows.length - 1)];

  useEffect(() => {
    rootRef.current?.focus();
  }, []);
  useEffect(() => {
    if (filtering) filterRef.current?.focus();
    else rootRef.current?.focus();
  }, [filtering]);
  useEffect(() => {
    rootRef.current?.querySelector(".chooser-row.current")?.scrollIntoView({ block: "nearest" });
  }, [cursor, filter]);

  function choose(row: ChooserRow | undefined) {
    if (row === undefined || !choosable(row)) return;
    if (row.kind === "tab") onSwitch(row.tab.tab);
    else onResume(row.record.providerSessionId);
  }

  function onKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    if (filtering || composing(event)) return;
    const key = event.key;
    if (!["j", "k", "Enter", "x", "/", "Escape", "q"].includes(key)) return;
    event.preventDefault();
    event.stopPropagation();
    if (key === "j") setCursor((c) => Math.min(c + 1, rows.length - 1));
    else if (key === "k") setCursor((c) => Math.max(c - 1, 0));
    else if (key === "Enter") choose(current);
    else if (key === "x" && current?.kind === "tab") onCloseTab(current.tab.tab);
    else if (key === "/") setFiltering(true);
    else onLeave(envelope.launch);
  }

  return (
    <div className="chooser" role="dialog" aria-label="Choose a session" tabIndex={-1} ref={rootRef} onKeyDown={onKeyDown}>
      {filtering || filter !== "" ? (
        <input
          ref={filterRef}
          className="chooser-filter"
          aria-label="Filter sessions"
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
        {rows.map((row, index) => (
          <li
            key={row.kind === "tab" ? `tab-${row.tab.tab}` : `rec-${row.record.providerSessionId}`}
            className={["chooser-row", index === cursor ? "current" : "", choosable(row) ? "" : "unchoosable"].filter(Boolean).join(" ")}
            aria-current={index === cursor || undefined}
            onClick={() => choose(row)}
          >
            {rowText(row)}
          </li>
        ))}
        {rows.length === 0 && <li className="chooser-empty">nothing matches</li>}
      </ul>
    </div>
  );
}
