//! Every sentence more than one tool or window says about a call, once.

/// A call open while its tab moved. Both windows reach the same editor, so the call can simply be made again.
pub const MOVED: &str = "the tab moved to the other window; try again";

/// A window-2 tab asked for what only window 1's own judgement could admit.
pub const LINK: &str = "outside the project, or through a link; move the tab to the editor's window to be asked";

/// A tool that runs only beside the editor, asked from a window-2 tab.
pub const MOVE_TO_EDITOR: &str = "move the tab to the editor's window";

/// A link to window 1 that is down.
pub const WINDOW_CLOSED: &str = "the editor's window is closed";

/// A call to show something, past its budget.
pub const NOT_SHOWN: &str = "the editor did not answer within 5 s; nothing was shown";

/// A call to read something, past its budget.
pub const NOT_READ: &str = "the editor did not answer within 5 s; nothing was read";

/// What follows a file's name when a swap file kept it from loading.
pub const HAS_SWAP: &str = "has a swap file (another editor, or a crash); it was not opened";

/// One tab's queue is full.
pub const TOO_MANY: &str = "too many editor calls are waiting";

/// A card that waited out its limit.
pub const NOT_ANSWERED: &str = "the person did not answer";

/// A card denied.
pub const NOT_ALLOWED: &str = "the person did not allow it";

/// The turn that held a bridge call ended interrupted.
pub const INTERRUPTED: &str = "the turn was interrupted";

/// No window can reach an editor at all.
pub const NO_EDITOR: &str = "the editor is not running";

/// A tool's answer over the cap, past its own cutting.
pub const TOO_LARGE: &str = "the answer was too large; ask for less";

/// What INPUT's `Ctrl+y` flashes with only a broker card waiting.
pub const CTRL_Y_NOT_A_CARD: &str = "Ctrl+y does not answer an editor card: Esc, then a or d on the card";
