import type { Row } from "./agents";

/** How long a row that left keeps its place for its key: a tab moving to another window, or a reader starting over,
 *  comes back where it was. After that the place is empty and the next new row of the project may take it. */
export const HOLD_MS = 5_000;

export type Slot = {
  /** The key that owns the place: a row on screen, or one held since `left`. `null` is an empty place. */
  key: string | null;
  left: number | null;
};
export type Group = { key: string; name: string; slots: Slot[] };

/** Places for life by key. A row keeps its place whatever its state does; a new row takes an empty place of its
 *  project or goes at the end; a new project goes at the bottom. Nothing is ever sorted again, and only empty places
 *  at the end of a project (and a project left with none) go away. */
export class Slots {
  private list: Group[] = [];

  update(rows: Row[], now: number): void {
    const present = new Map(rows.map(row => [row.key, row]));
    const placed = new Set<string>();
    for (const group of this.list) {
      for (const slot of group.slots) {
        if (slot.key === null) continue;
        const row = present.get(slot.key);
        if (row && row.project.key === group.key && !placed.has(slot.key)) {
          slot.left = null;
          placed.add(slot.key);
        } else if (row) {
          // The tab now belongs to another project's window: its place there is the right one.
          slot.key = null;
          slot.left = null;
        } else if (slot.left === null) {
          slot.left = now;
        } else if (now - slot.left >= HOLD_MS) {
          slot.key = null;
          slot.left = null;
        }
      }
    }
    for (const row of rows) {
      if (placed.has(row.key)) continue;
      let group = this.list.find(g => g.key === row.project.key);
      if (!group) {
        group = { key: row.project.key, name: row.project.name, slots: [] };
        this.list.push(group);
      }
      const free = group.slots.find(slot => slot.key === null);
      if (free) free.key = row.key;
      else group.slots.push({ key: row.key, left: null });
      placed.add(row.key);
    }
    for (const group of this.list) {
      const named = rows.find(row => row.project.key === group.key);
      if (named) group.name = named.project.name;
      while (group.slots.length && group.slots[group.slots.length - 1].key === null) group.slots.pop();
    }
    this.list = this.list.filter(group => group.slots.length > 0);
  }

  groups(): readonly Group[] {
    return this.list;
  }

  /** Whether a held place is still waiting for its row. */
  holding(): boolean {
    return this.list.some(group => group.slots.some(slot => slot.key !== null && slot.left !== null));
  }
}
