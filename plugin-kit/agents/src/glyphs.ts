import type { Row } from "./store";

const states: Record<string, string> = {
  card: "⚑", question: "?", trust: "?", failed: "✕", ended: "✕", running: "●", background: "◌",
  starting: "…", done: "•", idle: "·", new: " ",
};
const classes: Record<string, string> = { needs: "⚑", working: "●", quiet: "·", ended: "✕" };
const owns = (table: object, key: string) => Object.prototype.hasOwnProperty.call(table, key);
export function glyph(row: Row): string {
  return owns(states, row.state) ? states[row.state] : owns(classes, row.class) ? classes[row.class] : "·";
}
export function word(row: Row): string {
  return owns(states, row.state) ? row.state : row.class;
}
