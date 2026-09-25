/** Claude Code's statusLine, in its own row beneath the composer (docs, statusline.md). `Enter` on it
 *  (it is a button) opens the detail popover, as `prefix i` does. */
export function StatusRow({ text, warning, onOpenDetail }: { text: string; warning: string | null; onOpenDetail: () => void }) {
  return (
    <button type="button" className="status-row" data-nav-stop="status-row" onClick={onOpenDetail}>
      {text}
      {warning !== null && (
        <span className="status-warning" title={warning}>
          ⚠
        </span>
      )}
    </button>
  );
}
