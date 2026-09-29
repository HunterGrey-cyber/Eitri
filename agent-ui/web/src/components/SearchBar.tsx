import { useEffect, useRef } from "react";
import type { KeyboardEvent } from "react";

type Props = {
  query: string;
  onChange: (q: string) => void;
  /** Pass Enter and Esc keydown events to the caller so the typing guard sees them even though
   *  propagation stops here. Otherwise a following `a` could answer a card immediately. */
  onAccept: (event: KeyboardEvent<HTMLInputElement>) => void;
  onCancel: (event: KeyboardEvent<HTMLInputElement>) => void;
  /** What the line starts with: `/` for search, `:` for the command line. */
  lead?: string;
  /** The box's accessible name. */
  label?: string;
  /** Increment to return focus to an already open box when `:` or `/` is typed elsewhere.
   *  The box also focuses on mount. A number keeps successive requests distinct. */
  focusRequest?: number;
};

/** Every Tab, whatever it is held with -- and Shift+Tab's other WebKitGTK shape, the one `isShiftTab`
 *  (`../modeKey`) reads. */
function isTab(e: KeyboardEvent<HTMLInputElement>): boolean {
  return e.key === "Tab" || (e.key === "Unidentified" && e.code === "Tab");
}

/** A one-line footer prompt for `/` search or a `:` command line that runs nothing. */
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
            // Keep Tab in the command line so browser focus movement cannot expose permission actions to typing.
            // Prevent the default focus move without stopping propagation, so the panel's typing guard still sees the key.
            e.preventDefault();
          }
        }}
      />
    </span>
  );
}
