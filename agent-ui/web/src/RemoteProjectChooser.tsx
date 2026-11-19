import { useEffect, useRef, useState } from "react";
import { Revealed } from "./components/Revealed";
import { revealHidden, revealPath, revealedPathText } from "./revealHidden";
import { NO_TEXT_ASSIST } from "./textField";

export type RemoteProjectPage =
  | { kind: "hosts"; hosts: string[] }
  | { kind: "question"; id: number; text: string }
  | { kind: "folder"; id: number }
  | { kind: "suggestions"; id: number; prefix: string; folders: string[]; error: string | null }
  | { kind: "line"; text: string }
  | { kind: "error"; text: string };

export type RemoteProjectAction =
  | { kind: "ready" | "cancel" }
  | { kind: "host"; host: string }
  | { kind: "answer"; id: number; yes: boolean }
  | { kind: "folder"; id: number; path: string }
  | { kind: "complete"; id: number; prefix: string };

type Prompt = Extract<RemoteProjectPage, { kind: "hosts" | "question" | "folder" }>;
type Suggestions = Extract<RemoteProjectPage, { kind: "suggestions" }>;

type ChooserWindow = Window & {
  eitriRemoteProject?: (page: RemoteProjectPage) => void;
  webkit?: { messageHandlers?: { eitriRemoteProject?: { postMessage: (text: string) => void } } };
};

function post(action: RemoteProjectAction) {
  (window as ChooserWindow).webkit?.messageHandlers?.eitriRemoteProject?.postMessage(JSON.stringify(action));
}

