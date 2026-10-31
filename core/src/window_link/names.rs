//! The names of the windows of a group: what a window is called on its command line, in `hello` and in the file names
//! that are its own. Window 1 has no name here: it is the one window that is never started by another.

use sha2::{Digest, Sha256};

use crate::layout::module::{ModuleId, ModuleKind};

/// Window 2's name on its command line.
pub const SECOND_WINDOW: &str = "2";

const CHAT_PREFIX: char = 'c';
const PAGE_PREFIX: &str = "p:";
const PAGE_FILE_PREFIX: &str = "p-";
const MODULE_CHAT_PREFIX: &str = "chat:";
const MODULE_PAGE_PREFIX: &str = "page:";

/// How many hex digits of the id's hash name a page window's files.
const PAGE_FILE_HEX: usize = 8;

/// The longest page id a window's name holds, the plugin id rule's limit.
const MAX_PAGE_ID: usize = 40;

/// The ids a page window can be named by: every plugin id, and a Lua panel's id made of the same plain parts,
/// `[A-Za-z0-9._-]`, at most 40, starting and ending with a letter or a digit. Such an id holds no path separator, no
/// space, no shell character and no control character, and it cannot be read as an option or as `.` or `..`.
fn page_id_ok(id: &str) -> bool {
    let plain = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-');
    let bytes = id.as_bytes();
    match (bytes.first(), bytes.last()) {
        (Some(first), Some(last)) => {
            bytes.len() <= MAX_PAGE_ID
                && first.is_ascii_alphanumeric()
                && last.is_ascii_alphanumeric()
                && bytes.iter().all(|b| plain(*b))
        }
        _ => false,
    }
}

/// A window that window 1 starts. A name that comes from a command line, a message or a module id comes through
/// [`WindowName::parse`] or [`WindowName::for_module`], so a page's id has passed [`page_id_ok`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WindowName {
    /// Window 2 of a linked pair.
    Second,
    /// A chat view's own window, 1 to 9.
    Chat(u8),
    /// A page's own window, by its plugin's or Lua panel's id.
    Page(String),
}

impl WindowName {
    /// `2`, `c1` to `c9` or `p:<id>`; anything else is refused by name.
    pub fn parse(text: &str) -> Result<WindowName, String> {
        if text == SECOND_WINDOW {
            return Ok(WindowName::Second);
        }
        if let Some(id) = text.strip_prefix(PAGE_PREFIX) {
            if page_id_ok(id) {
                return Ok(WindowName::Page(id.to_owned()));
            }
            return Err(format!("{id:?} is not a page's id"));
        }
        if let Some(number) = text.strip_prefix(CHAT_PREFIX).and_then(one_digit) {
            return Ok(WindowName::Chat(number));
        }
        Err(format!("{text:?} is not a window's name"))
    }

    /// The inverse of [`WindowName::parse`].
    pub fn text(&self) -> String {
        match self {
            WindowName::Second => SECOND_WINDOW.to_owned(),
            WindowName::Chat(n) => format!("{CHAT_PREFIX}{n}"),
            WindowName::Page(id) => format!("{PAGE_PREFIX}{id}"),
        }
    }

    /// The part of a file name that says which window it is. A page's is a hash of fixed length, so a socket name
    /// built from it fits `sun_path` whatever the page's id, and the id stays out of file names.
    pub fn file_form(&self) -> String {
        match self {
            WindowName::Second | WindowName::Chat(_) => self.text(),
            WindowName::Page(id) => {
                let hex: String = Sha256::digest(id.as_bytes())
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect();
                format!("{PAGE_FILE_PREFIX}{}", &hex[..PAGE_FILE_HEX])
            }
        }
    }

    /// The module this window exists to show; window 2 holds whatever it is given, so it has none.
    pub fn module(&self) -> Option<ModuleId> {
        match self {
            WindowName::Second => None,
            WindowName::Chat(n) => ModuleId::instance(ModuleKind::Agent, *n).ok(),
            WindowName::Page(id) => Some(ModuleId::lua(id)),
        }
    }

