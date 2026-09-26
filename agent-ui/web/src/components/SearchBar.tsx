import { useEffect, useRef } from "react";

type Props = { query: string; onChange: (q: string) => void; onAccept: () => void; onCancel: () => void };

/** R4's one-line `/` prompt, in the footer where vim draws its command line. */
export function SearchBar({ query, onChange, onAccept, onCancel }: Props) {
  const ref = useRef<HTMLInputElement>(null);
  useEffect(() => ref.current?.focus(), []);
  return (
    <span className="search-bar" role="search">
      /
      <input
        ref={ref}
        aria-label="Search the conversation"
        value={query}
        onChange={(e) => onChange(e.target.value)}
        onKeyDown={(e) => {
          if (e.nativeEvent.isComposing || e.keyCode === 229) return;
          if (e.key === "Enter") {
            e.preventDefault();
            e.stopPropagation();
            onAccept();
          } else if (e.key === "Escape") {
            e.preventDefault();
            e.stopPropagation();
            onCancel();
          }
        }}
      />
    </span>
  );
}
