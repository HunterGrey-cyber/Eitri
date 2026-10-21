export const STYLE_ID = "eitri-keys-style";
export const STYLE_TEXT: string =
  '#eitri-keys-hud{position:fixed;bottom:0;left:0;right:0;z-index:2147483647;display:flex;align-items:baseline;padding:0 8px 0 0;white-space:nowrap;pointer-events:none;background:var(--nv-chrome, #20252b);color:var(--nv-chrome-fg, #f4f4f4);font:13px/20px var(--nv-font-mono, monospace)}' +
  '#eitri-keys-hud[hidden]{display:none}' +
  '.eitri-keys-mode{flex:0 0 auto;padding:0 1ch;font-weight:bold;background:var(--nv-status-normal-bg, #20252b);color:var(--nv-status-normal-fg, #f4f4f4)}' +
  '.eitri-keys-mode[data-mode="insert"]{background:var(--nv-status-insert-bg, #20252b);color:var(--nv-status-insert-fg, #f4f4f4)}' +
  '.eitri-keys-pending{flex:0 0 auto;padding:0 0 0 1ch;color:var(--nv-chrome-muted, #c8c8c8)}' +
  '.eitri-keys-pending:empty{display:none}' +
  '.eitri-keys-whichkey{flex:1 1 auto;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;padding:0 1ch;color:var(--nv-chrome-muted, #b0b0b0)}' +
  '.eitri-keys-whichkey:empty{display:none}' +
  '.eitri-keys-message{flex:1 1 auto;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;padding:0 1ch}' +
  '.eitri-keys-status{flex:0 1 auto;max-width:60%;margin-left:auto;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;color:var(--nv-chrome-fg, #f4f4f4)}' +
  '.eitri-keys-current{outline:2px solid var(--nv-focus-ring, #cfa85d);outline-offset:3px}' +
  '.eitri-keys-current.eitri-keys-dimmed{outline-color:var(--nv-border, #8888)}' +
  '.eitri-hint{position:fixed;padding:1px 3px;border-radius:2px;text-transform:none;background:var(--nv-hint-bg, #ffe082);color:var(--nv-hint-fg, #171717);border:1px solid var(--nv-hint-fg, #171717);font:bold 13px/16px var(--nv-font-mono, monospace)}' +
  '.eitri-hint.eitri-hint-danger{border:2px solid var(--nv-error, #9c2525)}' +
  '#eitri-keys-sheet{position:fixed;inset:8%;z-index:2147483646;overflow:auto;padding:24px;background:var(--nv-surface, #20252b);color:var(--nv-surface-fg, #f4f4f4);border:1px solid var(--nv-border, #888);font:14px var(--nv-font-prose, sans-serif)}' +
  '.eitri-keys-sheet-context{margin:1em 0 .25em;font-size:1em;color:var(--nv-surface-muted, #b0b0b0)}' +
  '.eitri-keys-sheet-row kbd{font-family:var(--nv-font-mono, monospace)}' +
  '.eitri-keys-sheet-muted{color:var(--nv-surface-muted, #b0b0b0)}' +
  '#eitri-keys-find{position:fixed;left:0;right:0;bottom:20px;z-index:2147483647;display:flex;gap:4px;padding:0 8px;background:var(--nv-chrome, #20252b);color:var(--nv-chrome-fg, #f4f4f4);font:13px/20px var(--nv-font-mono, monospace)}' +
  '#eitri-keys-find[hidden]{display:none}' +
  '#eitri-keys-find input{flex:1;min-width:0;padding:0;border-width:0;outline-style:none;background-color:var(--nv-chrome, #20252b);color:var(--nv-chrome-fg, #f4f4f4);font:13px/20px var(--nv-font-mono, monospace)}' +
  '::highlight(eitri-find){background-color:var(--nv-search, #ffd54a)}' +
  '::highlight(eitri-find-current){background-color:var(--nv-chrome-accent, #ff9800);color:var(--nv-chrome, #000)}';

export function ensureStyle(doc: Document = document): void {
  if (doc.getElementById(STYLE_ID)) return;
  const style = doc.createElement("style");
  style.id = STYLE_ID;
  style.textContent = STYLE_TEXT;
  (doc.head ?? doc.documentElement).append(style);
}
