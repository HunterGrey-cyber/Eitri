import type { EditorCardWire } from "./types";

const LINE_KINDS = new Set(["plain", "code", "agent", "warning"]);

/** Whether `card` is a broker card the card view can draw: its own id, a title, and body lines of a known kind with
 *  text. Anything else is dropped where the list enters the page. */
export function isEditorCardWire(card: unknown): card is EditorCardWire {
  if (card === null || typeof card !== "object") return false;
  const c = card as Record<string, unknown>;
  return typeof c.id === "string" && typeof c.seq === "number" && typeof c.tool === "string" &&
    (c.toolUseId === null || c.toolUseId === undefined || typeof c.toolUseId === "string") &&
    typeof c.title === "string" && Array.isArray(c.body) &&
    c.body.every((line) => line !== null && typeof line === "object" &&
      LINE_KINDS.has((line as { kind?: unknown }).kind as string) && typeof (line as { text?: unknown }).text === "string");
}
