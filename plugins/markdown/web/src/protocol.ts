/** The page's side of the plugin protocol: the host's hello, the editor events, and the four editor verbs. Every
 *  reply and event is checked before the page reads it, since the page renders whatever the host hands over. */

export type SettingValue = boolean | string | number;

export interface Hello {
  host: "eitri";
  version: string;
  protocol: 1;
  capabilities: string[];
  settings: Record<string, SettingValue>;
  timeoutlen: number;
  prefix: string;
  /** The keyboard kit's settings from `init.lua`, handed to the kit as they came: it reads each field itself and keeps
   *  its default for a missing or bad one. */
  keys_settings?: unknown;
  module_key: string | null;
  theme: { vars: Record<string, string> };
}

export interface PreviewSettings {
  auto: boolean;
  remote_images: "https" | "off";
  sync: "both" | "editor" | "off";
}

/** The range `font_size` may take: the manifest's own, checked again because the page renders what it is handed. */
export const FONT_SIZE_RANGE = { min: 9, max: 32 } as const;

export interface DocPrefixes {
  root: string;
  base: string;
}

export interface EditorView {
  buf: number;
  name: string;
  filetype: string;
  tick: number;
  current: boolean;
  line_count: number;
  top: number;
  top_fraction: number;
  bottom: number;
  cursor: { line: number; col: number };
  cause: "user" | "reveal";
  token?: number;
  doc: DocPrefixes | null;
  /** The window's effective 'scrolloff', in screen lines; an older host sends none. */
  scrolloff?: number;
  /** nvim's screen height in rows (its whole grid); an older host sends none. */
  rows?: number;
  /** The screen row (one-based, from the top of nvim's grid) on which the cursor's line starts; an older host sends
   *  none. */
  cursor_row?: number;
  /** The share of the editor's height above its grid's first row (the part of a cell the grid does not fill, which
   *  the editor leaves at the top); only from a host that draws nvim's grid itself. */
  grid_top?: number;
}

export interface TooLarge {
  bytes: number;
  lines: number;
  max_bytes: number;
  max_lines: number;
}

export interface TextBody {
  buf: number;
  tick: number;
  name: string;
  text: string;
}

export interface TextTooLarge {
  buf: number;
  tick: number;
  name: string;
  too_large: TooLarge;
}

export type EditorText = TextBody | TextTooLarge;

export interface EditorClosed {
  buf: number;
  reason: "wiped" | "editor";
}

export interface Bridge {
  call(verb: string, args: unknown): Promise<unknown>;
  on(event: string, fn: (data: unknown) => void): () => void;
}

type Rec = Record<string, unknown>;

