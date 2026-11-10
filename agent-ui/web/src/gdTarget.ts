import { parsePath, pathAt, pathsInText } from "./paths";

export type Cited = { path: string; line: number };
export type GdTarget =
  | { kind: "pathline"; path: string; line: number; endLine?: number }
  | { kind: "path"; path: string }
  | { kind: "name"; name: string; cited: Cited | null }
  | { kind: "none" };

/** Alphabetic characters (letters and the marks that spell them) and numbers: the set Rust's `char::is_alphanumeric`
 *  takes, so the page and the host refuse the same names. */
const NAME_CHARS = "\\p{Alphabetic}\\p{N}_$.:\\-";
const NAME = new RegExp(`^[${NAME_CHARS}]+$`, "u");
const NAME_RUN = new RegExp(`[${NAME_CHARS}]+`, "gu");

export function validName(name: string): boolean {
  return NAME.test(name) && new TextEncoder().encode(name).length <= 200;
}

/* File extensions a bare name like `README.md` or `App.tsx` ends with. A capitalised head (`Type.name`) or member
   (`pkg.Name`) reads as a qualified name, but not when what follows the last dot is one of these: then it is a file,
   opened as `gf` would. Words that are also common member names (`lock`, `log`, `map`, `mod`, `env`) are left out. */
const FILE_EXTENSIONS = new Set([
  "md", "mdx", "markdown", "rst", "txt", "rs", "go", "ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs", "py", "pyi",
  "toml", "json", "jsonc", "json5", "yaml", "yml", "lua", "c", "h", "cc", "cpp", "cxx", "hpp", "hh", "java", "kt", "kts",
  "swift", "rb", "sh", "bash", "zsh", "fish", "css", "scss", "sass", "less", "html", "htm", "xml", "svg", "sql",
  "proto", "cfg", "ini", "conf", "vue", "svelte", "nix", "gradle", "tf", "zig", "exs", "erl", "hrl", "hs", "ml", "mli",
  "php", "cs", "dart", "scala", "clj", "vim", "tex", "csv", "tsv", "ipynb", "cmake",
]);

/** The hard line is supplied separately so a selected name keeps its nearby citation. */
export function classify(text: string, line: string): GdTarget {
  const value = text.trim().replace(/[.:]+$/, "");
  if (value === "") return { kind: "none" };
  const range = /^(.*:\d+)-(\d+)$/.exec(value);
  const ref = parsePath(range?.[1] ?? value);
  // Qualified symbols with a capitalized component overlap the prose path grammar.
  const extension = /\.([a-z0-9]+)$/.exec(value)?.[1];
  const qualified = !value.includes("/") && !/:\d/.test(value) && /(?:^[A-Z][\w$]*\.|\.[A-Z])/.test(value) &&
    !(extension !== undefined && FILE_EXTENSIONS.has(extension));
  if (ref !== null && !qualified) {
    return ref.line === null ? { kind: "path", path: ref.path }
      : { kind: "pathline", path: ref.path, line: ref.line, ...(range ? { endLine: Number(range[2]) } : {}) };
  }
  const cited = pathsInText(line).find((path) => path.line !== null);
  return { kind: "name", name: value, cited: cited ? { path: cited.path, line: cited.line! } : null };
}

/** Paths include slashes; otherwise the caret reads the whole identifier run around it. */
export function wordAt(text: string, offset: number): string {
  const ref = pathAt(text, offset);
  if (ref !== null) {
    const value = ref.path + (ref.line === null ? "" : `:${ref.line}`);
    if (classify(value, text).kind !== "name") return value;
  }
  for (const match of text.matchAll(NAME_RUN)) {
    if (offset >= match.index! && offset < match.index! + match[0].length) return match[0].replace(/[.:]+$/, "");
  }
  return "";
}

/** A quoted code name is advisory data for the disk-line check, never editor source. Only a code span beside the
 *  cited path counts: the first one after it, else the last one before it, with no other span between. It must read
 *  as a name (not a path, not a number or flag alone), so `` `a.go:10` calls `b.go` `` or `` `x.go:3` returns `42` ``
 *  name nothing to expect. */
export function expectedName(line: string, path: string): string | undefined {
  const at = line.indexOf(path);
  if (at < 0) return undefined;
  const spans = [...line.matchAll(/`([^`\n]+)`/g)].map((match) => ({
    start: match.index!, end: match.index! + match[0].length, text: match[1],
  }));
  const own = spans.find((span) => span.start <= at && at + path.length <= span.end);
  const from = own?.end ?? at + path.length;
  const to = own?.start ?? at;
  const after = spans.find((span) => span !== own && span.start >= from);
  const before = spans.filter((span) => span !== own && span.end <= to).at(-1);
  return [after, before].map((span) => span?.text).find((name) => name !== undefined && codeName(name, path));
}

function codeName(name: string, path: string): boolean {
  return name !== path && !name.startsWith(`${path}:`) && validName(name) && /[\p{Alphabetic}_$]/u.test(name) &&
    !/^-/.test(name) && classify(name, "").kind === "name";
}
