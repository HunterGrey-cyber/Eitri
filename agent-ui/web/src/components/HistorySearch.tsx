import { useEffect, useRef, useState } from "react";
import { isImeKey } from "../composerKeys";
import { searchHistory } from "../promptHistory";
import { NO_TEXT_ASSIST } from "../textField";

type Props = {
  history: string[];
  onAccept: (text: string) => void;
  onCancel: () => void;
  /** Report Enter, Tab, and Escape before stopping them here. They never bubble to `App`'s
   *  typing guard, so without this callback a following `Ctrl+y` could approve a card. This
   *  matches the `/` and `:` input lines' use of `noteLineKey`. */
  onLineKey?: (event: React.KeyboardEvent<HTMLInputElement | HTMLTextAreaElement>) => void;
};

/** readline's `(reverse-i-search)` line: typing narrows, `Ctrl+r` goes older,
 *  `Enter`/`Tab` put the match in the box without sending, `Esc` leaves the box as it was. */
export function HistorySearch({ history, onAccept, onCancel, onLineKey }: Props) {
  const [query, setQuery] = useState("");
  const [index, setIndex] = useState<number | null>(null);
  const ref = useRef<HTMLInputElement>(null);
  useEffect(() => ref.current?.focus(), []);
  const match = index === null ? null : history[index];
  return (
    <div className="history-search" role="search">
      <span className="history-search-label">(reverse-i-search)`</span>
      <input
        ref={ref}
        {...NO_TEXT_ASSIST}
        aria-label="Search your earlier prompts"
        value={query}
        onChange={(e) => {
          setQuery(e.target.value);
          setIndex(searchHistory(history, e.target.value, null));
        }}
        onKeyDown={(e) => {
          if (isImeKey({ isComposing: e.nativeEvent.isComposing, keyCode: e.keyCode })) return;
          if (e.key === "r" && e.ctrlKey) {
            e.preventDefault();
            const older = searchHistory(history, query, index);
            if (older !== null) setIndex(older);
          } else if (e.key === "Enter" || e.key === "Tab") {
            e.preventDefault();
            e.stopPropagation();
            onLineKey?.(e);
            if (match !== null) onAccept(match);
            else onCancel();
          } else if (e.key === "Escape") {
            e.preventDefault();
            e.stopPropagation();
            onLineKey?.(e);
            onCancel();
          }
        }}
      />
      <span className="history-search-label">': </span>
      <span className="history-search-match">{match ?? ""}</span>
    </div>
  );
}
