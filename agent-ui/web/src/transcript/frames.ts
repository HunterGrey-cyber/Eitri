import { applyCallNotes, applyEvents, applySnapshot, initialState } from "../reducer";
import type { CallNotes } from "../reducer";
import type { AgentDomainEvent, AgentUiSnapshot, AgentUiState } from "../types";

export const TRANSCRIPT_PROTOCOL = 1;
export type HostHello = {
  kind: "host"; protocol: number; deskProtocol: number; app: string; device: string; location: string;
};
export type ThemeTokens = { vars: Record<string, string>; scheme: string };
export type CutEntry = { entry_id: string; bytes: number };
export type TranscriptModel = {
  state: AgentUiState; key: string | null; generation: number | null; firstTurn: number | null;
  throughRevision: number; theme: ThemeTokens | null; prompt: unknown | null; notices: string[]; rootNote: string | null;
  shellRunning: boolean; sendBlock: string | null; cut: CutEntry[]; ended: string | null; gone: boolean;
  update: boolean; stopped: boolean; wantSnapshot: boolean; location: string; device: string; owedPage: boolean;
};

export function initialTranscript(): TranscriptModel {
  return {
    state: initialState(), key: null, generation: null, firstTurn: null, throughRevision: 0,
    theme: null, prompt: null, notices: [], rootNote: null, shellRunning: false, sendBlock: null,
    cut: [], ended: null, gone: false, update: false, stopped: false, wantSnapshot: false, location: "", device: "",
    owedPage: false,
  };
}

type ObjectValue = Record<string, unknown>;
const object = (v: unknown): v is ObjectValue => v !== null && typeof v === "object" && !Array.isArray(v);
const uint = (v: unknown): v is number => typeof v === "number" && Number.isSafeInteger(v) && v >= 0;
const textOrNull = (v: unknown): v is string | null => v === null || typeof v === "string";
const needsUpdate = (m: TranscriptModel): TranscriptModel => m.update ? m : { ...m, update: true };
const stop = (m: TranscriptModel): TranscriptModel => ({ ...m, update: true, stopped: true });
const needsSnapshot = (m: TranscriptModel): TranscriptModel => m.wantSnapshot ? m : { ...m, wantSnapshot: true };

function themeTokens(v: unknown): v is ThemeTokens {
  return object(v) && object(v.vars) && typeof v.scheme === "string" &&
    Object.values(v.vars).every((value) => typeof value === "string");
}

const collections = ["userPrompts", "transcript", "toolCalls", "thinking", "turnStarts", "notices", "turnEndings", "questions"] as const;
type Collection = typeof collections[number];
type Entry = { seq: number };

function snapshotState(v: unknown): v is AgentUiSnapshot {
  return object(v) && collections.every((name) => v[name] === undefined ||
    (Array.isArray(v[name]) && v[name].every((entry) => object(entry) && uint(entry.seq)))) &&
    ["userPrompts", "transcript", "toolCalls", "pendingPermissions"].every((name) => Array.isArray(v[name]));
}

function cuts(v: unknown): v is CutEntry[] {
  return Array.isArray(v) && v.every((entry) => object(entry) && typeof entry.entry_id === "string" && uint(entry.bytes));
}

/** A turn starts at a prompt or a CLI turn start. Turn zero also owns entries before the first prompt. */
export function starts(state: AgentUiState, firstTurn: number): number[] {
  const seqs = [...new Set([...state.userPrompts, ...state.turnStarts].map((entry) => entry.seq))].sort((a, b) => a - b);
  if (firstTurn === 0) {
    if (seqs.length === 0) seqs.push(0);
    else seqs[0] = 0;
  }
  return seqs;
}

function applyPage(m: TranscriptModel, f: ObjectValue): TranscriptModel {
  if (f.key !== m.key || f.generation !== m.generation || m.firstTurn === null || m.wantSnapshot ||
    !uint(f.from_turn) || !uint(f.through_turn) || f.from_turn > f.through_turn ||
    !object(f.frame) || !snapshotState(f.frame.state)) return m;
  const boundaries = starts(m.state, m.firstTurn);
  const lastTurn = m.firstTurn + Math.max(1, boundaries.length) - 1;
  const prepend = f.through_turn === m.firstTurn - 1;
  const overlaps = f.from_turn <= lastTurn && f.through_turn >= m.firstTurn && f.through_turn <= lastTurn;
  if (!prepend && !overlaps) return m;

  const lo = f.from_turn < m.firstTurn ? 0 : boundaries[f.from_turn - m.firstTurn] ?? 0;
  const hi = boundaries[f.through_turn - m.firstTurn + 1] ?? Infinity;
  // A page through the last turn held replaces what later events still add to: it must be read where this view
  // stands, or those events would apply a second time. Dropped, it can be asked for again.
  if (hi === Infinity && f.frame.throughRevision !== m.throughRevision) return { ...m, owedPage: false };
  const incoming = applySnapshot(initialState(), f.frame.state, m.throughRevision);
  const state = { ...m.state };
  const replaced = new Set<string>();
  for (const name of collections) {
    const entries = new Map<number, Entry>();
    for (const entry of m.state[name]) {
      if (entry.seq < lo || entry.seq >= hi) entries.set(entry.seq, entry);
      else replaced.add(`${name}:${entry.seq}`);
    }
    for (const entry of incoming[name]) {
      entries.set(entry.seq, entry);
      replaced.add(`${name}:${entry.seq}`);
    }
    // Each collection keeps its own wire records; only their shared ordering key is used here.
    (state as Record<Collection, Entry[]>)[name] = [...entries.values()].sort((a, b) => a.seq - b.seq);
  }
  return { ...m, state, firstTurn: Math.min(m.firstTurn, f.from_turn),
    cut: m.cut.filter((entry) => !replaced.has(entry.entry_id)), owedPage: false };
}

