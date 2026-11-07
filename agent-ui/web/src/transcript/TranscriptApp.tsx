import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { MouseEvent } from "react";
import { PromptAtDesk, QuestionAtDesk } from "../components/AtDesk";
import { MessageList } from "../components/MessageList";
import { Revealed } from "../components/Revealed";
import { themeVars } from "../preview/themes";
import { revealHidden } from "../revealHidden";
import { applyTheme } from "../theme";
import type { Transport } from "../transport";
import { applyFrame, initialTranscript, starts, TRANSCRIPT_PROTOCOL, wantMore } from "./frames";
import type { ThemeTokens, TranscriptModel } from "./frames";

/** Everything this page ever posts to its host. It changes nothing on the desk: it says it is ready, asks for older
 *  turns or a fresh snapshot, and hands over a link the person tapped, which the host confirms and opens. */
export type PageToHost =
  | { kind: "ready"; protocol: number }
  | { kind: "more"; key: string; generation: number; before_turn: number }
  | { kind: "open_link"; url: string }
  | { kind: "want_snapshot"; key: string };

/** The turn an entry of `seq` belongs to, by the same turn boundaries pages are applied with. */
function turnOf(m: TranscriptModel, seq: number): number | null {
  if (m.firstTurn === null) return null;
  const boundaries = starts(m.state, m.firstTurn);
  let index = 0;
  boundaries.forEach((start, i) => {
    if (start <= seq) index = i;
  });
  return m.firstTurn + index;
}

/** Whether `m` still holds only the start of the entry `seq`. */
function holdsCut(m: TranscriptModel, seq: number): boolean {
  return m.cut.some((entry) => entry.entry_id.slice(entry.entry_id.lastIndexOf(":") + 1) === String(seq));
}

const HEX_COLOUR = /^#[0-9a-fA-F]{3,8}$/;

/** The built-in pair the panel preview uses, by the phone's own light or dark, for the time before the desk's
 *  colours arrive. Colours only: the fonts and sizes are the phone's (`transcript.css`). */
function builtInTheme(w: Window): ThemeTokens {
  const light = typeof w.matchMedia === "function" && w.matchMedia("(prefers-color-scheme: light)").matches;
  const vars = Object.fromEntries(Object.entries(themeVars(light ? "catppuccin-latte" : "tokyonight-night"))
    .filter(([, value]) => HEX_COLOUR.test(value)));
  return { vars, scheme: light ? "light" : "dark" };
}

function paint(theme: ThemeTokens): void {
  applyTheme(theme.vars);
  document.documentElement.style.setProperty("--nv-color-scheme", theme.scheme === "light" ? "light" : "dark");
}

/** The address a tapped link hands the host: an absolute `http`, `https` or `mailto` address, normalized; nothing
 *  for any other (a relative link, a fragment, another scheme), which the page then leaves unopened. */
function linkTarget(raw: string): string | null {
  try {
    const url = new URL(raw);
    return ["http:", "https:", "mailto:"].includes(url.protocol) ? url.href : null;
  } catch {
    return null;
  }
}

function endedText(how: string): string {
  if (how === "failed") return "The session failed at the desk.";
  if (how === "lost") return "The session ended at the desk: its process went away.";
  return "The session ended at the desk.";
}

/** The phone's transcript: the message list of the one tab the host follows, fed the host's frames, with the
 *  greeting's prompt, notices, root note and running `!` line shown and nothing that answers or sends. */
