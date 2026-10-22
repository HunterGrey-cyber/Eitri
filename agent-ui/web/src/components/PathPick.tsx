import type { PathRef } from "../paths";
import { HINT_ALPHABET } from "../nav";

export function pathLabel(i: number): string {
  return HINT_ALPHABET[i] ?? "";
}

/** `gf` with several paths: each with a HINT letter, in the footer, until a letter picks one. */
export function PathPick({ paths }: { paths: PathRef[] }) {
  return (
    <span className="path-pick" role="listbox">
      {paths
        .map((p, i) => `${pathLabel(i)} ${p.path}${p.line === null ? "" : `:${p.line}`}`)
        .join(" · ")}
    </span>
  );
}
