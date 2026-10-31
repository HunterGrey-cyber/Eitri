//! The presets: what each choice gives and the table a generated file starts from.

use super::display::display_text;
use super::lua_text::quote;
use super::text::{char_columns, columns, fill, text, TextId};
use super::{Build, Choice, Generated, Lang, PresetId, Tiling, WmMod};
use crate::keymap::bundles::bundle;
use crate::keymap::hints::WHICH_KEY_DELAY_KEY;
use crate::keymap::stock::STOCK_PREFIX;
use crate::keymap::{action, tmux, KeySpec, Keymap, KeymapOp};
use crate::nvim_verbs::leader::NVIM_PREFIX_KEY;

/// The left-hand sides `keymap.nvim_prefix` may take, in the order they are tried.
pub const NVIM_LHS_CANDIDATES: [&str; 3] = ["<leader>W", "<leader>E", "<leader>z"];

const PREFIX_TABLE: &str = "prefix";
const SEND_PREFIX: &str = "send-prefix";
const I3_ALT: &str = "i3-alt";
pub(crate) const TILING_KEY: &str = "window.tiling";
const WHICH_KEY_DELAY_MS: u32 = 200;
/// The name `{wm}` falls back to; preset 5 is offered only once a compositor has answered with its name.
const UNNAMED_COMPOSITOR: &str = "compositor";
/// The column a trailing comment starts at.
const NOTE_COLUMN: usize = 54;
/// The longest line a long comment is folded to.
const COMMENT_WIDTH: usize = 100;

/// A prefix-table call the file makes.
enum Call {
    Prefix(KeySpec),
    Del(KeySpec),
    SendPrefix(KeySpec),
}

impl Call {
    fn op(&self) -> KeymapOp {
        match self {
            Call::Prefix(key) => KeymapOp::Prefix { key: key.to_string() },
            Call::Del(key) => KeymapOp::Del {
                table: PREFIX_TABLE.to_string(),
                key: key.to_string(),
            },
            Call::SendPrefix(key) => KeymapOp::Set {
                table: PREFIX_TABLE.to_string(),
                key: key.to_string(),
                action: SEND_PREFIX.to_string(),
                opts: Vec::new(),
            },
        }
    }

    fn line(&self) -> String {
        match self {
            Call::Prefix(key) => format!("eitri.keymap.prefix({})", quote(&key.to_string())),
            Call::Del(key) => format!("eitri.keymap.del({}, {})", quote(PREFIX_TABLE), quote(&key.to_string())),
            Call::SendPrefix(key) => format!(
                "eitri.keymap.set({}, {}, {})",
                quote(PREFIX_TABLE),
                quote(&key.to_string()),
                quote(SEND_PREFIX)
            ),
        }
    }
}

/// A setting's value, as the kernel stores it and as the file writes it.
enum Value {
    Text(&'static str),
    Number(u32),
}

impl Value {
    fn stored(&self) -> String {
        match self {
            Value::Text(text) => (*text).to_string(),
            Value::Number(number) => number.to_string(),
        }
    }

    fn literal(&self) -> String {
        match self {
            Value::Text(text) => quote(text),
            Value::Number(number) => number.to_string(),
        }
    }
}

fn setting_line(key: &str, value: &Value) -> String {
    format!("eitri.config.set({}, {})", quote(key), value.literal())
}

/// The file as it is built: each helper records the data first and renders the line from it, so the text and the
/// data can differ only through a bug the replay catches.
struct File {
    lang: Lang,
    lines: Vec<String>,
    ops: Vec<KeymapOp>,
    settings: Vec<(String, String)>,
}

impl File {
    fn say(&self, id: TextId, args: &[(&str, &str)]) -> String {
        fill(text(self.lang, id), args)
    }

    fn blank(&mut self) {
        self.lines.push(String::new());
    }

    fn comment(&mut self, text: &str) {
        self.lines.push(format!("-- {text}"));
    }

    /// A comment folded once at the last space that fits, for a sentence too long to read on one line.
    fn folded_comment(&mut self, text: &str) {
        let room = COMMENT_WIDTH - "-- ".len();
        let mut used = 0;
        let mut fold = None;
        for (at, c) in text.char_indices() {
            if used > room {
                break;
            }
            if c == ' ' {
                fold = Some(at);
            }
            used += char_columns(c);
        }
        match fold {
            Some(at) if columns(text) > room => {
                self.comment(&text[..at]);
                self.comment(&text[at + 1..]);
            }
            _ => self.comment(text),
        }
    }

    fn code(&mut self, line: String, note: Option<String>) {
        self.lines.push(match note {
            Some(note) => format!("{line:<width$}  -- {note}", width = NOTE_COLUMN - 2),
            None => line,
        });
    }

    fn call(&mut self, call: Call, note: Option<String>) {
        self.ops.push(call.op());
        self.code(call.line(), note);
    }

    fn set(&mut self, key: &str, value: Value, note: Option<String>) {
        self.settings.push((key.to_string(), value.stored()));
        self.code(setting_line(key, &value), note);
    }

    fn use_bundle(&mut self, name: &str) {
        self.ops
            .extend(bundle(name).expect("the presets name bundles that exist"));
        self.lines.push(format!("eitri.keymap.use({})", quote(name)));
    }
}

/// A key as a comment or a summary shows it. A key can come from a tmux configuration, so its one character goes
/// through the same cut as any other value.
fn shown(key: &KeySpec) -> String {
    display_text(&key.human())
}

fn stock_prefix() -> KeySpec {
    KeySpec::parse(STOCK_PREFIX).expect("the stock prefix parses")
}

/// The prefix the file itself sets: what the user chose, or for preset 3, which is that line, what is running.
fn set_prefix(choice: &Choice) -> Option<KeySpec> {
    choice
        .prefix
        .or((choice.preset == PresetId::OtherPrefix).then_some(choice.running_prefix))
}

/// The prefix a window started from `preset`'s file has, before any `eitri.keymap` call of the prefix's own.
fn base_prefix(preset: PresetId, choice: &Choice) -> KeySpec {
    match preset {
        PresetId::NvimNoTmux => stock_prefix(),
        _ => choice.tmux_prefix.unwrap_or_else(stock_prefix),
    }
}

fn effective_prefix(choice: &Choice) -> KeySpec {
    set_prefix(choice).unwrap_or_else(|| base_prefix(choice.preset, choice))
}

fn wm_name(choice: &Choice) -> &'static str {
    choice.wm_name.unwrap_or(UNNAMED_COMPOSITOR)
}