/**
 * Pure fold of one message delivered by the host. An incompatible protocol freezes this view
 * (`stopped`). A kind or event this build does not know, or a known frame it cannot read, only
 * raises the "update the app" row (`update`): every frame it does understand keeps applying.
 */
export function applyFrame(m: TranscriptModel, frame: unknown): TranscriptModel {
  if (m.stopped) return m;
  if (!object(frame) || typeof frame.kind !== "string") return needsUpdate(m);
  const f = frame;
  if (f.kind === "host") {
    if (f.protocol !== TRANSCRIPT_PROTOCOL || f.deskProtocol !== TRANSCRIPT_PROTOCOL) return stop(m);
    return { ...m, location: typeof f.location === "string" ? f.location : "",
      device: typeof f.device === "string" ? f.device : "" };
  }
  if (f.v !== TRANSCRIPT_PROTOCOL) return uint(f.v) ? stop(m) : needsUpdate(m);
  switch (f.kind) {
    case "greeting": {
      const g = f.greeting;
      if (typeof f.key !== "string" || !object(g) || !themeTokens(g.theme) || !Array.isArray(g.notices) ||
        !g.notices.every((notice) => typeof notice === "string") || !textOrNull(g.root_note) ||
        !textOrNull(g.send_block) || typeof g.shell_running !== "boolean") return needsUpdate(m);
      return { ...m, key: f.key, prompt: g.prompt ?? null, notices: [...g.notices], rootNote: g.root_note,
        shellRunning: g.shell_running, sendBlock: g.send_block, theme: g.theme };
    }
    case "snapshot":
      if (typeof f.key !== "string" || !uint(f.generation) || !uint(f.through) || !uint(f.first_turn) ||
        !cuts(f.cut) || !object(f.frame) || !snapshotState(f.frame.state)) return needsSnapshot(m);
      return { ...m, key: f.key, generation: f.generation, firstTurn: f.first_turn, throughRevision: f.through,
        state: applySnapshot(m.state, f.frame.state, f.through), cut: [...f.cut], wantSnapshot: false,
        owedPage: false, ended: null, gone: false };
    case "events": {
      if (m.wantSnapshot) return m;
      if (m.generation === null || f.key !== m.key || f.generation !== m.generation || f.from !== m.throughRevision ||
        !uint(f.through) || f.through < m.throughRevision || !object(f.frame) || !Array.isArray(f.frame.events) ||
        !f.frame.events.every((event) => object(event) && typeof event.type === "string")) return needsSnapshot(m);
      const envelope = f.frame;
      if (envelope.revisions !== undefined && (!Array.isArray(envelope.revisions) ||
        envelope.revisions.length !== f.frame.events.length || !envelope.revisions.every(uint))) return needsSnapshot(m);
      if ((envelope.events as ObjectValue[]).some((event) => event.type === "history_evicted")) return needsSnapshot(m);
      let update = false;
      try {
        const state = applyEvents(m.state, envelope.events as AgentDomainEvent[], envelope.revisions as number[] | undefined,
          () => { update = true; });
        return { ...m, state: applyCallNotes(state, envelope as CallNotes), throughRevision: f.through,
          update: m.update || update };
      } catch {
        return needsSnapshot(m);
      }
    }
    case "page": return applyPage(m, f);
    case "gone": return f.key === m.key ? { ...m, gone: true, owedPage: false } : m;
    case "ended": return f.key === m.key && typeof f.how === "string" ? { ...m, ended: f.how, owedPage: false } : m;
    case "theme": return themeTokens(f.theme) ? { ...m, theme: f.theme } : needsUpdate(m);
    case "send_block": return f.key === m.key && textOrNull(f.block) ? { ...m, sendBlock: f.block } : m;
    case "refused": return f.key === m.key ? { ...m, owedPage: false } : m;
    default: return needsUpdate(m);
  }
}

/** The caller marks owedPage when it sends this request; computing it does not change the model. */
export function wantMore(m: TranscriptModel, beforeTurn = m.firstTurn ?? 0): {
  kind: "more"; key: string; generation: number; before_turn: number;
} | null {
  if (m.owedPage || m.wantSnapshot || m.stopped || m.gone || m.key === null || m.generation === null ||
    !uint(beforeTurn) || beforeTurn === 0) return null;
  return { kind: "more", key: m.key, generation: m.generation, before_turn: beforeTurn };
}
