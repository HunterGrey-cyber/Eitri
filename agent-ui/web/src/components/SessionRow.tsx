import type { ResumableSession } from "../types";

/** Shows the first 8 characters of a provider session id. */
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
 * Read the absence of a "started" line as "these two stamps are equal, or `createdAt` is empty", not
 * as "this session was never resumed". They are the same thing only while `created_at` really does
 * survive every resume, which is `conversation::persist_record`'s job for a session recorded in either storage
 * layout -- see `ResumableSession::created_at` in `agent/src/persistence.rs`. */
export function describeWhen(session: ResumableSession): string {
  const lastOpened = `last opened ${formatWhen(session.updatedAt)}`;
  if (session.createdAt === "" || session.createdAt === session.updatedAt) return lastOpened;
  return `${lastOpened} · started ${formatWhen(session.createdAt)}`;
}

/** Epoch milliseconds as a string. The non-numeric branch is defensive: persistence tolerates
 *  hand-edited or corrupted records, and the literal stamp is more useful than "Invalid Date".
 *  The writer (`conversation::epoch_millis`) emits numeric stamps; no known real case or test
 *  exercises the fallback. */
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