    /// The window that shows `id` on its own: the inverse of [`WindowName::module`]. An editor, a terminal or a canvas
    /// never has one, and neither has a Lua panel whose id [`page_id_ok`] refuses.
    pub fn for_module(id: &ModuleId) -> Option<WindowName> {
        match id.kind() {
            ModuleKind::Agent => Some(WindowName::Chat(id.number())),
            ModuleKind::LuaWebview => {
                let name = id.as_str().strip_prefix("lua:")?;
                page_id_ok(name).then(|| WindowName::Page(name.to_owned()))
            }
            ModuleKind::Editor | ModuleKind::Canvas | ModuleKind::Terminal => None,
        }
    }

    pub fn chat_number(&self) -> Option<u8> {
        match self {
            WindowName::Chat(n) => Some(*n),
            WindowName::Second | WindowName::Page(_) => None,
        }
    }

    /// What `--module` says for this window; window 2 is started without it.
    pub fn module_arg(&self) -> Option<ModuleArg> {
        match self {
            WindowName::Second => None,
            WindowName::Chat(n) => Some(ModuleArg::Chat(*n)),
            WindowName::Page(id) => Some(ModuleArg::Page(id.clone())),
        }
    }
}

/// `--module chat:<n>` or `--module page:<id>`: the module a linked window is started to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleArg {
    Chat(u8),
    Page(String),
}

impl ModuleArg {
    /// Held to the rule of the window it names.
    pub fn parse(text: &str) -> Result<ModuleArg, String> {
        let name = if let Some(number) = text.strip_prefix(MODULE_CHAT_PREFIX) {
            WindowName::parse(&format!("{CHAT_PREFIX}{number}"))
        } else if let Some(id) = text.strip_prefix(MODULE_PAGE_PREFIX) {
            WindowName::parse(&format!("{PAGE_PREFIX}{id}"))
        } else {
            Err(format!(
                "{text:?} is neither {MODULE_CHAT_PREFIX}<n> nor {MODULE_PAGE_PREFIX}<id>"
            ))
        };
        name?.module_arg().ok_or_else(|| format!("{text:?} names no module"))
    }

    /// The inverse of [`ModuleArg::parse`].
    pub fn text(&self) -> String {
        match self {
            ModuleArg::Chat(n) => format!("{MODULE_CHAT_PREFIX}{n}"),
            ModuleArg::Page(id) => format!("{MODULE_PAGE_PREFIX}{id}"),
        }
    }

    /// The name a window started with this argument must have.
    pub fn name(&self) -> WindowName {
        match self {
            ModuleArg::Chat(n) => WindowName::Chat(*n),
            ModuleArg::Page(id) => WindowName::Page(id.clone()),
        }
    }
}

