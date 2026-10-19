import type { Row } from "./rows";

export type PluginBridge = {
  call(verb: string, args: unknown): Promise<unknown>;
  on(event: string, fn: (data: unknown) => void): () => void;
};

declare global {
  interface Window { eitriPlugin?: PluginBridge }
}

export function pluginBridge(): PluginBridge {
  if (!window.eitriPlugin) throw new Error("未连接 Eitri");
  return window.eitriPlugin;
}

function record(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

export function effectiveRows(reply: unknown): Row[] {
  const rows: unknown = record(reply) ? reply.rows : reply;
  if (!Array.isArray(rows)) throw new Error("按键表格式无效");
  return rows.map((row: unknown) => {
    if (!record(row) || typeof row.action !== "string" || typeof row.context !== "string" ||
      typeof row.desc !== "string" || !Array.isArray(row.keys)) throw new Error("按键表格式无效");
    const keys = row.keys.map((entry: unknown) => {
      if (typeof entry === "string") return entry;
      if (Array.isArray(entry) && entry.length === 2 && typeof entry[0] === "string" && typeof entry[1] === "string") {
        return entry[0];
      }
      throw new Error("按键表格式无效");
    });
    return { action: row.action, keys, context: row.context, desc: row.desc };
  });
}

export function hasKeys(data: unknown): boolean | undefined {
  return record(data) && typeof data.has_keys === "boolean" ? data.has_keys : undefined;
}

export function timeoutOf(data: unknown): number | undefined {
  return record(data) && typeof data.timeoutlen === "number" && Number.isFinite(data.timeoutlen) && data.timeoutlen >= 0
    ? data.timeoutlen : undefined;
}
