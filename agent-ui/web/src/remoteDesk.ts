import { nextRequestId, postRemoteAnswer } from "./bridge";
import type { RemoteConnectedDevice, RemoteDeskEnvelope, RemoteGrant } from "./bridge";

export type Pairing = Extract<RemoteDeskEnvelope, { kind: "remote_pairing" }>;
export type Question =
  | (Extract<RemoteDeskEnvelope, { kind: "remote_enroll" }> & { chosen: RemoteGrant })
  | Extract<RemoteDeskEnvelope, { kind: "remote_listen" }>
  | Extract<RemoteDeskEnvelope, { kind: "remote_grant" }>;
export type Flagged = Extract<RemoteDeskEnvelope, { kind: "remote_device_flagged" }>;

/** What the window's remote-control questions look like now. */
export type DeskView = {
  /** A code waiting to be scanned. */
  pairing: Pairing | null;
  /** The questions waiting, oldest first, at most one of each kind (the window asks no more); the first is shown. */
  questions: Question[];
  /** Why the window turned down the shown question's `y`. */
  refusal: string | null;
  flagged: Flagged[];
  /** The phones connected now, as the window last said. Not part of what Escape takes down: the mark it feeds stays
   *  while a device is connected. */
  connected: RemoteConnectedDevice[];
  sandbox: string | null;
  /** Bumped each time the shown question must be on screen afresh before a `y` counts. */
  asking: number;
};

const EMPTY: DeskView = { pairing: null, questions: [], refusal: null, flagged: [], connected: [], sandbox: null, asking: 0 };

/** The window's remote-control questions, kept apart from the component that draws them: the panel draws that
 *  component in whichever layout is up, and a question must outlive a switch between them, since the window asks it
 *  only once. Fed by the bridge (`installRemoteDesk(desk.receive)`); read through `subscribe`/`snapshot`. */
export class RemoteDesk {
  private view: DeskView = EMPTY;
  private listeners = new Set<() => void>();
  /** The request of the `y` the window has not answered yet, and the question it answers. */
  private pending: { requestId: string; nonce: number } | null = null;
  private pairingTimer: ReturnType<typeof setTimeout> | null = null;
  private questionTimers = new Map<number, ReturnType<typeof setTimeout>>();

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  };

  snapshot = (): DeskView => this.view;

  /** Whether a `y` waits for the window's word. */
  answering(): boolean {
    return this.pending !== null;
  }

  private set(next: Partial<DeskView>) {
    const before = this.view.questions[0]?.nonce;
    let view = { ...this.view, ...next };
    if (view.questions[0]?.nonce !== before) {
      // Another question is shown: what was said about the last one, and its `y` in flight, are not about it.
      if (this.pending !== null && this.pending.nonce !== view.questions[0]?.nonce) this.pending = null;
      view = { ...view, refusal: null, asking: view.asking + 1 };
    }
    this.view = view;
    for (const listener of this.listeners) listener();
  }

  private ask(question: Question, expiresInMs: number) {
    const [replaced] = this.view.questions.filter((q) => q.kind === question.kind);
    if (replaced !== undefined) this.forgetTimer(replaced.nonce);
    this.questionTimers.set(
      question.nonce,
      setTimeout(() => this.close(question.nonce), expiresInMs),
    );
    // The window asks a second question of one kind only once the first went unanswered past its time.
    const questions = this.view.questions.map((q) => (q.kind === question.kind ? question : q));
    this.set({ questions: replaced === undefined ? [...questions, question] : questions });
  }

  private forgetTimer(nonce: number) {
    const timer = this.questionTimers.get(nonce);
    if (timer !== undefined) clearTimeout(timer);
    this.questionTimers.delete(nonce);
  }

  /** Takes the question `nonce` down: answered, past its time, or closed by the window. */
  private close(nonce: number) {
    this.forgetTimer(nonce);
    if (!this.view.questions.some((q) => q.nonce === nonce)) return;
    if (this.pending?.nonce === nonce) this.pending = null;
    this.set({ questions: this.view.questions.filter((q) => q.nonce !== nonce) });
  }

  receive = (payload: RemoteDeskEnvelope): void => {
    switch (payload.kind) {
      case "remote_pairing": {
        if (this.pairingTimer !== null) clearTimeout(this.pairingTimer);
        // A code is shown for as long as it can be presented.
        this.pairingTimer = setTimeout(() => {
          this.pairingTimer = null;
          if (this.view.pairing === payload) this.set({ pairing: null });
        }, payload.expiresInMs);
        this.set({ pairing: payload });
        return;
      }
      case "remote_enroll":
        // The code was presented: it is spent, and the question takes its place.
        if (this.pairingTimer !== null) clearTimeout(this.pairingTimer);
        this.pairingTimer = null;
        this.set({ pairing: null });
        this.ask({ ...payload, chosen: payload.grant }, payload.expiresInMs);
        return;
      case "remote_grant":
      case "remote_listen":
        this.ask(payload, payload.expiresInMs);
        return;
      case "remote_question_closed":
        this.close(payload.nonce);
        return;
      case "remote_device_flagged":
        if (!this.view.flagged.some((f) => f.device === payload.device)) {
          this.set({ flagged: [...this.view.flagged, payload] });
        }
        return;
      case "remote_connected":
        this.set({ connected: payload.devices, sandbox: payload.sandbox ?? null });
        return;
      case "command_result": {
        if (this.pending === null || payload.requestId !== this.pending.requestId) return;
        const { nonce } = this.pending;
        this.pending = null;
        if (payload.ok) {
          this.close(nonce);
        } else if (this.view.questions[0]?.nonce === nonce) {
          // Asked again: the guard starts over from when the reason is on screen.
          this.set({ refusal: payload.error, asking: this.view.asking + 1 });
        }
        return;
      }
    }
  };

  /** Answers the shown question: a `yes` waits for the window's word, a no takes it down at once. */
  answer(yes: boolean): void {
    const q = this.view.questions[0];
    if (q === undefined) return;
    const request_id = nextRequestId();
    if (q.kind === "remote_enroll") {
      postRemoteAnswer({ type: "remote_enroll_answer", request_id, nonce: q.nonce, yes, grant: q.chosen });
    } else if (q.kind === "remote_grant") {
      postRemoteAnswer({ type: "remote_enroll_answer", request_id, nonce: q.nonce, yes, grant: q.to });
    } else {
      postRemoteAnswer({ type: "remote_listen_answer", request_id, nonce: q.nonce, yes });
    }
    if (yes) {
      this.pending = { requestId: request_id, nonce: q.nonce };
    } else {
      this.close(q.nonce);
    }
  }

  /** Tab on a phone question: the next grant it offers. */
  nextGrant(): void {
    const [q, ...rest] = this.view.questions;
    if (q?.kind !== "remote_enroll" || q.grants.length < 2) return;
    const chosen = q.grants[(q.grants.indexOf(q.chosen) + 1) % q.grants.length];
    this.set({ questions: [{ ...q, chosen }, ...rest] });
  }

  /** Escape with no question shown: the code and the warnings go. */
  dismiss(): void {
    if (this.pairingTimer !== null) clearTimeout(this.pairingTimer);
    this.pairingTimer = null;
    this.set({ pairing: null, flagged: [] });
  }
}
