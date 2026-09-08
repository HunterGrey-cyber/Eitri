type Props = {
  onStart: (mode: "auto" | "bypass") => void;
};

export function ModeSelector({ onStart }: Props) {
  return (
    <div className="mode-selector">
      <p>Choose a permission mode for this conversation (cannot be changed after the first message):</p>
      <button onClick={() => onStart("auto")}>Auto (recommended — prompts for risky tool calls)</button>
      <button onClick={() => onStart("bypass")}>Bypass (no prompts, everything proceeds)</button>
    </div>
  );
}
