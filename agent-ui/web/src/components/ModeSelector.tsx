import { useState } from "react";
import type { Hello, PermissionModeChoice, ResumableSession } from "../types";
import { Row } from "./Row";

type Props = {
  hello: Hello | null;
  connecting: boolean;
  onStart: (mode: PermissionModeChoice, resume?: string) => void;
};

/** One button per permission mode the backend GENUINELY offers, driven by `hello.permissionModes`
 * and never hardcoded. That list is the client's own implemented set intersected, at session
 * creation, with what the provider advertises -- a backend whose provider does not offer a policy
 * fails loudly there rather than being quietly given a different one. It is deliberately not keyed
 * on which backend this is: both have a real, separately verified interactive gate. */
/* The two modes differ in what the agent MAY do, not only in whether it asks -- and since
   2026-09-18 that difference is real rather than a manner of speaking. The gate is the `PreToolUse`
   hook, and Bypass installs none, so editing tools are denied there: an edit under Bypass would be
   a file rewritten under a live buffer with no diff, no decision and nothing in the transcript that
   had to be read. `agent::disallowed_tools_for` is where that lives.

   The Bypass line used to read "Every tool call proceeds immediately", which stopped being true the
   moment the lists diverged. A start screen that says a mode does something it does not is worse
   than one that says less: this is the screen whose own text already promises the choice cannot be
   changed afterwards. */
const MODE_LABELS: Record<PermissionModeChoice, { title: string; detail: string }> = {
  auto: {
    title: "Auto",
    detail: "Asks before each tool call, and can edit files once you approve.",
  },
  bypass: {
    title: "Bypass",
    detail: "Never asks — and cannot edit files, because nothing would be there to ask.",
  },
};

function shortId(id: string): string {
  return id.slice(0, 8);
}

export function ModeSelector({ hello, connecting, onStart }: Props) {
  /* WHICH conversation to start, as a provider session id, or null for a fresh one. Two axes, not
     one: which conversation and under what permission policy are independent choices, and a resume
     mints a new run whose policy is genuinely open.

     This used to be one button that carried a permission mode chosen for the user
     (`permissionModes.includes("auto") ? "auto" : "bypass"`). That was invisible but harmless while
     the sidecar offered a single mode -- there was nothing to choose. Once it offered two, the same
     line started silently picking one, on a screen whose own text promises "choose a permission mode
     (it cannot be changed afterwards)". A screen cannot say that and then decide for you. */
  const [selectedSessionId, setSelectedSessionId] = useState<string | null>(null);

  if (connecting) {
    return (
      <div className="mode-selector">
        <p className="connecting">Starting the agent backend…</p>
        <p className="detail">
          The first start on a fresh Verdandi checkout also builds the sidecar, which can take a
          while. The window stays responsive.
        </p>
      </div>
    );
  }

  if (hello === null) {
    return <div className="mode-selector"><p className="connecting">Connecting to the shell…</p></div>;
  }

  const onlyBypass = hello.permissionModes.length === 1 && hello.permissionModes[0] === "bypass";
  const sessions = hello.resumableSessions;
  /* Resolved against the list rather than trusted on its own, so a selection cannot outlive the row
     it points at -- a `hello` that arrived without the session that armed it leaves this null and
     the next start is a fresh one, which is the safe direction. */
  const selected = sessions.find((s) => s.providerSessionId === selectedSessionId) ?? null;

  return (
    <div className="mode-selector">
      <p>
        Start a conversation in <code>{hello.projectDir}</code>
        {hello.permissionModes.length > 1
          ? " — choose a permission mode (it cannot be changed afterwards):"
          : ":"}
      </p>

      {sessions.length > 0 && (
        /* Rendered on `resumableSessions` alone. That array is already the full condition -- server
           advertised resume, this client implements it, and this workspace has these stored provider
           sessions -- so there is no second check to forget here.

           A selector, not a row of start buttons: picking one changes what the mode buttons below
           will do rather than starting anything, which is what keeps the permission choice the
           user's. */
        <>
          {/* Outside the radiogroup, not inside it: a `radiogroup` owns its radios, and a paragraph
              among them is a child a screen reader has to walk past to reach the options. Said
              plainly at all because the rows genuinely look opaque, and a user deserves to know why
              rather than assuming the labels failed to load -- nothing here stands in for a title
              that exists elsewhere. No title is stored anywhere. */}
          <p className="detail session-choice-note" data-testid="no-title-note">
            Previous conversations here, newest first. A session is identified by its provider and
            session id and when it was last opened; no title or summary of a conversation is stored.
          </p>
          {/* Both choice rows go through `Row` (`./Row`), the one component that owns the two-cell
              sign grid. It also switches its cells to `<span>` for the button shape, which is what
              these rows need: a `<button>`'s content model is phrasing content. */}
          <div className="session-choice" role="radiogroup" aria-label="Which conversation">
            <Row
              as="button"
              kind="choice"
              sign="›"
              role="radio"
              navStop="choice"
              aria-checked={selected === null}
              className={selected === null ? "selected" : undefined}
              onClick={() => setSelectedSessionId(null)}
            >
              <strong>New session</strong>
            </Row>
            {/* Array order, unsorted: Rust ranked these by a timestamp it can compare as a number,
                and re-deriving that here from strings would be a second ranking free to disagree. */}
            {sessions.map((session) => {
              const checked = selected?.providerSessionId === session.providerSessionId;
              return (
                <Row
                  key={session.providerSessionId}
                  as="button"
                  kind="choice"
                  sign="↺"
                  role="radio"
                  navStop="choice"
                  aria-checked={checked}
                  className={checked ? "resume selected" : "resume"}
                  onClick={() => setSelectedSessionId(session.providerSessionId)}
                >
                  {/* The provider name comes from the record, not from a literal. It was `Claude`
                      hardcoded, which is correct today only because `PROVIDER_NAME` is "claude" --
                      on a screen whose whole argument is that a row must not say what the data does
                      not support, the one word naming the provider was the one word not read from
                      it. Rendered as stored, lowercase and all: a display-name table here would be
                      a second place to keep in sync with the provider list. */}
                  <strong>{session.provider} {shortId(session.providerSessionId)}</strong>
                  <span className="detail">{describeWhen(session)}</span>
                </Row>
              );
            })}
          </div>
        </>
      )}

      {hello.permissionModes.map((mode) => (
        <button
          key={mode}
          data-nav-stop="mode"
          onClick={() => onStart(mode, selected === null ? undefined : selected.providerSessionId)}
        >
          <strong>{MODE_LABELS[mode].title}</strong>
          <span className="detail">
            {MODE_LABELS[mode].detail}
            {selected === null ? "" : ` Continues ${selected.provider} ${shortId(selected.providerSessionId)}.`}
          </span>
        </button>
      ))}

      {onlyBypass && (
        // Said plainly rather than buried: this backend offers exactly one policy. Calling that a
        // "choice" would imply an alternative exists.
        <p className="warning">
          This backend ({hello.backend}) offers <strong>Bypass only</strong>. Tool calls run without
          asking, and file-editing tools are unavailable — there would be no card to answer.
        </p>
      )}
    </div>
  );
}

/** The two timestamps a record carries, and nothing else -- there is no title to fall back to.
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
function describeWhen(session: ResumableSession): string {
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
function formatWhen(stamp: string): string {
  const millis = Number(stamp);
  if (!Number.isFinite(millis) || millis <= 0) return stamp;
  return new Date(millis).toLocaleString();
}