fn nvim_tmux(file: &mut File, choice: &Choice) {
    let key = shown(&base_prefix(PresetId::NvimTmux, choice));
    let keys = if choice.tmux_config_found {
        TextId::CTmuxKeys
    } else {
        TextId::CTmuxNone
    };
    let comment = file.say(keys, &[("key", &key)]);
    file.comment(&comment);
    file.set(tmux::SETTING, Value::Text("on"), None);
    let comment = file.say(TextId::CPanelOpt, &[]);
    file.comment(&comment);
    file.comment(&setting_line(WHICH_KEY_DELAY_KEY, &Value::Number(WHICH_KEY_DELAY_MS)));
}

fn nvim_no_tmux(file: &mut File, choice: &Choice, build: Build) {
    let key = shown(&effective_prefix(choice));
    let comment = file.say(TextId::CNoTmux, &[("key", &key)]);
    file.comment(&comment);
    file.set(tmux::SETTING, Value::Text("off"), None);
    let compositor = build.compositor_mode && choice.tiling == Tiling::Compositor;
    let panel = if compositor {
        TextId::CPanelOnCm
    } else {
        TextId::CPanelOn
    };
    let comment = file.say(panel, &[]);
    file.comment(&comment);
    file.set(WHICH_KEY_DELAY_KEY, Value::Number(WHICH_KEY_DELAY_MS), None);
    if !build.nvim_prefix {
        return;
    }
    match choice.nvim_lhs {
        Some(lhs) => {
            let comment = file.say(TextId::CLeader, &[("lhs", lhs)]);
            file.comment(&comment);
            file.set(NVIM_PREFIX_KEY, Value::Text(lhs), None);
        }
        None => {
            let lhs = NVIM_LHS_CANDIDATES[0];
            let comment = file.say(TextId::CLeaderOff, &[("lhs", lhs)]);
            file.comment(&comment);
            file.comment(&setting_line(NVIM_PREFIX_KEY, &Value::Text(lhs)));
        }
    }
}

fn i3_style(file: &mut File, choice: &Choice, build: Build) {
    match choice.wm_mod {
        WmMod::Alt => {
            let key = shown(&effective_prefix(choice));
            let comment = file.say(TextId::CI3Alt, &[("key", &key)]);
            file.folded_comment(&comment);
            file.use_bundle(I3_ALT);
        }
        WmMod::Super => {
            let comment = file.say(TextId::CI3Super, &[]);
            file.comment(&comment);
        }
    }
    if build.compositor_mode {
        let note = file.say(TextId::CTilingEitri, &[]);
        file.set(TILING_KEY, Value::Text("eitri"), Some(note));
    }
}

fn compositor_tiling(file: &mut File, choice: &Choice) {
    let comment = file.say(TextId::CWindowsWm, &[("wm", wm_name(choice))]);
    file.comment(&comment);
    file.set(TILING_KEY, Value::Text("compositor"), None);
}

/// The lines that set `key` as the prefix. `key` may be the stock prefix itself: its `del` then frees the row
/// whatever a tmux configuration made of it, and the `set` is what keeps the key twice sending itself after a tmux
/// `unbind`.
fn prefix_lines(file: &mut File, choice: &Choice, key: KeySpec) {
    let stock = stock_prefix();
    let human = shown(&key);
    let comment = file.say(TextId::COtherPrefix, &[("key", &human)]);
    file.comment(&comment);
    if choice.press_failed {
        let comment = file.say(TextId::CPressFailed, &[]);
        file.comment(&comment);
    }
    if let Some(tmux_prefix) = choice.tmux_prefix.filter(|prefix| *prefix != key) {
        let comment = file.say(TextId::CTmuxLoses, &[("key", &shown(&tmux_prefix))]);
        file.comment(&comment);
    }
    file.call(Call::Prefix(key), None);
    let was = choice
        .bound_as
        .as_deref()
        .map(|name| file.say(TextId::CWas, &[("action", &display_text(name))]));
    if key == stock {
        file.call(Call::Del(stock), was);
    } else {
        let note = file.say(TextId::CDelDefault, &[("key", &shown(&stock))]);
        file.call(Call::Del(stock), Some(note));
        if was.is_some() {
            file.call(Call::Del(key), was);
        }
    }
    file.call(Call::SendPrefix(key), None);
}

/// The choice of who arranges the windows, on the presets that do not settle it themselves. Off a live tiling
/// compositor there is no choice to state: `auto` resolves to Eitri there.
fn tiling_lines(file: &mut File, choice: &Choice, build: Build) {
    let (true, Some(wm)) = (build.compositor_mode, choice.wm_name) else {
        return;
    };
    match choice.tiling {
        Tiling::Unasked => {}
        Tiling::Eitri => {
            file.blank();
            let comment = file.say(TextId::CWindowsEitri, &[("wm", wm)]);
            file.comment(&comment);
            file.set(TILING_KEY, Value::Text("eitri"), None);
        }
        Tiling::Compositor => {
            file.blank();
            compositor_tiling(file, choice);
        }
    }
}

/// The file `choice` writes. Its comments follow `lang`; its code does not. Only canonical key and action names,
/// the `nvim_prefix` candidates, constants and the date reach it, so nothing detected on this computer does.
pub fn generate(choice: &Choice, build: Build, lang: Lang) -> Generated {
    let mut file = File {
        lang,
        lines: Vec::new(),
        ops: Vec::new(),
        settings: Vec::new(),
    };
    let preset = title(choice.preset, lang);
    let header = file.say(TextId::CHeader1, &[("date", &choice.date), ("preset", preset)]);
    file.comment(&header);
    let header = file.say(TextId::CHeader2, &[]);
    file.comment(&header);
    file.blank();
    match choice.preset {
        PresetId::NvimTmux => nvim_tmux(&mut file, choice),
        PresetId::NvimNoTmux => nvim_no_tmux(&mut file, choice, build),
        PresetId::OtherPrefix => {}
        PresetId::I3Style => i3_style(&mut file, choice, build),
        PresetId::Compositor if build.compositor_mode => compositor_tiling(&mut file, choice),
        PresetId::Compositor => {}
    }
    if let Some(key) = set_prefix(choice) {
        if choice.preset != PresetId::OtherPrefix {
            file.blank();
        }
        prefix_lines(&mut file, choice, key);
    }
    if matches!(
        choice.preset,
        PresetId::NvimTmux | PresetId::NvimNoTmux | PresetId::OtherPrefix
    ) {
        tiling_lines(&mut file, choice, build);
    }
    Generated {
        lines: file.lines,
        ops: file.ops,
        settings: file.settings,
    }
}