function isRecord(v: unknown): v is Rec {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

function isCount(v: unknown): v is number {
  return typeof v === "number" && Number.isInteger(v) && v >= 0;
}

function isStringArray(v: unknown): v is string[] {
  return Array.isArray(v) && v.every((x) => typeof x === "string");
}

function isStringRecord(v: unknown): v is Record<string, string> {
  return isRecord(v) && Object.values(v).every((x) => typeof x === "string");
}

/** The host's bridge, which the host injects before the page's scripts run. */
export function bridge(): Bridge {
  const b = (window as unknown as { eitriPlugin?: Bridge }).eitriPlugin;
  if (!b) throw new Error("not connected to Eitri");
  return b;
}

export function isHello(v: unknown): v is Hello {
  if (!isRecord(v)) return false;
  if (v.host !== "eitri" || v.protocol !== 1 || typeof v.version !== "string") return false;
  if (!isStringArray(v.capabilities)) return false;
  if (typeof v.timeoutlen !== "number" || !Number.isFinite(v.timeoutlen)) return false;
  if (typeof v.prefix !== "string") return false;
  if (v.module_key !== null && typeof v.module_key !== "string") return false;
  if (!isRecord(v.theme) || !isStringRecord(v.theme.vars)) return false;
  if (v.settings !== undefined) {
    if (!isRecord(v.settings)) return false;
    if (!Object.values(v.settings).every(isSettingValue)) return false;
  }
  return true;
}

function isSettingValue(x: unknown): x is SettingValue {
  return typeof x === "boolean" || typeof x === "string" || (typeof x === "number" && Number.isFinite(x));
}

/** A setting value of a type no setting kind has is dropped before the reply is checked, so it reads as absent and
 *  takes its default, rather than costing the page the whole hello. */
function withoutForeignSettings(reply: unknown): unknown {
  if (!isRecord(reply) || !isRecord(reply.settings)) return reply;
  const kept = Object.entries(reply.settings).filter(([, x]) => isSettingValue(x));
  return { ...reply, settings: Object.fromEntries(kept) };
}

export async function hello(b: Bridge): Promise<Hello> {
  const reply = withoutForeignSettings(await b.call("host.hello", {}));
  if (!isHello(reply)) throw new Error("the host's hello reply is malformed");
  return { ...reply, settings: reply.settings ?? {} };
}

/** Each setting independently: a missing or wrong value is that setting's default, so one bad value never costs the
 *  others. */
export function readSettings(h: Hello): PreviewSettings {
  const s: Record<string, unknown> = h.settings ?? {};
  const auto = typeof s.auto === "boolean" ? s.auto : false;
  const remote_images = s.remote_images === "https" || s.remote_images === "off" ? s.remote_images : "https";
  const sync = s.sync === "both" || s.sync === "editor" || s.sync === "off" ? s.sync : "both";
  return { auto, remote_images, sync };
}

/** The `font_size` setting from `init.lua` in CSS px, or `null` when it is unset or not a whole number in range: the
 *  text size is then the host's to give (the editor's own) or, with none given, the page's own. */
export function readFontSize(h: Hello): number | null {
  const size = (h.settings ?? {}).font_size;
  const whole = typeof size === "number" && Number.isInteger(size);
  return whole && size >= FONT_SIZE_RANGE.min && size <= FONT_SIZE_RANGE.max ? size : null;
}

function isDoc(v: unknown): v is DocPrefixes {
  return isRecord(v) && typeof v.root === "string" && typeof v.base === "string";
}

export function isEditorView(v: unknown): v is EditorView {
  if (!isRecord(v)) return false;
  for (const key of ["buf", "tick", "line_count", "top", "bottom"]) if (!isCount(v[key])) return false;
  if (!isRecord(v.cursor) || !isCount(v.cursor.line) || !isCount(v.cursor.col)) return false;
  if (typeof v.name !== "string" || typeof v.filetype !== "string" || typeof v.current !== "boolean") return false;
  const f = v.top_fraction;
  if (typeof f !== "number" || !Number.isFinite(f) || f < 0 || f >= 1) return false;
  if (v.cause !== "user" && v.cause !== "reveal") return false;
  if (v.token !== undefined && !isCount(v.token)) return false;
  if (v.scrolloff !== undefined && !isCount(v.scrolloff)) return false;
  if (v.rows !== undefined && !isCount(v.rows)) return false;
  if (v.cursor_row !== undefined && !isCount(v.cursor_row)) return false;
  const g = v.grid_top;
  if (g !== undefined && (typeof g !== "number" || !Number.isFinite(g) || g < 0 || g >= 1)) return false;
  return v.doc === null || isDoc(v.doc);
}

function isTooLarge(v: unknown): v is TooLarge {
  return isRecord(v) && isCount(v.bytes) && isCount(v.lines) && isCount(v.max_bytes) && isCount(v.max_lines);
}

export function isEditorText(v: unknown): v is EditorText {
  if (!isRecord(v) || !isCount(v.buf) || !isCount(v.tick) || typeof v.name !== "string") return false;
  const hasText = v.text !== undefined;
  const hasTooLarge = v.too_large !== undefined;
  if (hasText === hasTooLarge) return false;
  return hasText ? typeof v.text === "string" : isTooLarge(v.too_large);
}

export function isEditorClosed(v: unknown): v is EditorClosed {
  return isRecord(v) && isCount(v.buf) && (v.reason === "wiped" || v.reason === "editor");
}

async function verb<T>(b: Bridge, name: string, args: unknown, check: (v: unknown) => v is T): Promise<T> {
  const reply = await b.call(name, args);
  if (!check(reply)) throw new Error(`${name}: the host's reply is malformed`);
  return reply;
}

function isApplied(v: unknown): v is { applied: boolean } {
  return isRecord(v) && typeof v.applied === "boolean";
}

export function follow(b: Bridge): Promise<{ filetypes: string[] }> {
  return verb(b, "editor.follow", {}, (v): v is { filetypes: string[] } => isRecord(v) && isStringArray(v.filetypes));
}

// The host refuses an argument it does not know, so each verb sends exactly the object it was given. A reveal's
// `cursor` (the line nvim's cursor goes to, kept in view) is optional: an older host refuses it.
export function reveal(
  b: Bridge,
  a: { buf: number; line: number; token: number; cursor?: number; row?: number },
): Promise<{ applied: boolean }> {
  return verb(b, "editor.reveal", a, isApplied);
}

/** `select` (an older host refuses it): nvim selects from (`line`, `col`) to its place in its own visual mode, its
 *  cursor ending there. */
export function jump(
  b: Bridge,
  a: { buf: number; line: number; col: number; focus: boolean; select?: { line: number; col: number; mode: "char" | "line" } },
): Promise<{ applied: boolean }> {
  return verb(b, "editor.jump", a, isApplied);
}

export function openLink(
  b: Bridge,
  a: { buf: number; target: string },
): Promise<{ how: "edit" | "split" | "current"; line: number; path?: string }> {
  return verb(
    b,
    "editor.open_link",
    a,
    (v): v is { how: "edit" | "split" | "current"; line: number; path?: string } =>
      isRecord(v) && (v.how === "edit" || v.how === "split" || v.how === "current") && isCount(v.line) &&
      (v.path === undefined || typeof v.path === "string"),
  );
}
