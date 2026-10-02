import type { RevealedPiece } from "../revealHidden";

/** Text a person approves, with each character `revealHidden` found drawn as its outlined escape. */
export function Revealed({ pieces }: { pieces: RevealedPiece[] }) {
  return (
    <>
      {pieces.map((piece, i) =>
        "text" in piece ? (
          piece.text
        ) : (
          <span key={i} className="permission-card-escape">
            {piece.escape}
          </span>
        ),
      )}
    </>
  );
}

/** One line under what was shown saying why it holds `⟨U+…⟩`, only when it does. */
export function HiddenWarning({ count, what }: { count: number; what: string }) {
  if (count === 0) return null;
  const characters = count === 1 ? "character" : "characters";
  return (
    <div className="permission-card-warning">
      the {what} holds {count} invisible or direction-changing {characters}, shown as ⟨U+…⟩
    </div>
  );
}
