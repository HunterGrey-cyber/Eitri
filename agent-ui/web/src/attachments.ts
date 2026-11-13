import { nextRequestId, postForResult, postToRust } from "./bridge";
import type { AwaitedResult, OutboundMessage } from "./bridge";
import type { DraftImage, TabId } from "./types";

/** The most one `attach_chunk` carries, decoded: the host refuses a larger one and ends the import. */
export const CHUNK_BYTES = 256 * 1024;
/** The largest image the host takes before resizing it (`RAW_MAX_BYTES`): a larger one is refused before it is read. */
export const RAW_MAX_BYTES = 32 * 1024 * 1024;
/** How many characters of an image's name a chip shows before `…`. */
export const NAME_SHOWN_CHARS = 40;

/** Sends one import message and waits for its `command_result` (`postForResult`; a test hands in its own). */
export type PostForResult = (message: OutboundMessage & { request_id: string }) => Promise<AwaitedResult>;

/** Why an import ended without its image: `begin` when the host refused it before the image joined the draft (no chip
 *  shows the reason, so the page says it), `later` when the import had a token and its chip shows the failure. */
export class ImportRefused extends Error {
  constructor(readonly stage: "begin" | "later", message: string) {
    super(message);
  }
}

/** The imports under way, by the host's token for each, and those `cancelImport` stopped. */
const running = new Set<string>();
const cancelled = new Set<string>();

/** Imports `blob` into `tab`'s draft of `generation`: `attach_begin`, then the bytes in order, at most `CHUNK_BYTES`
 *  per `attach_chunk` and the next only once the last was answered, then `attach_end` with their SHA-256. Resolves with
 *  the host's token for the image, whose `attachment_state` says when it is ready. The page never names a path: the
 *  bytes are what it sends, and `name` is display text. */
export async function importBlob(
  tab: TabId,
  generation: number,
  blob: Blob,
  name: string,
  options: { post?: PostForResult; onBegun?: (id: string) => void; onFailed?: (id: string, reason: string) => void } = {},
): Promise<string> {
  const post = options.post ?? postForResult;
  // Refused before a byte is read: a huge clipboard image would otherwise be held whole only to be turned away.
  if (blob.size > RAW_MAX_BYTES) {
    throw new ImportRefused("begin", `image too large: ${Math.ceil(blob.size / 1_048_576)} MB (the limit is 32 MB)`);
  }
  const bytes = new Uint8Array(await readBytes(blob));
  const begun = await post({
    type: "attach_begin", request_id: nextRequestId(), tab, generation, byte_length: bytes.length, name,
  });
  if (!begun.ok) throw new ImportRefused("begin", begun.error);
  const id = begun.attachmentId;
  if (typeof id !== "string") throw new ImportRefused("begin", "the image was not taken");
  running.add(id);
  options.onBegun?.(id);
  try {
    for (let offset = 0; offset < bytes.length; offset += CHUNK_BYTES) {
      if (cancelled.has(id)) return id;
      const chunk = bytes.subarray(offset, Math.min(offset + CHUNK_BYTES, bytes.length));
      const answer = await post({
        type: "attach_chunk", request_id: nextRequestId(), tab, attachment_id: id, offset, data_b64: base64Of(chunk),
      });
      if (!answer.ok) throw new ImportRefused("later", answer.error);
    }
    if (cancelled.has(id)) return id;
    const ended = await post({
      type: "attach_end", request_id: nextRequestId(), tab, attachment_id: id, sha256: await sha256Hex(bytes),
    });
    if (!ended.ok) throw new ImportRefused("later", ended.error);
    return id;
  } catch (error) {
    const reason = error instanceof Error ? error.message : "the image could not be imported";
    if (!cancelled.has(id)) {
      cancelImport(tab, id);
      options.onFailed?.(id, reason);
    }
    throw new ImportRefused("later", reason);
  } finally {
    running.delete(id);
    cancelled.delete(id);
  }
}

/** Stops the import of `id` if it is still sending: no more of its bytes go, and the host is told to drop it. False
 *  when it is not sending (its bytes are all with the host, or it never started here): take it out with
 *  `attach_remove` instead. */
export function cancelImport(tab: TabId, id: string): boolean {
  if (!running.has(id) || cancelled.has(id)) return false;
  cancelled.add(id);
  postToRust({ type: "attach_cancel", request_id: nextRequestId(), tab, attachment_id: id });
  return true;
}

/** Whether a clipboard or a drop holds an image file, read from the item types alone: no file is opened. */
export function holdsImage(items: DataTransferItemList | null | undefined): boolean {
  if (!items) return false;
  for (let i = 0; i < items.length; i += 1) {
    if (items[i].kind === "file" && items[i].type.startsWith("image/")) return true;
  }
  return false;
}

/** The image files of a paste's items, in order. */
export function imageItemFiles(items: DataTransferItemList | null | undefined): File[] {
  const files: File[] = [];
  if (!items) return files;
  for (let i = 0; i < items.length; i += 1) {
    const item = items[i];
    if (item.kind !== "file" || !item.type.startsWith("image/")) continue;
    const file = item.getAsFile();
    if (file !== null) files.push(file);
  }
  return files;
}

/** A chip's name: at most `NAME_SHOWN_CHARS` characters, then `…`. Cut by code point, so no character is halved. */
export function shownName(name: string): string {
  const chars = Array.from(name);
  return chars.length > NAME_SHOWN_CHARS ? `${chars.slice(0, NAME_SHOWN_CHARS).join("")}…` : name;
}

/** `PNG · 1280×720 · 312 KB` for a ready image; `null` while the host has not said. */
export function metaLine(item: DraftImage): string | null {
  if (item.mediaType === undefined || item.width === undefined || item.height === undefined || item.bytes === undefined) {
    return null;
  }
  const kind = item.mediaType.replace(/^image\//, "").toUpperCase();
  return `${kind} · ${item.width}×${item.height} · ${sizeWords(item.bytes)}`;
}

function sizeWords(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

/** Why the composer will not send the draft's images now, or `null` when they may go. */
export function imagesBlocking(items: readonly DraftImage[], pending: number): string | null {
  if (pending > 0 || items.some((item) => item.state === "importing")) return "an image is still being prepared";
  if (items.some((item) => item.state === "failed")) return "an image could not be used: remove it first";
  return null;
}

function readBytes(blob: Blob): Promise<ArrayBuffer> {
  if (typeof blob.arrayBuffer === "function") return blob.arrayBuffer();
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(reader.result as ArrayBuffer);
    reader.onerror = () => reject(reader.error ?? new Error("the image could not be read"));
    reader.readAsArrayBuffer(blob);
  });
}

/** `bytes` as base64, built in slices so a large chunk never becomes one huge argument list. */
export function base64Of(bytes: Uint8Array): string {
  let binary = "";
  for (let i = 0; i < bytes.length; i += 0x8000) {
    binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  }
  return btoa(binary);
}

/** The SHA-256 of `bytes`, lowercase hex, from the secure page's platform digest. */
export async function sha256Hex(bytes: Uint8Array): Promise<string> {
  try {
    const digest = await globalThis.crypto.subtle.digest("SHA-256", bytes as Uint8Array<ArrayBuffer>);
    return Array.from(new Uint8Array(digest), (b) => b.toString(16).padStart(2, "0")).join("");
  } catch {
    throw new Error("this page cannot hash images");
  }
}