export function TranscriptApp({ transport }: { transport: Transport }) {
  // The model is folded here, frame by frame, and rendered from the latest fold: what to post (a snapshot request
  // after a gap, one `more` at a time) is decided on the model as it is after each frame, not on what was last drawn,
  // so several frames between two renders lose nothing.
  const modelRef = useRef<TranscriptModel | null>(null);
  modelRef.current ??= initialTranscript();
  const [model, setModel] = useState<TranscriptModel>(modelRef.current);
  const update = (next: TranscriptModel) => {
    modelRef.current = next;
    setModel(next);
  };
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});
  const [behind, setBehind] = useState(false);
  const jumpRef = useRef<(() => void) | null>(null);
  /** The last `more` sent, and the last one the host refused, as `generation:before_turn`: a refused request is
   *  not sent again by reaching the top, only by a tap, until a snapshot starts the list afresh. */
  const lastMoreRef = useRef<string | null>(null);
  const refusedMoreRef = useRef<string | null>(null);
  /** A cut entry tapped while its page could not be asked for yet (another is on its way, or a snapshot is): asked
   *  for as soon as one can be, if the session is the same and the entry is still cut. */
  const [waitingCut, setWaitingCut] = useState<{ seq: number; generation: number | null } | null>(null);
  const waitingCutRef = useRef(waitingCut);
  const waitFor = (next: { seq: number; generation: number | null } | null) => {
    waitingCutRef.current = next;
    setWaitingCut(next);
  };

  const post = (message: PageToHost) => transport.post(JSON.stringify(message));
  const postRef = useRef(post);
  postRef.current = post;
  const updateRef = useRef(update);
  updateRef.current = update;

  useLayoutEffect(() => {
    paint(builtInTheme(window));
  }, []);
  useLayoutEffect(() => {
    if (model.theme !== null) paint(model.theme);
  }, [model.theme]);

  useEffect(() => {
    transport.listen((json) => {
      let frame: unknown = null;
      try {
        frame = JSON.parse(json);
      } catch {
        // `null` is a frame this build cannot read: the update row, nothing else.
      }
      if (frame !== null && typeof frame === "object" && (frame as { kind?: unknown }).kind === "refused") {
        refusedMoreRef.current = lastMoreRef.current;
      }
      const before = modelRef.current!;
      const next = applyFrame(before, frame);
      if (next === before) return;
      updateRef.current(next);
      if ((frame as { kind?: unknown }).kind === "snapshot") refusedMoreRef.current = null;
      // One request per gap: asked when the model starts wanting a snapshot, again only after one has come.
      if (next.wantSnapshot && !before.wantSnapshot && next.key !== null && !next.stopped) {
        postRef.current({ kind: "want_snapshot", key: next.key });
      }
      const waiting = waitingCutRef.current;
      if (waiting !== null) {
        if (waiting.generation !== next.generation || !holdsCut(next, waiting.seq) || next.stopped || next.gone) {
          waitFor(null);
        } else if (!next.owedPage && !next.wantSnapshot) {
          loadWholeRef.current(waiting.seq);
        }
      }
    });
    postRef.current({ kind: "ready", protocol: TRANSCRIPT_PROTOCOL });
  }, [transport]);

  /** Sends `more` for the turns before `beforeTurn` (the first turn held when not given); whether it went out. */
  const requestMore = (beforeTurn: number | undefined, byTap: boolean): boolean => {
    const m = modelRef.current!;
    const request = wantMore(m, beforeTurn);
    if (request === null) return false;
    const id = `${request.generation}:${request.before_turn}`;
    if (!byTap && refusedMoreRef.current === id) return false;
    lastMoreRef.current = id;
    update({ ...m, owedPage: true });
    post(request);
    return true;
  };
  /** A cut entry's tap: the page holding its turn whole, now or once a page can be asked for. */
  const loadWhole = (seq: number) => {
    const m = modelRef.current!;
    const turn = turnOf(m, seq);
    if (turn === null) return;
    if (requestMore(turn + 1, true)) waitFor(null);
    else if (!m.stopped && !m.gone && m.key !== null) waitFor({ seq, generation: m.generation });
  };
  const loadWholeRef = useRef(loadWhole);
  loadWholeRef.current = loadWhole;

  const cut = useMemo(() => {
    const bySeq = new Map<number, number>();
    for (const entry of model.cut) {
      const seq = Number(entry.entry_id.slice(entry.entry_id.lastIndexOf(":") + 1));
      if (Number.isSafeInteger(seq) && seq >= 0) bySeq.set(seq, entry.bytes);
    }
    return bySeq;
  }, [model.cut]);

  const onClickCapture = (event: MouseEvent<HTMLDivElement>) => {
    const anchor = event.target instanceof Element ? event.target.closest<HTMLAnchorElement>("a[href]") : null;
    if (anchor === null) return;
    // The page opens nothing itself: the host asks the person and opens the address in the phone's browser.
    event.preventDefault();
    const url = linkTarget(anchor.getAttribute("href") ?? "");
    if (url !== null) post({ kind: "open_link", url });
  };

  const waiting = model.state.questions.filter((record) => record.state.kind !== "settled");
  const after = model.prompt !== null || waiting.length > 0 ? (
    <div className="transcript-at-desk">
      {model.prompt !== null && <PromptAtDesk prompt={model.prompt} location={model.location} />}
      {waiting.map((record) => <QuestionAtDesk key={record.interactionId} record={record} location={model.location} />)}
    </div>
  ) : null;

  return (
    <div className="transcript-app" onClickCapture={onClickCapture}>
      <div className="transcript-status" role="status">
        {(model.update || model.stopped) && (
          <div className="update-row">
            {model.stopped
              ? "This desk speaks a newer version than this app: update the app to keep following it."
              : "This desk sent something this app cannot show: update the app to see all of it."}
          </div>
        )}
        {model.gone && <div className="transcript-ended">This tab is no longer open at the desk.</div>}
        {model.ended !== null && <div className="transcript-ended">{endedText(model.ended)}</div>}
        {model.rootNote !== null && <div className="root-note"><Revealed pieces={revealHidden(model.rootNote)} /></div>}
        {model.notices.map((notice, i) => (
          <div key={i} className="transcript-notice"><Revealed pieces={revealHidden(notice)} /></div>
        ))}
        {model.shellRunning && <div className="shell-running">! A shell line started at the desk is running.</div>}
      </div>
      <MessageList
        state={model.state}
        sessionEnded={model.ended !== null || model.gone}
        expanded={expanded}
        cursor={-1}
        focused={false}
        onAnswerPermission={() => {}}
        touch
        deskLocation={model.location}
        thisDevice={model.device === "" ? null : model.device}
        cut={cut}
        cutWaiting={waitingCut?.seq ?? null}
        onLoadWhole={loadWhole}
        onToggle={(key) => setExpanded((open) => ({ ...open, [key]: !open[key] }))}
        onReachTop={() => requestMore(undefined, false)}
        onUnreadChange={(label, jump) => {
          jumpRef.current = jump;
          setBehind(label !== null);
        }}
        after={after}
      />
      {behind && (
        <button type="button" className="transcript-latest" onClick={() => jumpRef.current?.()}>
          latest ↓
        </button>
      )}
    </div>
  );
}
