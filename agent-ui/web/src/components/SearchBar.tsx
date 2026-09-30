import { useEffect, useRef } from "react";
import type { KeyboardEvent } from "react";

type Props = {
  query: string;
  onChange: (q: string) => void;
  /** Enter and Esc. Each gets its own keydown, which this bar stops here, so the caller can still
   *  hand it to the typing guard (K02: a key the guard never saw let `a` right after it answer). */
  onAccept: (event: KeyboardEvent<HTMLInputElement>) => void;
  onCancel: (event: KeyboardEvent<HTMLInputElement>) => void;
  /** What the line starts with: `/` for R4's search, `:` for K02's command line. */
  lead?: string;
  /** The box's accessible name. */
  label?: string;
  /** K02 fix round 2: bumped to hand the box the keys again while it is already open -- `:` or `/`
   *  typed where the keys went instead (a click took them), which used to wipe the line and leave
   *  them there. The box also takes them when it mounts. A number, so no request is coalesced away. */
  focusRequest?: number;
};

/** Every Tab, whatever it is held with -- and Shift+Tab's other WebKitGTK shape, the one `isShiftTab`
 *  (`../modeKey`) reads. */
function isTab(e: KeyboardEvent<HTMLInputElement>): boolean {
  return e.key === "Tab" || (e.key === "Unidentified" && e.code === "Tab");
}

/** A one-line prompt in the footer, where vim draws its command line: R4's `/` search, and K02's
 *  `:` command line, which runs nothing. */
export function SearchBar({
  query,
  onChange,
  onAccept,
  onCancel,
  lead = "/",
  label = "Search the conversation",
  focusRequest = 0,
}: Props) {
  const ref = useRef<HTMLInputElement>(null);
  useEffect(() => ref.current?.focus(), [focusRequest]);
  return (
    <span className="search-bar" role="search">
      {lead}
      <input
        ref={ref}
        aria-label={label}
        value={query}
        onChange={(e) => onChange(e.target.value)}
        onKeyDown={(e) => {
          if (e.nativeEvent.isComposing || e.keyCode === 229) return;
          if (e.key === "Enter") {
            e.preventDefault();
            e.stopPropagation();
            onAccept(e);
          } else if (e.key === "Escape") {
            e.preventDefault();
            e.stopPropagation();
            onCancel(e);
          } else if (isTab(e)) {
            // K02 fix round 2 (review): vim's command line never leaves on Tab (it completes there).
            // The browser's own focus move put the keys on the band's button just after this box, so
            // the line stayed drawn while BROWSE had them: `:`, Tab, `l`, Enter pressed Approve.
            // Claimed and nothing else; not stopped, so the panel's typing guard still counts it.
            e.preventDefault();
          }
        }}
      />
    </span>
  );
}
