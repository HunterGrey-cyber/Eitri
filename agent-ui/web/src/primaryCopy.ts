import { postToRust } from "./bridge";

/** The most text one copy puts in the primary selection: 1 MiB of UTF-8. The shell refuses more
 *  (`PRIMARY_COPY_MAX_BYTES` in `core/src/agent_bridge.rs`; `primaryCopy.test.ts` holds the two equal),
 *  so a longer copy reaches the clipboard alone rather than being sent only to be dropped. */
export const PRIMARY_COPY_MAX_BYTES = 1 << 20;

/** `text`'s length in UTF-8 bytes, the unit the shell counts in. A lone surrogate counts as the
 *  three bytes of the replacement character it becomes. */
export function utf8Length(text: string): number {
  let bytes = 0;
  for (let i = 0; i < text.length; i += 1) {
    const unit = text.charCodeAt(i);
    if (unit < 0x80) bytes += 1;
    else if (unit < 0x800) bytes += 2;
    else if (unit >= 0xd800 && unit <= 0xdbff && (text.charCodeAt(i + 1) & 0xfc00) === 0xdc00) {
      bytes += 4;
      i += 1;
    } else bytes += 3;
  }
  return bytes;
}

/** Puts a copy in the primary selection as well as the clipboard, so a middle-click pastes it as it
 *  does from a terminal or nvim. The page cannot reach the primary selection itself (WebKit sets it
 *  only from the live selection, row signs and fold markers included), so the shell sets it from the
 *  exact text the clipboard got. */
export function copyToPrimary(text: string): void {
  // Cheap first: 3 bytes is the most one UTF-16 unit costs, so a short text never needs counting.
  if (text.length * 3 > PRIMARY_COPY_MAX_BYTES && utf8Length(text) > PRIMARY_COPY_MAX_BYTES) return;
  postToRust({ type: "copy_primary", text });
}