/// The presets the screen lists: preset 5 only in a build with compositor mode, once a tiling compositor answered.
pub fn offered(build: Build, tiling_live: bool) -> Vec<PresetId> {
    let mut presets = vec![
        PresetId::NvimTmux,
        PresetId::NvimNoTmux,
        PresetId::OtherPrefix,
        PresetId::I3Style,
    ];
    if build.compositor_mode && tiling_live {
        presets.push(PresetId::Compositor);
    }
    presets
}

/// The preset's name.
pub fn title(id: PresetId, lang: Lang) -> &'static str {
    text(
        lang,
        match id {
            PresetId::NvimTmux => TextId::Preset1,
            PresetId::NvimNoTmux => TextId::Preset2,
            PresetId::OtherPrefix => TextId::Preset3,
            PresetId::I3Style => TextId::Preset4,
            PresetId::Compositor => TextId::Preset5,
        },
    )
}

/// The preset's line of explanation. No summary promises a key the compositor's mode turns off, so the resolved
/// mode changes none of today's wording.
pub fn summary(id: PresetId, choice: &Choice, _tiling_resolved: Tiling, lang: Lang) -> String {
    let template = |id| text(lang, id);
    match id {
        PresetId::NvimTmux => fill(
            template(TextId::Preset1Summary),
            &[(
                "prefix",
                &shown(&choice.prefix.unwrap_or_else(|| base_prefix(id, choice))),
            )],
        ),
        PresetId::NvimNoTmux => template(TextId::Preset2Summary).to_string(),
        PresetId::OtherPrefix => template(TextId::Preset3Summary).to_string(),
        PresetId::I3Style => template(TextId::Preset4Summary).to_string(),
        PresetId::Compositor => fill(template(TextId::Preset5Summary), &[("wm", wm_name(choice))]),
    }
}

/// The first candidate with nothing mapped at or under it. `mapped_lhs` are as nvim's `keytrans` writes them, with
/// the leader spelled out, so each candidate is spelled out the same way before comparing; a candidate ends in one
/// plain character, so a text prefix is a key-sequence prefix.
pub fn free_nvim_lhs(mapped_lhs: &[String], leader: &str) -> Option<&'static str> {
    NVIM_LHS_CANDIDATES.into_iter().find(|candidate| {
        let spelled = candidate.replacen("<leader>", leader, 1);
        !mapped_lhs.iter().any(|lhs| lhs.starts_with(&spelled))
    })
}

/// The prefix table the file's `eitri.keymap` calls meet: the defaults for preset 2, which turns the tmux import off;
/// `running` with the `i3-alt` ops applied for preset 4 on Alt; `running` otherwise. Each row is a key and its
/// canonical action name.
pub fn start_table(preset: PresetId, wm_mod: WmMod, running: &[(KeySpec, String)]) -> Vec<(KeySpec, String)> {
    match (preset, wm_mod) {
        (PresetId::NvimNoTmux, _) => Keymap::defaults()
            .bindings()
            .iter()
            .map(|binding| (binding.key, binding.action.name()))
            .collect(),
        (PresetId::I3Style, WmMod::Alt) => {
            let mut table = running.to_vec();
            for op in bundle(I3_ALT).expect("the presets name bundles that exist") {
                apply(&mut table, &op);
            }
            table
        }
        _ => running.to_vec(),
    }
}

/// One bundle op on a table of names. A key the import removed is simply not there, as `then_user` allows.
fn apply(table: &mut Vec<(KeySpec, String)>, op: &KeymapOp) {
    let parsed_key = |key: &str| KeySpec::parse(key).expect("a bundle's keys parse");
    match op {
        KeymapOp::Del { key, .. } => {
            let key = parsed_key(key);
            table.retain(|(bound, _)| *bound != key);
        }
        KeymapOp::Set { key, action, opts, .. } => {
            let key = parsed_key(key);
            let parsed = action::parse(action, opts, &[]).expect("a bundle's actions parse");
            table.retain(|(bound, _)| *bound != key);
            table.push((key, parsed.action.name()));
        }
        KeymapOp::Prefix { .. } | KeymapOp::Invalid(_) => {}
    }
}

/// The canonical action name `key` has in `table`.
pub fn bound_in(table: &[(KeySpec, String)], key: &KeySpec) -> Option<String> {
    table
        .iter()
        .find(|(bound, _)| bound == key)
        .map(|(_, name)| name.clone())
}

/// The cases every check of a generated file runs over.
#[cfg(test)]
pub(crate) mod test_matrix {
    use super::*;
    use crate::keymap::Keymap;

    pub(crate) struct Case {
        pub name: String,
        pub choice: Choice,
        pub build: Build,
        pub lang: Lang,
    }

    pub(crate) fn key(text: &str) -> KeySpec {
        KeySpec::parse(text).unwrap()
    }

    pub(crate) fn choice(preset: PresetId) -> Choice {
        Choice {
            preset,
            prefix: None,
            wm_mod: WmMod::Alt,
            tiling: Tiling::Unasked,
            wm_name: None,
            tmux_prefix: None,
            tmux_config_found: false,
            running_prefix: key("C-b"),
            nvim_lhs: None,
            bound_as: None,
            press_failed: false,
            date: "2026-11-02".to_string(),
        }
    }

    pub(crate) fn stock_table() -> Vec<(KeySpec, String)> {
        Keymap::defaults()
            .bindings()
            .iter()
            .map(|b| (b.key, b.action.name()))
            .collect()
    }

