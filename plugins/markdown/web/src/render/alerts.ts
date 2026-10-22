import type MarkdownIt from "markdown-it";
import githubAlerts from "markdown-it-github-alerts";
import { mark, NONCE_ATTR } from "./env";

export type AlertKind = "note" | "tip" | "important" | "warning" | "caution";

/* GitHub's five kinds, and Obsidian's callout words drawn in the GitHub colour nearest their meaning. */
const KINDS = new Map<string, AlertKind>([
  ...["note", "info", "todo", "abstract", "summary", "tldr", "quote", "cite", "example"].map((w) => [w, "note"] as const),
  ...["tip", "hint", "success", "check", "done"].map((w) => [w, "tip"] as const),
  ...["important", "question", "help", "faq"].map((w) => [w, "important"] as const),
  ...["warning", "attention"].map((w) => [w, "warning"] as const),
  ...["caution", "danger", "error", "failure", "fail", "missing", "bug"].map((w) => [w, "caution"] as const),
]);

/** The colour of an alert word, in any letter case; a word nobody defined is a note. */
export function alertKind(word: string): AlertKind {
  return KINDS.get(word.toLowerCase()) ?? "note";
}

interface AlertMeta { kind: AlertKind; title: string; fold: "" | "-" | "+" }

function capitalize(word: string): string {
  return word.charAt(0).toUpperCase() + word.slice(1);
}

/** GitHub's alerts (`> [!NOTE]`) and Obsidian's callouts (any `[!word]`, a title after the marker, the fold markers
 *  `-` and `+`). The package finds the marker; this rule settles the kind, the title and the fold, closes each alert
 *  at its own end, and renders it with the token's attributes so source lines and the nonce reach the element. */
export function alertsPlugin(md: MarkdownIt): void {
  md.use(githubAlerts, { markers: "*", matchCaseSensitive: false, icons: {} });
  md.core.ruler.after("github-alerts", "eitri-alerts", (state) => {
    const tokens = state.tokens;
    // The package pairs an alert with the first quote close after it, which is a nested quote's when there is one:
    // every close it marked is undone, and each alert is paired with the close at its own level instead.
    for (const t of tokens) {
      if (t.type === "alert_close") {
        t.type = "blockquote_close";
        t.tag = "blockquote";
      }
    }
    for (let i = 0; i < tokens.length; i++) {
      const open = tokens[i];
      if (open.type !== "alert_open") continue;
      const found = open.meta as { type: string; title: string };
      let title = found.title;
      let fold: AlertMeta["fold"] = "";
      const marker = /^([+-])(?:\s+([\s\S]*))?$/.exec(title);
      if (marker) {
        fold = marker[1] as "-" | "+";
        title = (marker[2] ?? "").trim();
      }
      if (title === "") title = capitalize(found.type);
      const tag = fold ? "details" : "div";
      open.tag = tag;
      open.meta = { kind: alertKind(found.type), title, fold } satisfies AlertMeta;
      let close = tokens.length;
      for (let j = i + 1; j < tokens.length; j++) {
        if (tokens[j].nesting === -1 && tokens[j].level === open.level) {
          tokens[j].type = "alert_close";
          tokens[j].tag = tag;
          close = j;
          break;
        }
      }
      // A marker alone in its paragraph (a nested quote or a list follows it) leaves that paragraph empty, which
      // would draw as a blank line under the title.
      const first = tokens.findIndex((t, j) => j > i && j < close && t.type === "inline");
      if (first > 0 && tokens[first].content.trim() === "" && tokens[first - 1].type === "paragraph_open" &&
        tokens[first + 1]?.type === "paragraph_close") {
        tokens[first - 1].hidden = true;
        tokens[first + 1].hidden = true;
      }
    }
    return false;
  });

  md.renderer.rules.alert_open = (tokens, idx, _options, env, slf) => {
    const token = tokens[idx];
    const meta = token.meta as AlertMeta;
    const nonce = (env as { nonce: string }).nonce;
    token.attrJoin("class", `markdown-alert markdown-alert-${meta.kind}`);
    token.attrSet(NONCE_ATTR, nonce);
    const title = md.utils.escapeHtml(meta.title);
    if (meta.fold) {
      if (meta.fold === "+") token.attrSet("open", "");
      return `<details${slf.renderAttrs(token)}><summary class="markdown-alert-title"${mark({ nonce })}>${title}</summary>\n`;
    }
    return `<div${slf.renderAttrs(token)}><p class="markdown-alert-title"${mark({ nonce })}>${title}</p>\n`;
  };
  md.renderer.rules.alert_close = (tokens, idx) => (tokens[idx].tag === "details" ? "</details>\n" : "</div>\n");
}
