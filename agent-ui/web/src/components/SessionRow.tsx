import type { ResumableSession } from "../types";

/** Shows the first 8 characters of a provider session id -- moved out of `ModeSelector.tsx`
 *  unchanged (Task 9, EmptyTab replaces the start screen). */
export function shortId(id: string): string {
  return id.slice(0, 8);
}

/** The two timestamps a record carries. A titled row shows this after its id; an untitled row
 *  shows it as its only detail.
 *
 * "last opened", not "last used" or "last active": `updatedAt` marks when the session was last
 * STARTED OR RESUMED. Nothing rewrites a record during a conversation, so an hour of work and a
 * window opened and abandoned produce the same stamp.
 *
 * `createdAt` is shown only when it differs from `updatedAt`, which is exactly when it says
 * something new: a session started once and never reopened carries the same value twice, and
 * printing it again would read as two facts where there is one.
 *
 * Read the absence of a "started" line as "these two stamps are equal", not as "this session was
 * never resumed". They are the same thing only while `created_at` really does survive every resume,
 * which is `conversation::persist_record`'s job and was genuinely broken for the one case that
 * crossed the 2026-09-15 storage layouts -- see `ResumableSession::created_at` in
 * `agent/src/persistence.rs`. */
export function describeWhen(session: ResumableSession): string {
  const lastOpened = `last opened ${formatWhen(session.updatedAt)}`;
  if (session.createdAt === "" || session.createdAt === session.updatedAt) return lastOpened;
  return `${lastOpened} · started ${formatWhen(session.createdAt)}`;
}

/** Epoch milliseconds as a string (no date library is a dependency of the Rust side that writes
 * it).
 *
 * The non-numeric branch is defensive only, and has no known real case: `conversation::epoch_millis`
 * is the only thing that has ever written one of these stamps, in either storage layout, since the
 * first record was persisted. It is kept because `agent::persistence` deliberately tolerates a
 * hand-edited or corrupted record rather than discarding it, and rendering such a stamp verbatim is
 * better than rendering "Invalid Date". Nothing tests it, because nothing can produce it.
 *
 * This comment used to justify the branch with "a record written under the pre-2026-09-15 layout,
 * whose stamp is an ISO-8601 string". That was never true -- see `updated_at_rank` in
 * `agent/src/persistence.rs`, which carried the same mistake. */
export function formatWhen(stamp: string): string {
  const millis = Number(stamp);
  if (!Number.isFinite(millis) || millis <= 0) return stamp;
  return new Date(millis).toLocaleString();
}

/** A resumable session's text: the name or the title leads, then provider, short id and when.
 *  Without either, what it always was: provider, short id, and when (no label is invented). */
export function SessionRowText({ session }: { session: ResumableSession }) {
  const lead = session.name ?? session.title ?? null;
  return lead ? (
    <>
      <strong className="session-title">{lead}</strong>
      <span className="detail">
        {session.provider} {shortId(session.providerSessionId)} · {describeWhen(session)}
      </span>
    </>
  ) : (
    <>
      <strong>
        {session.provider} {shortId(session.providerSessionId)}
      </strong>
      <span className="detail">{describeWhen(session)}</span>
    </>
  );
}
