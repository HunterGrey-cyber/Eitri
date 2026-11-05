import type { Row } from "../../../../plugin-kit/agents/dist/store";

export const SORTS = ["state", "age", "project", "label"] as const;
export type Sort = typeof SORTS[number];
// What needs the person, then what went wrong, then what works, then what is quiet.
const states = ["card", "question", "trust", "failed", "ended", "running", "background", "starting", "done", "idle", "new"];
const classes = ["needs", "working", "quiet", "ended"];
const text = (a: string, b: string) => a < b ? -1 : a > b ? 1 : 0;
function stateOrder(row: Row): number {
  const known = states.indexOf(row.state);
  if (known >= 0) return known;
  const group = classes.indexOf(row.class);
  return [2.5, 7.5, 10.5, 11.5][group] ?? 11;
}
function projectOrder(a: Row, b: Row): number {
  return text(a.project.name, b.project.name) || text(a.project.key, b.project.key);
}
export function sortRows(rows: Row[], sort: Sort, reverse = false): Row[] {
  return [...rows].sort((a, b) => {
    const primary = sort === "state" ? stateOrder(a) - stateOrder(b)
      : sort === "age" ? a.since - b.since
      : sort === "project" ? projectOrder(a, b) : text(a.label, b.label);
    const result = primary || projectOrder(a, b) || text(a.window.name, b.window.name) ||
      text(a.window.instance, b.window.instance) || a.number - b.number || text(a.key, b.key);
    return reverse ? -result : result;
  });
}