export function RemoteProjectChooser() {
  const [prompt, setPrompt] = useState<Prompt | null>(null);
  const [suggestions, setSuggestions] = useState<Suggestions | null>(null);
  const [lines, setLines] = useState<string[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [input, setInput] = useState("");
  const [chosenFolder, setChosenFolder] = useState<{ name: string; prefix: string } | null>(null);
  const [completing, setCompleting] = useState(false);
  const panel = useRef<HTMLElement>(null);
  const field = useRef<HTMLInputElement>(null);
  const folderId = useRef<number | null>(null);
  const completeFor = useRef<string | null>(null);

  useEffect(() => {
    const host = window as ChooserWindow;
    host.eitriRemoteProject = (page) => {
      switch (page.kind) {
        case "line":
          setLines((old) => [...old.slice(-999), page.text]);
          break;
        case "error":
          folderId.current = null;
          setPrompt(null);
          setError(page.text);
          break;
        case "suggestions":
          if (folderId.current === page.id) {
            setPrompt((current) => current ?? { kind: "folder", id: page.id });
            if (completeFor.current === page.prefix) setSuggestions(page);
            setCompleting(false);
          }
          break;
        default:
          if (page.kind !== "question") {
            folderId.current = page.kind === "folder" ? page.id : null;
            setInput(page.kind === "folder" ? "/" : "");
            setChosenFolder(null);
            completeFor.current = null;
            setCompleting(false);
          }
          setPrompt(page);
          setSuggestions(null);
      }
    };
    post({ kind: "ready" });
    return () => { delete host.eitriRemoteProject; };
  }, []);

  useEffect(() => {
    if (field.current) field.current.focus();
    else panel.current?.focus();
  }, [prompt]);

  const end = useRef<HTMLDivElement>(null);
  useEffect(() => {
    end.current?.scrollIntoView?.({ block: "nearest" });
  }, [lines, prompt, suggestions, error]);

  function answer(id: number, yes: boolean) {
    setPrompt(null);
    post({ kind: "answer", id, yes });
  }

  function selectHost(host: string) {
    if (!host.trim()) return;
    setPrompt(null);
    post({ kind: "host", host: host.trim() });
  }

  const folder = chosenFolder?.name ?? input;
  const clearCompletion = (value: string, choice: typeof chosenFolder = null) => {
    setInput(value);
    setChosenFolder(choice);
    completeFor.current = null;
    setSuggestions(null);
  };

  return (
    <section
      ref={panel}
      className="chooser remote-project-chooser"
      tabIndex={-1}
      aria-labelledby="remote-project-title"
      onKeyDown={(event) => {
        if (event.nativeEvent.isComposing || event.ctrlKey || event.altKey || event.metaKey) return;
        if (event.key === "Escape") {
          event.preventDefault();
          post({ kind: "cancel" });
        }
        if (prompt?.kind === "question" && !event.repeat && /^[yn]$/i.test(event.key)) {
          event.preventDefault();
          answer(prompt.id, event.key.toLowerCase() === "y");
        }
      }}
    >
      <header className="chooser-header">
        <span className="chooser-title" id="remote-project-title">Open on a host…</span>
        <button type="button" onClick={() => post({ kind: "cancel" })}>{error ? "Close" : "Cancel"}</button>
      </header>
      <div className="remote-project-lines" role="log" aria-label="First connect">
        {lines.map((line, i) => <div key={i}><Revealed pieces={revealHidden(line)} /></div>)}
      </div>
      {error && <p role="alert"><Revealed pieces={revealHidden(error)} /></p>}
      {prompt?.kind === "hosts" && (
        <form onSubmit={(event) => { event.preventDefault(); selectHost(input); }}>
          <label htmlFor="remote-host">SSH host</label>
          <input id="remote-host" ref={field} className="chooser-filter" {...NO_TEXT_ASSIST}
            value={input} onChange={(event) => setInput(event.target.value)} required />
          <button type="submit">Connect</button>
          <ul className="chooser-list">
            {prompt.hosts.filter((host) => host.includes(input)).map((host) => (
              <li key={host}><button type="button" className="chooser-row" onClick={() => selectHost(host)}>
                <Revealed pieces={revealPath(host)} />
              </button></li>
            ))}
          </ul>
        </form>
      )}
      {prompt?.kind === "question" && (
        <div className="remote-project-actions">
          <p><Revealed pieces={revealPath(prompt.text)} /></p>
          <button type="button" onClick={() => answer(prompt.id, true)}>Yes (y)</button>
          <button type="button" onClick={() => answer(prompt.id, false)}>No (n)</button>
        </div>
      )}
      {prompt?.kind === "folder" && (
        <form onSubmit={(event) => {
          event.preventDefault();
          if (completing || !folder.startsWith("/")) return;
          folderId.current = null;
          setPrompt(null);
          post({ kind: "folder", id: prompt.id, path: folder.replace(/\/+$/, "") || "/" });
        }}>
          <label htmlFor="remote-folder">Server folder</label>
          <input id="remote-folder" ref={field} className="chooser-filter" {...NO_TEXT_ASSIST}
            value={input} onChange={(event) => {
              const value = chosenFolder && input !== chosenFolder.name ? chosenFolder.prefix : event.target.value;
              clearCompletion(value);
            }} required />
          <div className="remote-project-actions">
            <button type="submit" disabled={!folder.startsWith("/") || completing}>Open folder</button>
            <button type="button" disabled={!folder.startsWith("/") || completing} onClick={() => {
              completeFor.current = folder;
              setCompleting(true);
              setSuggestions(null);
              post({ kind: "complete", id: prompt.id, prefix: folder });
            }}>{completing ? "Reading folders…" : "Complete folder"}</button>
          </div>
          {suggestions?.id === prompt.id && (
            suggestions.error ? <p role="alert"><Revealed pieces={revealHidden(suggestions.error)} /></p>
            : suggestions.folders.length === 0 ? <p className="chooser-muted">No matching folders</p>
            : <ul className="chooser-list">
              {suggestions.folders.map((name) => (
                <li key={name}><button type="button" className="chooser-row" onClick={() => {
                  clearCompletion(revealedPathText(name), { name, prefix: chosenFolder?.prefix ?? input });
                  field.current?.focus();
                }}><Revealed pieces={revealPath(name)} /></button></li>
              ))}
            </ul>
          )}
        </form>
      )}
      <div ref={end} />
    </section>
  );
}
