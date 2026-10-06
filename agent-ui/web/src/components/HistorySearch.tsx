import { useEffect, useRef, useState } from "react";
import { isImeKey } from "../composerKeys";
import { searchHistory } from "../promptHistory";
import { NO_TEXT_ASSIST } from "../textField";

type Props = {
  history: string[];
  onAccept: (text: string) => void;
  onCancel: () => void;
  /** Owner decision #39, fix round 1 (Codex): the keys this line claims and stops (Enter, Tab,
   *  Escape) never bubble to `App`'s own `onKeyDown`, so its typing guard never saw them and a
   *  `Ctrl+y` right after one approved a card. Each is handed over here first, as the `/` and `:`
   *  lines hand theirs over (`App.tsx`'s `noteLineKey`, K02). */
  onLineKey?: (event: React.KeyboardEvent<HTMLInputElement>) => void;
};

/** readline's `(reverse-i-search)` line (C5, ruling 12): typing narrows, `Ctrl+r` goes older,
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