/// `1` to `9`, one ASCII digit, so a name has one spelling.
fn one_digit(digits: &str) -> Option<u8> {
    match digits.as_bytes() {
        [digit @ b'1'..=b'9'] => Some(digit - b'0'),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::module::ModuleKind;

    fn chats() -> Vec<WindowName> {
        (1..=9).map(WindowName::Chat).collect()
    }

    #[test]
    fn window_names_round_trip() {
        let mut names = chats();
        names.push(WindowName::Second);
        names.push(WindowName::Page("markdown".into()));
        for name in names {
            assert_eq!(WindowName::parse(&name.text()), Ok(name.clone()));
        }
        assert_eq!(WindowName::Second.text(), "2");
        assert_eq!(WindowName::Chat(3).text(), "c3");
        assert_eq!(WindowName::Page("markdown".into()).text(), "p:markdown");

        assert_eq!(WindowName::Second.module(), None);
        assert_eq!(WindowName::Chat(1).module(), Some(ModuleId::agent()));
        assert_eq!(
            WindowName::Chat(4).module(),
            Some(ModuleId::instance(ModuleKind::Agent, 4).unwrap())
        );
        assert_eq!(
            WindowName::Page("markdown".into()).module(),
            Some(ModuleId::lua("markdown"))
        );
        for name in chats().into_iter().chain([WindowName::Page("markdown".into())]) {
            let id = name.module().unwrap();
            assert_eq!(WindowName::for_module(&id), Some(name));
        }
        assert_eq!(WindowName::for_module(&ModuleId::editor()), None);
        assert_eq!(WindowName::for_module(&ModuleId::terminal()), None);
        assert_eq!(
            WindowName::for_module(&ModuleId::instance(ModuleKind::Terminal, 2).unwrap()),
            None
        );
        assert_eq!(WindowName::for_module(&ModuleId::parse("canvas").unwrap()), None);

        assert_eq!(WindowName::Chat(7).chat_number(), Some(7));
        assert_eq!(WindowName::Second.chat_number(), None);
        assert_eq!(WindowName::Page("markdown".into()).chat_number(), None);
    }

    #[test]
    fn a_lua_panels_plain_id_names_its_window() {
        for id in ["preview_panel", "Todo_List", "UPPER", "notes.v2", "a-b"] {
            let name = WindowName::for_module(&ModuleId::lua(id)).expect(id);
            assert_eq!(name, WindowName::Page(id.to_owned()));
            assert_eq!(name.module(), Some(ModuleId::lua(id)));
            assert_eq!(WindowName::parse(&name.text()), Ok(name.clone()));
            let arg = name.module_arg().unwrap();
            assert_eq!(ModuleArg::parse(&arg.text()), Ok(arg));
            assert!(name.file_form().starts_with("p-") && name.file_form().len() == 10);
        }
    }

    #[test]
    fn a_lua_panel_whose_id_is_not_plain_has_no_window() {
        for id in [
            "My Panel", "a/b", "../x", "-a", "a-", "..", ".a", "a#2", "a\nb", "\u{e9}",
        ] {
            assert_eq!(WindowName::for_module(&ModuleId::lua(id)), None, "{id:?}");
        }
    }

    #[test]
    fn window_names_refuse_what_a_file_or_argv_cannot_hold() {
        let too_long = format!("p:{}", "a".repeat(41));
        let refused = [
            "c0",
            "c10",
            "c",
            "c01",
            "c+1",
            "c\u{0663}",
            "p:",
            "p:../x",
            "p:a/b",
            "p:-a",
            "p:a-",
            "p:a;b",
            "p:a b",
            "p:a\n",
            &too_long,
            "3",
            "1",
            "",
            "22",
            "2 ",
            "C1",
            "P:a",
        ];
        for text in refused {
            assert!(WindowName::parse(text).is_err(), "{text:?} is refused");
        }
        assert!(WindowName::parse(&format!("p:{}", "a".repeat(40))).is_ok());
    }

    #[test]
    fn a_page_windows_file_form_is_eight_hex_of_its_ids_hash() {
        let form = WindowName::Page("markdown".into()).file_form();
        assert_eq!(form, "p-bc18f706", "the first 8 hex digits of sha256(\"markdown\")");
        let hex = form.strip_prefix("p-").unwrap();
        assert_eq!(hex.len(), 8);
        assert!(hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        assert_eq!(WindowName::Second.file_form(), "2");
        assert_eq!(WindowName::Chat(3).file_form(), "c3");
    }

    #[test]
    fn a_module_argument_names_the_window_it_starts() {
        assert_eq!(ModuleArg::parse("chat:3"), Ok(ModuleArg::Chat(3)));
        assert_eq!(
            ModuleArg::parse("page:markdown"),
            Ok(ModuleArg::Page("markdown".into()))
        );
        for text in [
            "chat:0",
            "chat:10",
            "chat:",
            "chat:x",
            "page:",
            "page:a/b",
            "page:-a",
            "chat",
            "page",
            "c3",
            "p:markdown",
            "",
        ] {
            assert!(ModuleArg::parse(text).is_err(), "{text:?} is refused");
        }
        for arg in [ModuleArg::Chat(3), ModuleArg::Page("markdown".into())] {
            assert_eq!(ModuleArg::parse(&arg.text()), Ok(arg.clone()));
            assert_eq!(arg.name().module_arg(), Some(arg));
        }
        assert_eq!(ModuleArg::Chat(3).name(), WindowName::Chat(3));
        assert_eq!(WindowName::Second.module_arg(), None);
    }
}