    /// The whole matrix: presets 1-5 by prefix, by `$mod` for preset 4, by language, by build and tiling; preset 5
    /// only in a build with compositor mode, preset 2 with and without a left-hand side, preset 1 with and without
    /// a tmux configuration.
    pub(crate) fn cases() -> Vec<Case> {
        const NO_COMPOSITOR: Build = Build {
            compositor_mode: false,
            nvim_prefix: true,
        };
        const COMPOSITOR: Build = Build {
            compositor_mode: true,
            nvim_prefix: true,
        };
        let presets = [
            PresetId::NvimTmux,
            PresetId::NvimNoTmux,
            PresetId::OtherPrefix,
            PresetId::I3Style,
            PresetId::Compositor,
        ];
        let prefixes = [None, Some("C-a"), Some("C-Space"), Some("M-\"")];
        let builds = [
            (NO_COMPOSITOR, Tiling::Eitri),
            (COMPOSITOR, Tiling::Eitri),
            (COMPOSITOR, Tiling::Compositor),
        ];
        let mut cases = Vec::new();
        for preset in presets {
            let mods: &[WmMod] = if preset == PresetId::I3Style {
                &[WmMod::Alt, WmMod::Super]
            } else {
                &[WmMod::Alt]
            };
            let lhs: &[Option<&'static str>] = if preset == PresetId::NvimNoTmux {
                &[None, Some(NVIM_LHS_CANDIDATES[0])]
            } else {
                &[None]
            };
            let tmux_found: &[bool] = if preset == PresetId::NvimTmux {
                &[false, true]
            } else {
                &[false]
            };
            for prefix in prefixes {
                for wm_mod in mods {
                    for nvim_lhs in lhs {
                        for found in tmux_found {
                            for lang in [Lang::En, Lang::ZhCn] {
                                for (build, tiling) in builds {
                                    if preset == PresetId::Compositor && !build.compositor_mode {
                                        continue;
                                    }
                                    let mut choice = choice(preset);
                                    choice.prefix = prefix.map(key);
                                    choice.wm_mod = *wm_mod;
                                    choice.tiling = tiling;
                                    choice.wm_name = build.compositor_mode.then_some("sway");
                                    choice.nvim_lhs = *nvim_lhs;
                                    choice.tmux_config_found = *found;
                                    if *found {
                                        choice.tmux_prefix = Some(key("C-a"));
                                        choice.running_prefix = key("C-a");
                                    }
                                    choice.bound_as = choice.prefix.and_then(|chosen| {
                                        bound_in(&start_table(preset, *wm_mod, &stock_table()), &chosen)
                                    });
                                    let name = format!(
                                        "{preset:?} prefix={} mod={wm_mod:?} lhs={} tmux={found} {lang:?} \
                                         compositor_mode={} tiling={tiling:?}",
                                        prefix.unwrap_or("none"),
                                        nvim_lhs.unwrap_or("none"),
                                        build.compositor_mode,
                                    );
                                    cases.push(Case {
                                        name,
                                        choice,
                                        build,
                                        lang,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
        cases
    }
}

#[cfg(test)]
mod tests {
    use super::test_matrix::{cases, choice, key, stock_table};
    use super::*;
    use crate::source_scan;

    const COMPOSITOR_BUILD: Build = Build {
        compositor_mode: true,
        nvim_prefix: true,
    };
    const WITHOUT_COMPOSITOR: Build = Build {
        compositor_mode: false,
        nvim_prefix: true,
    };

    /// The file's lines after the two header comments and the blank line.
    fn body(generated: &Generated) -> Vec<&str> {
        generated.lines[3..].iter().map(String::as_str).collect()
    }

    fn pairs(rows: &[(&str, &str)]) -> Vec<(String, String)> {
        rows.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// The code of a line, with no trailing comment; `None` for a blank line or a whole-line comment.
    fn code_of(line: &str) -> Option<&str> {
        if line.is_empty() || line.starts_with("--") {
            return None;
        }
        Some(line.split("  --").next().unwrap().trim_end())
    }

    /// The comment of a line: all of a whole-line comment, the trailing part of a code line.
    fn comment_of(line: &str) -> Option<&str> {
        if line.starts_with("--") {
            return Some(line);
        }
        line.split_once("  --").map(|(_, comment)| comment)
    }

    fn prefix_op(text: &str) -> KeymapOp {
        KeymapOp::Prefix { key: text.to_string() }
    }

    fn del_op(text: &str) -> KeymapOp {
        KeymapOp::Del {
            table: "prefix".to_string(),
            key: text.to_string(),
        }
    }

    fn send_prefix_op(text: &str) -> KeymapOp {
        KeymapOp::Set {
            table: "prefix".to_string(),
            key: text.to_string(),
            action: "send-prefix".to_string(),
            opts: Vec::new(),
        }
    }

    #[test]
    fn every_preset_choice_language_build_and_tiling_gives_the_golden_text() {
        let mut actual = String::new();
        for case in cases() {
            actual.push_str("=== ");
            actual.push_str(&case.name);
            actual.push('\n');
            actual.push_str(&generate(&case.choice, case.build, case.lang).text());
        }
        // A rewrite of the golden file is asked for by naming where the new text goes; it is then read against
        // the wording by hand and copied over.
        if let Some(path) = std::env::var_os("EITRI_ONBOARDING_GOLDEN_OUT") {
            std::fs::write(path, &actual).unwrap();
        }
        let golden = include_str!("golden/matrix.txt");
        let first_difference = actual
            .lines()
            .zip(golden.lines())
            .position(|(a, g)| a != g)
            .map_or_else(|| "the shorter one's end".to_string(), |at| format!("line {}", at + 1));
        assert!(
            actual == golden,
            "the generated text differs from golden/matrix.txt at {first_difference}"
        );
    }

    #[test]
    fn the_matrix_covers_what_it_says() {
        let all = cases();
        assert!(all.len() > 150, "{}", all.len());
        assert!(all
            .iter()
            .filter(|c| c.choice.preset == PresetId::Compositor)
            .all(|c| c.build.compositor_mode));
    }

    #[test]
    fn code_lines_are_identical_across_languages() {
        for case in cases() {
            let en = generate(&case.choice, case.build, Lang::En);
            let zh = generate(&case.choice, case.build, Lang::ZhCn);
            let code = |g: &Generated| -> Vec<String> {
                g.lines.iter().filter_map(|l| code_of(l)).map(str::to_string).collect()
            };
            assert_eq!(code(&en), code(&zh), "{}", case.name);
            assert_eq!(en.ops, zh.ops, "{}", case.name);
            assert_eq!(en.settings, zh.settings, "{}", case.name);
        }
    }

    #[test]
    fn no_comment_line_holds_a_control_character() {
        for case in cases() {
            for line in generate(&case.choice, case.build, case.lang).lines {
                assert!(!line.contains(char::is_control), "{}: {line:?}", case.name);
            }
        }
    }

    #[test]
    fn a_key_that_would_not_show_is_escaped_in_comments_and_in_code() {
        let hostile = KeySpec::char('\u{202e}', false, true);
        let mut c = choice(PresetId::OtherPrefix);
        c.prefix = Some(hostile);
        c.tmux_prefix = Some(hostile);
        c.bound_as = Some("was\u{1b}[31m".to_string());
        let generated = generate(&c, Build::THIS_BINARY, Lang::En);
        assert!(generated.text().contains("\\u{202e}"), "{}", generated.text());
        assert!(!generated.text().contains('\u{202e}'), "{}", generated.text());
        assert!(!generated.text().contains('\u{1b}'), "{}", generated.text());
        assert_eq!(
            generated.ops.first(),
            Some(&KeymapOp::Prefix {
                key: hostile.to_string()
            })
        );
    }

    #[test]
    fn every_line_is_blank_a_comment_or_code() {
        for case in cases() {
            for line in generate(&case.choice, case.build, case.lang).lines {
                let code = code_of(&line);
                assert!(
                    line.is_empty()
                        || line.starts_with("-- ")
                        || code.is_some_and(|code| {
                            (code.starts_with("eitri.keymap.") || code.starts_with("eitri.config.set("))
                                && code.ends_with(')')
                        }),
                    "{}: {line:?}",
                    case.name
                );
            }
        }
    }

    #[test]
    fn with_compositor_no_promise_names_a_key_that_mode_turns_off() {
        let windows_wm_en =
            "-- Windows: sway arranges chats and pages as its own windows; Eitri's layout keys are off.";
        let windows_wm_zh = "-- 窗口：sway 把对话和页面当作自己的窗口排布；Eitri 的布局键关闭。";
        let banned = ["layout", "float", "workspace", "window.new", "split", "布局", "浮动"];
        for preset in [
            PresetId::NvimTmux,
            PresetId::NvimNoTmux,
            PresetId::OtherPrefix,
            PresetId::I3Style,
            PresetId::Compositor,
        ] {
            for lang in [Lang::En, Lang::ZhCn] {
                let mut c = choice(preset);
                c.tiling = Tiling::Compositor;
                c.wm_name = Some("sway");
                c.nvim_lhs = Some(NVIM_LHS_CANDIDATES[0]);
                c.prefix = Some(key("C-a"));
                let told = summary(preset, &c, Tiling::Compositor, lang);
                assert!(!told.is_empty());
                let generated = generate(&c, COMPOSITOR_BUILD, lang);
                let comments = generated
                    .lines
                    .iter()
                    .filter(|l| l.as_str() != windows_wm_en && l.as_str() != windows_wm_zh)
                    .filter_map(|l| comment_of(l));
                for text in comments.chain(std::iter::once(told.as_str())) {
                    for word in banned {
                        assert!(!text.contains(word), "{preset:?}: {word} in {text:?}");
                    }
                }
            }
        }
        let mut c = choice(PresetId::NvimNoTmux);
        c.tiling = Tiling::Compositor;
        c.wm_name = Some("sway");
        let text = generate(&c, COMPOSITOR_BUILD, Lang::En).text();
        assert!(
            text.contains("-- After the prefix a panel lists the keys after the prefix that still work here."),
            "{text}"
        );
        assert!(!text.contains("set it to \"off\" once you know them"), "{text}");
    }

    #[test]
    fn preset_four_on_a_compositor_environment_still_writes_eitri() {
        for tiling in [Tiling::Unasked, Tiling::Eitri, Tiling::Compositor] {
            let mut c = choice(PresetId::I3Style);
            c.tiling = tiling;
            c.wm_name = Some("sway");
            let generated = generate(&c, COMPOSITOR_BUILD, Lang::En);
            assert_eq!(generated.settings, pairs(&[("window.tiling", "eitri")]), "{tiling:?}");
        }
    }

    #[test]
    fn a_chosen_prefix_of_ctrl_b_keeps_its_three_lines() {
        let mut c = choice(PresetId::OtherPrefix);
        c.prefix = Some(key("C-b"));
        c.tmux_prefix = Some(key("C-a"));
        c.running_prefix = key("C-a");
        let generated = generate(&c, Build::THIS_BINARY, Lang::En);
        assert_eq!(
            body(&generated),
            [
                "-- Keys: tmux-style with Ctrl+b as the prefix; Ctrl+b twice sends Ctrl+b itself.",
                "-- Your tmux prefix (Ctrl+a) is not used in Eitri; this line wins.",
                r#"eitri.keymap.prefix("C-b")"#,
                r#"eitri.keymap.del("prefix", "C-b")"#,
                r#"eitri.keymap.set("prefix", "C-b", "send-prefix")"#,
            ]
        );
        assert_eq!(generated.ops, [prefix_op("C-b"), del_op("C-b"), send_prefix_op("C-b")]);
    }

    #[test]
    fn a_chosen_ctrl_b_that_is_bound_is_freed_once() {
        let mut c = choice(PresetId::OtherPrefix);
        c.prefix = Some(key("C-b"));
        c.bound_as = Some("send-prefix".to_string());
        let generated = generate(&c, Build::THIS_BINARY, Lang::En);
        assert_eq!(generated.ops, [prefix_op("C-b"), del_op("C-b"), send_prefix_op("C-b")]);
        assert!(generated.text().contains("-- was send-prefix"), "{}", generated.text());
    }

    #[test]
    fn a_key_already_bound_is_freed_with_its_old_name_in_a_comment() {
        let mut c = choice(PresetId::OtherPrefix);
        c.prefix = Some(key("C-o"));
        c.bound_as = Some("rotate.up".to_string());
        let generated = generate(&c, Build::THIS_BINARY, Lang::En);
        let lines = body(&generated);
        let del = lines
            .iter()
            .position(|l| l.starts_with(r#"eitri.keymap.del("prefix", "C-o")"#))
            .unwrap();
        let set = lines
            .iter()
            .position(|l| l.starts_with(r#"eitri.keymap.set("prefix", "C-o""#))
            .unwrap();
        assert!(del < set, "{lines:?}");
        assert!(lines[del].ends_with("-- was rotate.up"), "{lines:?}");
        assert_eq!(
            generated.ops,
            [prefix_op("C-o"), del_op("C-b"), del_op("C-o"), send_prefix_op("C-o")]
        );
    }

    #[test]
    fn bound_as_reads_the_table_the_file_starts_from() {
        let mut running = stock_table();
        running.retain(|(k, _)| *k != key("C-o"));
        running.push((key("C-g"), "module.editor".to_string()));

        let no_tmux = start_table(PresetId::NvimNoTmux, WmMod::Alt, &running);
        assert_eq!(bound_in(&no_tmux, &key("C-o")).as_deref(), Some("rotate.up"));
        assert_eq!(bound_in(&no_tmux, &key("C-g")), None);

        for preset in [PresetId::NvimTmux, PresetId::OtherPrefix, PresetId::Compositor] {
            let table = start_table(preset, WmMod::Alt, &running);
            assert_eq!(bound_in(&table, &key("C-o")), None, "{preset:?}");
            assert_eq!(
                bound_in(&table, &key("C-g")).as_deref(),
                Some("module.editor"),
                "{preset:?}"
            );
        }

        let i3 = start_table(PresetId::I3Style, WmMod::Alt, &running);
        assert_eq!(bound_in(&i3, &key("M-w")), None);
        assert_eq!(bound_in(&i3, &key("C-M-w")).as_deref(), Some("container.tabbed"));
        assert_eq!(bound_in(&i3, &key("C-g")).as_deref(), Some("module.editor"));
        let moved_once = i3.iter().filter(|(_, name)| name == "container.tabbed").count();
        assert_eq!(moved_once, 1, "only under its new key");

        let super_mod = start_table(PresetId::I3Style, WmMod::Super, &running);
        assert_eq!(super_mod, running);
    }

    #[test]
    fn every_bundle_row_keeps_the_stock_tables_name() {
        let stock = stock_table();
        let moved = start_table(PresetId::I3Style, WmMod::Alt, &stock);
        for (spec, name) in &moved {
            assert!(
                stock.iter().any(|(_, stock_name)| stock_name == name),
                "{spec} has {name}, which the stock table does not spell that way"
            );
        }
    }

    fn free(mapped: &[&str], leader: &str) -> Option<&'static str> {
        let mapped: Vec<String> = mapped.iter().map(|s| s.to_string()).collect();
        free_nvim_lhs(&mapped, leader)
    }

    #[test]
    fn free_nvim_lhs_skips_an_exact_mapping() {
        assert_eq!(free(&[], "<Space>"), Some("<leader>W"));
        assert_eq!(free(&["<Space>W"], "<Space>"), Some("<leader>E"));
        assert_eq!(free(&["<Space>W", "<Space>E"], "<Space>"), Some("<leader>z"));
    }

    #[test]
    fn free_nvim_lhs_skips_a_mapping_under_a_candidate() {
        assert_eq!(free(&["<Space>Wq"], "<Space>"), Some("<leader>E"));
        assert_eq!(free(&["<Space>W<C-x>"], "<Space>"), Some("<leader>E"));
    }

    #[test]
    fn free_nvim_lhs_reads_the_leader_it_is_given() {
        assert_eq!(free(&["\\W"], "\\"), Some("<leader>E"));
        assert_eq!(free(&["<Space>W"], "\\"), Some("<leader>W"));
        assert_eq!(free(&["\\W"], "<Space>"), Some("<leader>W"));
    }

    #[test]
    fn free_nvim_lhs_keeps_case() {
        assert_eq!(free(&["<Space>w", "<Space>Z"], "<Space>"), Some("<leader>W"));
    }

    #[test]
    fn free_nvim_lhs_gives_none_when_all_three_are_taken() {
        assert_eq!(free(&["<Space>W", "<Space>Ex", "<Space>z"], "<Space>"), None);
    }

    #[test]
    fn preset_one_says_what_tmux_gives() {
        let mut c = choice(PresetId::NvimTmux);
        c.tmux_config_found = true;
        c.tmux_prefix = Some(key("C-a"));
        c.running_prefix = key("C-a");
        let generated = generate(&c, Build::THIS_BINARY, Lang::En);
        assert_eq!(
            generated.lines,
            [
                "-- Eitri configuration, written by Eitri's first-start screen on 2026-11-02 (preset: Neovim + tmux).",
                "-- Eitri reads it at startup and never changes it. Every setting is explained in the guide, \
                 \"Configuration\".",
                "",
                "-- Keys: tmux's. The prefix and the keys after it come from your tmux configuration (now Ctrl+a).",
                r#"eitri.config.set("keymap.from_tmux", "on")"#,
                r#"-- A panel listing the keys after the prefix (milliseconds before it shows, or "off"):"#,
                r#"-- eitri.config.set("keymap.which_key_delay", 200)"#,
            ]
        );
        assert!(generated.ops.is_empty());
        assert_eq!(generated.settings, pairs(&[("keymap.from_tmux", "on")]));
    }

    #[test]
    fn preset_one_without_a_tmux_configuration_says_so() {
        let generated = generate(&choice(PresetId::NvimTmux), Build::THIS_BINARY, Lang::En);
        assert_eq!(
            body(&generated)[0],
            "-- Keys: tmux's. The prefix and the keys after it come from your tmux configuration (none found, so \
             Ctrl+b)."
        );
    }

    #[test]
    fn preset_two_turns_the_import_off_and_the_panel_on() {
        let mut c = choice(PresetId::NvimNoTmux);
        c.tmux_prefix = Some(key("C-a"));
        c.running_prefix = key("C-a");
        c.nvim_lhs = Some("<leader>W");
        let generated = generate(&c, Build::THIS_BINARY, Lang::En);
        assert_eq!(
            body(&generated),
            [
                "-- Keys: tmux-style, prefix Ctrl+b, whatever a tmux configuration on this computer says.",
                r#"eitri.config.set("keymap.from_tmux", "off")"#,
                r#"-- After the prefix a panel lists the next keys; set it to "off" once you know them."#,
                r#"eitri.config.set("keymap.which_key_delay", 200)"#,
                "-- The same window keys in Neovim under <leader>W, which which-key lists (:Eitri runs them too):",
                r#"eitri.config.set("keymap.nvim_prefix", "<leader>W")"#,
            ]
        );
        assert_eq!(
            generated.settings,
            pairs(&[
                ("keymap.from_tmux", "off"),
                ("keymap.which_key_delay", "200"),
                ("keymap.nvim_prefix", "<leader>W"),
            ])
        );
    }

    #[test]
    fn preset_two_without_a_report_writes_the_leader_line_commented_out() {
        let generated = generate(&choice(PresetId::NvimNoTmux), Build::THIS_BINARY, Lang::En);
        assert_eq!(
            body(&generated)[4..],
            [
                "-- No report from Neovim yet; uncomment to get the window keys under <leader>W:",
                r#"-- eitri.config.set("keymap.nvim_prefix", "<leader>W")"#,
            ]
        );
        assert_eq!(
            generated.settings,
            pairs(&[("keymap.from_tmux", "off"), ("keymap.which_key_delay", "200")])
        );
    }

    #[test]
    fn a_build_without_the_leader_group_writes_no_line_for_it() {
        let build = Build {
            compositor_mode: false,
            nvim_prefix: false,
        };
        let mut c = choice(PresetId::NvimNoTmux);
        c.nvim_lhs = Some("<leader>W");
        let text = generate(&c, build, Lang::En).text();
        assert!(!text.contains("nvim_prefix"), "{text}");
        assert!(!text.contains("<leader>"), "{text}");
    }

    #[test]
    fn preset_three_writes_the_three_lines() {
        let mut c = choice(PresetId::OtherPrefix);
        c.prefix = Some(key("C-Space"));
        let generated = generate(&c, Build::THIS_BINARY, Lang::En);
        assert_eq!(
            body(&generated),
            [
                "-- Keys: tmux-style with Ctrl+Space as the prefix; Ctrl+Space twice sends Ctrl+Space itself.",
                r#"eitri.keymap.prefix("C-Space")"#,
                r#"eitri.keymap.del("prefix", "C-b")                     -- Ctrl+b goes back to whatever has the keys"#,
                r#"eitri.keymap.set("prefix", "C-Space", "send-prefix")"#,
            ]
        );
        assert_eq!(
            generated.ops,
            [prefix_op("C-Space"), del_op("C-b"), send_prefix_op("C-Space")]
        );
        assert!(generated.settings.is_empty());
    }

    #[test]
    fn a_tmux_prefix_the_file_overrides_is_named_above_the_line_that_wins() {
        let mut c = choice(PresetId::OtherPrefix);
        c.prefix = Some(key("C-Space"));
        c.tmux_prefix = Some(key("C-a"));
        let generated = generate(&c, Build::THIS_BINARY, Lang::En);
        let lines = body(&generated);
        let wins = lines.iter().position(|l| l.ends_with("this line wins.")).unwrap();
        assert_eq!(lines[wins + 1], r#"eitri.keymap.prefix("C-Space")"#);
        assert_eq!(
            lines[wins],
            "-- Your tmux prefix (Ctrl+a) is not used in Eitri; this line wins."
        );

        c.tmux_prefix = Some(key("C-Space"));
        let text = generate(&c, Build::THIS_BINARY, Lang::En).text();
        assert!(!text.contains("this line wins"), "{text}");
    }

    #[test]
    fn a_press_test_that_saw_nothing_is_said_above_the_prefix_line() {
        let mut c = choice(PresetId::OtherPrefix);
        c.prefix = Some(key("C-Space"));
        c.press_failed = true;
        let generated = generate(&c, Build::THIS_BINARY, Lang::En);
        let lines = body(&generated);
        assert_eq!(
            lines[1],
            "-- The press test did not see this key reach Eitri; it was written anyway."
        );
        assert_eq!(lines[2], r#"eitri.keymap.prefix("C-Space")"#);
    }

    #[test]
    fn a_prefix_needing_quoting_goes_through_the_quoting() {
        let mut c = choice(PresetId::OtherPrefix);
        c.prefix = Some(key("M-\""));
        let lines = generate(&c, Build::THIS_BINARY, Lang::En).lines;
        assert!(lines.iter().any(|l| l == r#"eitri.keymap.prefix("M-\"")"#), "{lines:?}");
        c.prefix = Some(key("M-\\"));
        let lines = generate(&c, Build::THIS_BINARY, Lang::En).lines;
        assert!(lines.iter().any(|l| l == r#"eitri.keymap.prefix("M-\\")"#), "{lines:?}");
    }

    #[test]
    fn preset_four_on_alt_uses_the_bundle_and_names_the_sample() {
        let mut c = choice(PresetId::I3Style);
        c.wm_name = Some("sway");
        let generated = generate(&c, COMPOSITOR_BUILD, Lang::En);
        assert_eq!(
            body(&generated),
            [
                "-- Keys: tmux-style, prefix Ctrl+b. Your window manager's $mod is Alt, so the Alt keys after the",
                "-- prefix move to Ctrl+Alt (the same moves as docs/keymap/i3-alt-mod.lua):",
                r#"eitri.keymap.use("i3-alt")"#,
                r#"eitri.config.set("window.tiling", "eitri")            -- Eitri arranges its own modules"#,
            ]
        );
        assert_eq!(generated.ops, bundle("i3-alt").unwrap());
    }

    #[test]
    fn preset_four_on_super_moves_nothing() {
        let mut c = choice(PresetId::I3Style);
        c.wm_mod = WmMod::Super;
        let generated = generate(&c, WITHOUT_COMPOSITOR, Lang::En);
        assert_eq!(
            body(&generated),
            ["-- $mod is Super: nothing after the prefix collides with it."]
        );
        assert!(generated.ops.is_empty());
        assert!(generated.settings.is_empty());
    }

    #[test]
    fn preset_four_names_the_prefix_the_file_will_have() {
        let mut c = choice(PresetId::I3Style);
        c.tmux_prefix = Some(key("C-a"));
        let text = generate(&c, Build::THIS_BINARY, Lang::En).text();
        assert!(text.contains("prefix Ctrl+a."), "{text}");
        c.prefix = Some(key("C-Space"));
        let text = generate(&c, Build::THIS_BINARY, Lang::En).text();
        assert!(text.contains("prefix Ctrl+Space."), "{text}");
    }

    #[test]
    fn a_build_without_compositor_mode_writes_no_tiling_line() {
        for preset in [
            PresetId::NvimTmux,
            PresetId::NvimNoTmux,
            PresetId::OtherPrefix,
            PresetId::I3Style,
            PresetId::Compositor,
        ] {
            for tiling in [Tiling::Eitri, Tiling::Compositor] {
                let mut c = choice(preset);
                c.tiling = tiling;
                c.wm_name = Some("sway");
                let text = generate(&c, WITHOUT_COMPOSITOR, Lang::En).text();
                assert!(!text.contains("window.tiling\""), "{text}");
            }
        }
    }

    #[test]
    fn preset_five_leaves_the_windows_to_the_compositor() {
        let mut c = choice(PresetId::Compositor);
        c.tiling = Tiling::Compositor;
        c.wm_name = Some("sway");
        let generated = generate(&c, COMPOSITOR_BUILD, Lang::En);
        assert_eq!(
            body(&generated),
            [
                "-- Windows: sway arranges chats and pages as its own windows; Eitri's layout keys are off.",
                r#"eitri.config.set("window.tiling", "compositor")"#,
            ]
        );
        assert_eq!(generated.settings, pairs(&[("window.tiling", "compositor")]));
    }

    #[test]
    fn the_tiling_choice_ends_the_file_on_presets_one_to_three() {
        for preset in [PresetId::NvimTmux, PresetId::NvimNoTmux, PresetId::OtherPrefix] {
            let mut c = choice(preset);
            c.prefix = Some(key("C-a"));
            c.wm_name = Some("sway");
            c.tiling = Tiling::Eitri;
            let generated = generate(&c, COMPOSITOR_BUILD, Lang::En);
            let lines = body(&generated);
            assert_eq!(
                lines[lines.len() - 2..],
                [
                    r#"-- Windows: Eitri arranges its own modules (window.tiling "auto" would have picked sway here)."#,
                    r#"eitri.config.set("window.tiling", "eitri")"#,
                ],
                "{preset:?}"
            );
            c.tiling = Tiling::Compositor;
            let generated = generate(&c, COMPOSITOR_BUILD, Lang::En);
            let lines = body(&generated);
            assert_eq!(
                lines[lines.len() - 2..],
                [
                    "-- Windows: sway arranges chats and pages as its own windows; Eitri's layout keys are off.",
                    r#"eitri.config.set("window.tiling", "compositor")"#,
                ],
                "{preset:?}"
            );
        }
    }

    #[test]
    fn off_a_live_compositor_no_tiling_line_is_written() {
        for tiling in [Tiling::Unasked, Tiling::Eitri, Tiling::Compositor] {
            let mut c = choice(PresetId::NvimTmux);
            c.tiling = tiling;
            let text = generate(&c, COMPOSITOR_BUILD, Lang::En).text();
            assert!(!text.contains("window.tiling\""), "{tiling:?}: {text}");
        }
        let mut c = choice(PresetId::NvimTmux);
        c.wm_name = Some("sway");
        let text = generate(&c, COMPOSITOR_BUILD, Lang::En).text();
        assert!(!text.contains("window.tiling\""), "{text}");
    }

    #[test]
    fn a_changed_prefix_appends_the_three_lines_on_any_preset() {
        for preset in [PresetId::NvimTmux, PresetId::NvimNoTmux, PresetId::I3Style] {
            let mut c = choice(preset);
            c.prefix = Some(key("C-a"));
            let generated = generate(&c, Build::THIS_BINARY, Lang::En);
            assert_eq!(
                generated.ops[generated.ops.len() - 3..],
                [prefix_op("C-a"), del_op("C-b"), send_prefix_op("C-a")],
                "{preset:?}"
            );
        }
        let mut c = choice(PresetId::Compositor);
        c.prefix = Some(key("C-a"));
        c.wm_name = Some("sway");
        let generated = generate(&c, COMPOSITOR_BUILD, Lang::En);
        assert_eq!(generated.ops, [prefix_op("C-a"), del_op("C-b"), send_prefix_op("C-a")]);
    }

    #[test]
    fn the_comments_follow_the_language_and_the_header_names_the_preset() {
        let generated = generate(&choice(PresetId::NvimNoTmux), Build::THIS_BINARY, Lang::ZhCn);
        assert_eq!(
            generated.lines[0],
            "-- Eitri 配置，由 Eitri 首次启动界面于 2026-11-02 写入（预设：Neovim，不用 tmux）。"
        );
    }

    #[test]
    fn the_list_holds_what_the_build_can_do() {
        let four = vec![
            PresetId::NvimTmux,
            PresetId::NvimNoTmux,
            PresetId::OtherPrefix,
            PresetId::I3Style,
        ];
        assert_eq!(offered(WITHOUT_COMPOSITOR, false), four);
        assert_eq!(offered(WITHOUT_COMPOSITOR, true), four);
        assert_eq!(offered(COMPOSITOR_BUILD, false), four);
        let mut five = four;
        five.push(PresetId::Compositor);
        assert_eq!(offered(COMPOSITOR_BUILD, true), five);
    }

    #[test]
    fn titles_and_summaries_come_from_the_wording() {
        assert_eq!(title(PresetId::NvimTmux, Lang::En), "Neovim + tmux");
        assert_eq!(title(PresetId::NvimNoTmux, Lang::ZhCn), "Neovim，不用 tmux");
        let mut c = choice(PresetId::NvimTmux);
        c.tmux_prefix = Some(key("C-a"));
        assert_eq!(
            summary(PresetId::NvimTmux, &c, Tiling::Unasked, Lang::En),
            "Ctrl+a, then a key, as in tmux"
        );
        // The list shows every preset's line against one choice, whichever preset that choice holds.
        c.preset = PresetId::OtherPrefix;
        assert_eq!(
            summary(PresetId::NvimTmux, &c, Tiling::Unasked, Lang::En),
            "Ctrl+a, then a key, as in tmux"
        );
        c.wm_name = Some("Hyprland");
        assert_eq!(
            summary(PresetId::Compositor, &c, Tiling::Compositor, Lang::En),
            "Hyprland arranges chats and pages as its own windows"
        );
    }

    /// Each `format!` in `text` that puts a quote right before a field, or whose string a scan cannot read.
    fn quoted_fields(text: &str) -> Vec<String> {
        let masked = source_scan::code_only(text);
        let masked = source_scan::without(&masked, &source_scan::modules(&masked, "tests"));
        let mut hits = Vec::new();
        for end in source_scan::word_ends(&masked, "format") {
            if !masked[end..].starts_with("!(") {
                continue;
            }
            let open = end + 1;
            let mut depth = 0usize;
            let close = masked[open..]
                .char_indices()
                .find_map(|(at, c)| {
                    match c {
                        '(' => depth += 1,
                        ')' => depth -= 1,
                        _ => {}
                    }
                    (depth == 0).then_some(open + at)
                })
                .expect("a format! closes");
            // The mask keeps byte offsets, so the literals of the call are read from the source itself.
            let call = &text[open..=close];
            if call.contains("\\\"{") || call.contains("r\"") || call.contains("r#") {
                hits.push(call.to_string());
            }
        }
        hits
    }

    #[test]
    fn no_scan_of_format_puts_quotes_around_an_interpolated_value() {
        let sources = source_scan::rust_sources(&["core/src/onboarding"]);
        source_scan::require_anchors(&sources, &["format!", "quote(", "fn generate"]);
        for source in &sources {
            assert_eq!(quoted_fields(&source.text), Vec::<String>::new(), "{}", source.path);
        }
    }

    #[test]
    fn the_quoting_scan_finds_what_it_looks_for() {
        let bad = "fn a() { let x = format!(\n    \"set(\\\"{}\\\")\",\n    y\n); }\n";
        assert_eq!(quoted_fields(bad).len(), 1, "across lines");
        let raw = "fn a() { let x = format!(r#\"set(\"{y}\")\"#); }\n";
        assert_eq!(quoted_fields(raw).len(), 1, "a raw string");
        let fine = "fn a() { let x = format!(\"set({})\", quote(y)); }\n";
        assert!(quoted_fields(fine).is_empty());
        let test_only = "mod tests { fn a() { format!(\"\\\"{y}\\\"\"); } }\n";
        assert!(quoted_fields(test_only).is_empty());
    }
}
