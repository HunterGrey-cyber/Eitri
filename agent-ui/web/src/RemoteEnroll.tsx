import { useEffect, useLayoutEffect, useRef, useSyncExternalStore } from "react";
import type { RemoteGrant } from "./bridge";
import { afterPaint } from "./afterPaint";
import { isPlainAnswerKey } from "./keymap";
import type { KeyLike } from "./keymap";
import { bypassYesCounts } from "./modeKey";
import { isModifierKey } from "./typingGuard";
import { revealHidden } from "./revealHidden";
import { HiddenWarning, Revealed } from "./components/Revealed";
import type { RemoteDesk } from "./remoteDesk";

/** What the window says about a link that is not a tunnel before it listens there. */
export const PLAIN_LINK_WARNING = "traffic on this network is plain text; anyone on it who gets a token can use it";

function grantWords(grant: RemoteGrant): string {
  return grant === "watch" ? "only watch" : "watch and steer";
}

function hiddenCount(text: string): number {
  return revealHidden(text).filter((piece) => "escape" in piece).length;
}

/** The QR code's dark modules, with the quiet zone the standard asks for around them. */
function QrCode({ modules }: { modules: string[] }) {
  const quiet = 4;
  const size = modules.length + 2 * quiet;
  return (
    <svg
      className="remote-qr"
      viewBox={`0 0 ${size} ${size}`}
      width={size * 4}
      height={size * 4}
      shapeRendering="crispEdges"
      role="img"
      aria-label="enrollment code"
    >
      <rect width={size} height={size} fill="#fff" />
      {modules.flatMap((row, y) =>
        [...row].map((cell, x) =>
          cell === "1" ? <rect key={`${x}-${y}`} x={x + quiet} y={y + quiet} width={1} height={1} fill="#000" /> : null,
        ),
      )}
    </svg>
  );
}

/** The window's remote-control questions, asked in the base page: the enrollment code to scan, "Add device ...?",
 *  "Let device ... watch and steer?" and "Listen for phones on ...?", and the devices no desk said yes to. They live
 *  in `desk`, not here, so a switch between the panel's layouts (each draws this) loses none; one waits behind
 *  another. Each question is a y/n: a `y` counts only as a plain key (no Ctrl, Alt, Meta, Super or AltGraph, not
 *  composing, not a held key's repeat), once the question has been painted, and only when no other key came just
 *  before it (`bypassYesCounts`), so a key on its way when the question appeared answers nothing; a plain `n`,
 *  Escape and Enter answer no. While `covered` (another of the panel's overlays owns the keys) the question takes no
 *  key, and once uncovered it must be on screen afresh before a `y` counts. A `y` the window turns down (a covered
 *  chat) leaves the question up with the reason, to be answered again. `now` is the clock keys are judged on (tests
 *  give their own). */
