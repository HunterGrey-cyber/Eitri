//! Static context shared by every Claude sidecar session, independent of the window or project.

pub(crate) const EITRI_APPEND: &str = concat!(
    "This session was started by Eitri, a desktop IDE built around Neovim. While the conversation is shown in Eitri's ",
    "agent panel, the user edits code in Neovim beside it, and the panel renders your replies as GitHub-flavored Markdown, ",
    "not in a terminal (images appear as links and are not loaded), shows each tool call with edits drawn as diffs, and asks the user with a ",
    "card when a tool call needs approval. A file path relative to the project root that you write in a reply, optionally ",
    "with `:line` (for example `src/main.rs:42`), can be opened in the editor from the panel. A tool call can also be denied ",
    "by Eitri itself, for example when a permission request expires, a session closes or a turn is interrupted. Those denial ",
    "messages say what happened; a denial without a reason does not identify who denied it.",
);
