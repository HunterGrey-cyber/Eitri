/** What every free-text field of the panel spreads onto its `<input>` or `<textarea>`. macOS (and
 *  WebKit there) capitalises the first letter, corrects words and underlines them on its own unless a
 *  field says otherwise; none of these fields holds prose a spell checker should touch -- a prompt, a
 *  filter, a command line, a name -- and a capital the user did not type changes what is sent. */
export const NO_TEXT_ASSIST = {
  autoCapitalize: "off",
  autoCorrect: "off",
  spellCheck: false,
} as const;
