import { HINT_ALPHABET } from "../nav";

/** R6 (v1 picks, Task 8): `gx` with several links, or a titled one, or one nobody can see -- each with its
 *  HINT letter and its FULL normalized address, in the footer, until a letter picks one. The same shape as
 *  `PathPick`. Nothing is cut: an address shortened to fit would hide exactly the part a disguised link
 *  depends on, so a long one wraps (`.link-pick`, `overflow-wrap`) instead. The entries are joined by ` · `,
 *  unambiguous because a normalized address holds no space. */
export function LinkPick({ urls }: { urls: string[] }) {
  return (
    <span className="link-pick" role="listbox">
      {urls.map((url, i) => `${HINT_ALPHABET[i] ?? ""} ${url}`).join(" · ")}
    </span>
  );
}