export function RemoteEnroll({
  desk,
  covered = false,
  now = () => performance.now(),
}: {
  desk: RemoteDesk;
  covered?: boolean;
  now?: () => number;
}) {
  const { pairing, questions, refusal, flagged, asking } = useSyncExternalStore(desk.subscribe, desk.snapshot);
  const question = questions[0] ?? null;
  const openedAt = useRef(Infinity);
  const lastKeyAt = useRef(-Infinity);
  const nowRef = useRef(now);
  nowRef.current = now;

  useLayoutEffect(() => {
    openedAt.current = Infinity;
    if (question === null || covered) return;
    return afterPaint(() => {
      openedAt.current = nowRef.current();
    });
  }, [question?.nonce, asking, covered]);

  const shown = question !== null || pairing !== null || flagged.length > 0;
  useEffect(() => {
    if (!shown || covered) return;
    function onKey(event: KeyboardEvent) {
      if (isModifierKey(event.key)) return;
      const at = nowRef.current();
      const before = lastKeyAt.current;
      lastKeyAt.current = at;
      // A key an input method is composing belongs to it, whatever its name.
      if (event.isComposing || event.keyCode === 229) return;
      const q = desk.snapshot().questions[0];
      const take = () => {
        event.preventDefault();
        event.stopImmediatePropagation();
      };
      if (q === undefined) {
        if (event.key === "Escape") {
          desk.dismiss();
          take();
        }
        return;
      }
      const plain = isPlainAnswerKey(event as unknown as KeyLike);
      if (event.key === "y" || event.key === "Y") {
        // With a modifier it is an editing key, not this answer: left to whatever it does.
        if (!plain) return;
        take();
        if (event.repeat || desk.answering()) return;
        if (!bypassYesCounts({ now: at, openedAt: openedAt.current, lastKeyAt: before })) return;
        desk.answer(true);
      } else if (event.key === "Escape" || (plain && (event.key === "n" || event.key === "N" || event.key === "Enter"))) {
        take();
        desk.answer(false);
      } else if (plain && event.key === "Tab" && q.kind === "remote_enroll" && q.grants.length > 1) {
        take();
        desk.nextGrant();
      }
    }
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [desk, shown, covered]);

  if (!shown) return null;
  const waiting = questions.length - 1;
  return (
    <div className="remote-desk" role="dialog" aria-label="remote control">
      {question === null && pairing !== null && (
        <div className="remote-pairing">
          <QrCode modules={pairing.modules} />
          <div>
            Within three minutes, scan this with the phone, or give its client this pairing address (it names this
            window and where to connect; the code in it works once):
          </div>
          <code className="remote-pairing-uri">{pairing.uri}</code>
          <div className="remote-hint">Esc closes</div>
        </div>
      )}
      {question?.kind === "remote_enroll" && (
        <div className="remote-question">
          <div>
            Add device <Revealed pieces={revealHidden(question.name)} />, paired{" "}
            <Revealed pieces={revealHidden(question.target)} />, connecting
            through <Revealed pieces={revealHidden(question.host)} /> from{" "}
            <Revealed pieces={revealHidden(question.peer)} />? (y/n)
          </div>
          <HiddenWarning count={hiddenCount(question.name)} what="device name" />
          <div className="remote-grant">
            It may: {question.chosen === "watch" ? "watch" : "watch and steer"}
            {question.grants.length > 1 ? " (Tab changes)" : " (a phone using SSH only watches for now)"}
          </div>
          {refusal !== null && <div className="remote-refusal">{refusal}</div>}
        </div>
      )}
      {question?.kind === "remote_grant" && (
        <div className="remote-question">
          <div>
            Let device <Revealed pieces={revealHidden(question.name)} /> (<Revealed pieces={revealHidden(question.device)} />){" "}
            {grantWords(question.to)}{" "}
            (now it may {grantWords(question.from)})? (y/n)
          </div>
          <HiddenWarning count={hiddenCount(question.name)} what="device name" />
          {refusal !== null && <div className="remote-refusal">{refusal}</div>}
        </div>
      )}
      {question === null &&
        flagged.map((f) => (
          <div className="remote-warning" key={f.device}>
            Device <Revealed pieces={revealHidden(f.name)} /> (<Revealed pieces={revealHidden(f.device)} />) is in the
            device list, but no desk said yes to it. If you did not add it,{" "}
            <code>
              eitri remote revoke <Revealed pieces={revealHidden(f.device)} />
            </code>{" "}
            takes it out. (Esc closes)
          </div>
        ))}
      {waiting > 0 && (
        <div className="remote-hint">
          {waiting === 1 ? "1 more question waits" : `${waiting} more questions wait`}
        </div>
      )}
      {question?.kind === "remote_listen" && (
        <div className="remote-question">
          <div>
            Listen for phones on <Revealed pieces={revealHidden(question.addr)} /> (
            <Revealed pieces={revealHidden(question.iface)} />, {question.link})? (y/N)
          </div>
          {question.plain && <div className="remote-warning">{PLAIN_LINK_WARNING}</div>}
          {refusal !== null && <div className="remote-refusal">{refusal}</div>}
        </div>
      )}
    </div>
  );
}
