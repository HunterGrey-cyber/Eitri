// Types for the parts of the agents kit, host bridge and keys kit that this page reads.
// `window.eitriAgents` supplies the agents API used here.

export type Row = {
  id: string;
  key: string;
  agent: string;
  project: { key: string; name: string };
  window: { instance: string; name: string; focused: boolean };
  number: number;
  label: string;
  state: string;
  class: string;
  since: number;
  needs: { cards: number; questions: number; trust: boolean };
  waiting_since: number | null;
  unread: boolean;
  tool: string | null;
  background: { calls: number; shells: number; monitors: number; agents: number; other: number };
  queued: number;
  ended_how: string | null;
  moving: boolean;
};

export type Round = { current: string | null; visited: string[] };

export type Store = {
  rows(): Row[];
  byKey(key: string): Row | undefined;
  selection(): string | null;
  round(): Round | null;
  on(fn: () => void): () => void;
};

export type Kit = {
  version: number;
  store: Store;
  glyph(row: Row): string;
  word(row: Row): string;
  bindKeys(opts: { order: () => string[]; onFilter?: (q: string) => void }): Promise<() => void>;
  start(): Promise<void>;
};

export type Bridge = {
  call(verb: string, args: unknown): Promise<unknown>;
  on(event: string, fn: (data: unknown) => void): () => void;
};

export type KeyRow = { action: string; keys: string[]; context: string; desc: string };
export type KeysKit = {
  install(opts: { rows: KeyRow[]; run(action: string, ev: KeyboardEvent): boolean }): Promise<() => void>;
  message(text: string): void;
};

export type Host = Window & { eitriAgents?: Kit; eitriPlugin?: Bridge; eitriKeys?: KeysKit };
