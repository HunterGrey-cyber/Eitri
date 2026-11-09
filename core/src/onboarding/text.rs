//! The screen's wording, in both languages, as templates with named fields. The file's comments are here
//! too: they follow the screen's language, and only constants and canonical key and action names fill them.

use super::Lang;

/// Declares the wording: each row is the id, its stable name, the English and the Simplified Chinese.
macro_rules! wording {
    ($($id:ident $name:literal $en:literal $zh:literal)*) => {
        /// One string of the screen or of the generated file's comments.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum TextId {
            $($id,)*
        }

        impl TextId {
            /// Every id, in table order.
            pub const ALL: &'static [TextId] = &[$(TextId::$id,)*];

            /// The id's stable name.
            pub fn name(self) -> &'static str {
                match self {
                    $(TextId::$id => $name,)*
                }
            }
        }

        fn english(id: TextId) -> &'static str {
            match id {
                $(TextId::$id => $en,)*
            }
        }

        fn chinese(id: TextId) -> &'static str {
            match id {
                $(TextId::$id => $zh,)*
            }
        }
    };
}

wording! {
    Preset1 "preset.1"
        "Neovim + tmux"
        "Neovim + tmux"
    Preset1Summary "preset.1.summary"
        "{prefix}, then a key, as in tmux"
        "先按 {prefix} 再按一个键，和 tmux 一样"
    Preset2 "preset.2"
        "Neovim, no tmux"
        "Neovim，不用 tmux"
    Preset2Summary "preset.2.summary"
        "the same keys, with a panel listing them"
        "同样的键，有面板列出它们"
    Preset3 "preset.3"
        "Another prefix"
        "换个前缀"
    Preset3Summary "preset.3.summary"
        "Ctrl+a, Ctrl+Space, or a key you press"
        "Ctrl+a、Ctrl+Space 或你按的键"
    Preset4 "preset.4"
        "i3 / sway style"
        "i3 / sway 风格"
    Preset4Summary "preset.4.summary"
        "Eitri arranges; Alt keys move where $mod is Alt"
        "Eitri 排布；$mod 是 Alt 时让开 Alt"
    Preset5 "preset.5"
        "Tiling compositor"
        "平铺合成器"
    Preset5Summary "preset.5.summary"
        "{wm} arranges chats and pages as its own windows"
        "{wm} 把对话和页面当作自己的窗口排布"
    Title "title"
        "Set up Eitri's window keys"
        "设置 Eitri 的窗口按键"
    FoundHeading "found.heading"
        "Found on this computer"
        "在这台电脑上找到"
    RowTmux "row.tmux"
        "tmux"
        "tmux"
    RowDesktop "row.desktop"
        "desktop"
        "桌面"
    RowWindows "row.windows"
        "windows"
        "窗口"
    RowNeovim "row.neovim"
        "Neovim"
        "Neovim"
    RowInput "row.input"
        "input"
        "输入法"
    RowClaude "row.claude"
        "claude"
        "claude"
    ClaudeMissing "claude.missing"
        "Claude Code was not found in {looked}: {url}"
        "在 {looked} 中未找到 Claude Code：{url}"
    ClaudeRefused "claude.refused"
        "Claude Code was found but not used (another user could change it, or what it starts is not on PATH; the log says which): {url}"
        "找到了 Claude Code 但没有使用（其他用户能改动它，或它要启动的程序不在 PATH 上；日志里写明了原因）：{url}"
    RowLooking "row.looking"
        "looking…"
        "查找中…"
    RowNotFound "row.not_found"
        "not found"
        "未找到"
    RowReadingNvim "row.reading_nvim"
        "reading Neovim's mappings…"
        "正在读取 Neovim 映射…"
    TmuxFound "tmux.found"
        "{path}: prefix {key}"
        "{path}：前缀 {key}"
    TmuxWillUse "tmux.will_use"
        "(will be used)"
        "（将使用）"
    DesktopRunning "desktop.running"
        "{wm}, running"
        "{wm}，运行中"
    DesktopNamed "desktop.named"
        "{wm}"
        "{wm}"
    DesktopModSuper "desktop.mod_super"
        "$mod is Super (Mod4)"
        "$mod 是 Super（Mod4）"
    DesktopModAlt "desktop.mod_alt"
        "$mod is Alt (Mod1)"
        "$mod 是 Alt（Mod1）"
    DesktopHyprLua "desktop.hypr_lua"
        "Hyprland (Lua config): keys not read"
        "Hyprland（Lua 配置）：未读取按键"
    WindowsAuto "windows.auto"
        "window.tiling \"auto\" picks {wm}'s own windows here"
        "这里 window.tiling \"auto\" 会用 {wm} 的窗口"
    WindowsHint "windows.hint"
        "(w on the next step)"
        "（下一步按 w）"
    NvimNavOk "nvim.nav_ok"
        "Ctrl+h/j/k/l: {who} (works as is)"
        "Ctrl+h/j/k/l：{who}（可直接用）"
    NvimNavEdge "nvim.nav_edge"
        "Ctrl+h/j/k/l: {who} (set its at_edge hook, guide \"Moving between panes\")"
        "Ctrl+h/j/k/l：{who}（需设 at_edge，见指南“在面板间移动”）"
    NvimNavYours "nvim.nav_yours"
        "Ctrl+h/j/k/l: yours ({who}); Eitri's moves: {key} arrows"
        "Ctrl+h/j/k/l：你的映射（{who}）；Eitri 移动：{key} 方向键"
    NvimChordCertain "nvim.chord_certain"
        "{key} in {mode}: {who} (press {key} twice there)"
        "{mode}模式 {key}：{who}（在那里按两次 {key}）"
    NvimChordProbably "nvim.chord_probably"
        "{key} in {mode}: probably {who} (press {key} twice there)"
        "{mode}模式 {key}：可能是 {who}（在那里按两次 {key}）"
    NvimLeader "nvim.leader"
        "leader: {leader}"
        "leader：{leader}"
    InputRunning "input.running"
        "{im} is running: Ctrl+Space may switch input methods"
        "{im} 正在运行：Ctrl+Space 可能会切换输入法"
    InputMaybe "input.maybe"
        "an input method may be running: Ctrl+Space may switch input methods"
        "可能有输入法在运行：Ctrl+Space 可能会切换输入法"
    Question "question"
        "How should the window's keys work?"
        "窗口按键要怎么用？"
    TagFound "tag.found"
        "found"
        "已找到"
    FootChoose "foot.choose"
        "j/k or 1-{n} choose · Enter next · Esc not now · d don't ask again · r read Neovim again · ? help"
        "j/k 或 1-{n} 选择 · Enter 下一步 · Esc 以后再说 · d 不再询问 · r 重读 Neovim · ? 帮助"
    FootChooseShow "foot.choose_show"
        "j/k or 1-{n} choose · Enter next · Esc close · r read Neovim again · ? help"
        "j/k 或 1-{n} 选择 · Enter 下一步 · Esc 关闭 · r 重读 Neovim · ? 帮助"
    SubPrefix "sub.prefix"
        "Which prefix?"
        "用哪个前缀？"
    SubPrefixA "sub.prefix_a"
        "a  Ctrl+a"
        "a  Ctrl+a"
    SubPrefixS "sub.prefix_s"
        "s  Ctrl+Space"
        "s  Ctrl+Space"
    SubPrefixK "sub.prefix_k"
        "k  a key you press"
        "k  你按下的键"
    SubPressKey "sub.press_key"
        "Press the key you want as the prefix"
        "按下想用作前缀的键"
    SubMod "sub.mod"
        "Which key is your window manager's $mod?"
        "你的窗口管理器 $mod 是哪个键？"
    SubModA "sub.mod_a"
        "a  Alt"
        "a  Alt"
    SubModS "sub.mod_s"
        "s  Super"
        "s  Super"
    SubDetected "sub.detected"
        "(found)"
        "(已找到)"
    ReviewWrites "review.writes"
        "Writes {path}:"
        "将写入 {path}："
    ReviewPrefix "review.prefix"
        "Prefix: {key}   p change"
        "前缀：{key}   p 修改"
    ReviewWindowsEitri "review.windows_eitri"
        "Windows: Eitri arranges   w change"
        "窗口：Eitri 排布   w 修改"
    ReviewWindowsWm "review.windows_wm"
        "Windows: {wm} arranges   w change"
        "窗口：{wm} 排布   w 修改"
    ReviewWmWould "review.wm_would"
        "{wm} would otherwise arrange them"
        "否则会由 {wm} 排布"
    ReviewTmuxWins "review.tmux_wins"
        "Your tmux prefix ({key}) is not used in Eitri; this line wins."
        "你的 tmux 前缀（{key}）在 Eitri 里不用；以这一行为准。"
    FootReview "foot.review"
        "Enter write · Backspace back · Esc not now · j/k scroll · ? help"
        "Enter 写入 · Backspace 返回 · Esc 以后再说 · j/k 滚动 · ? 帮助"
    FootReviewPress "foot.review_press"
        "Enter press test · Backspace back · Esc not now · j/k scroll · ? help"
        "Enter 按键测试 · Backspace 返回 · Esc 以后再说 · j/k 滚动 · ? 帮助"
    FootPress "foot.press"
        "Esc back"
        "Esc 返回"
    PressAsk "press.ask"
        "Press {key} once"
        "按一下 {key}"
    PressArrived "press.arrived"
        "✓ {key} reaches Eitri."
        "✓ {key} 能到达 Eitri。"
    PressOther "press.other"
        "That was {got} (not {key}). Press {key}, or choose another prefix."
        "这是 {got}（不是 {key}）。请按 {key}，或换一个前缀。"
    PressNever "press.never"
        "{key} never reached Eitri: an input method or your desktop probably takes it. Choose another prefix ({running} keeps today's), or w write it anyway."
        "{key} 没有到达 Eitri：可能被输入法或桌面占用。请换一个前缀（{running} 保持现状），或按 w 仍然写入。"
    PressWriteAnyway "press.write_anyway"
        "w write it anyway"
        "按 w 仍然写入"
    PressImWarning "press.im_warning"
        "An input method seems to be running. If it switches with Ctrl+Space, that key either never reaches Eitri (the test below shows it) or stops switching input methods inside Eitri."
        "似乎有输入法在运行。如果它用 Ctrl+Space 切换，这个键要么到不了 Eitri（下面的测试会显示），要么在 Eitri 里不再切换输入法。"
    PressBindWarning "press.bind_warning"
        "{key} is also a binding of {wm}."
        "{key} 也是 {wm} 的快捷键。"
    RefuseKey "refuse.key"
        "{key} cannot be the prefix: {why}"
        "{key} 不能作前缀：{why}"
    RefuseUnnamedKey "refuse.unnamed_key"
        "a key tmux has no name for"
        "一个 tmux 没有名字的键"
    RefuseUnnamedWhy "refuse.unnamed_why"
        "the prefix must be a key tmux can name, held with Ctrl or Alt, or a function key"
        "前缀必须是 tmux 能命名的键，并按住 Ctrl 或 Alt，或是功能键"
    DoneWritten "done.written"
        "Written {path}. Your keys change the next time Eitri starts; this window keeps {key}."
        "已写入 {path}。按键在下次启动 Eitri 时生效；这个窗口继续用 {key}。"
    DoneTutorStock "done.tutor_stock"
        "The tutor teaches the stock keys after the prefix; your tmux configuration changes some of them ({key} ? lists yours)."
        "教程讲的是前缀后的默认键；你的 tmux 配置改了其中一些（{key} ? 列出你的键）。"
    DoneWindowsNext "done.windows_next"
        "The window.tiling line applies at the next start too; this run keeps the windows it opened with."
        "window.tiling 这一行同样在下次启动时生效；这次运行保持打开时的窗口。"
    DoneTutorWindows "done.tutor_windows"
        "The tutor teaches the keys of Eitri's own layout: where {wm} arranges the windows, its splits, zoom, floats and workspaces are off."
        "教程讲的是 Eitri 自身布局的按键：由 {wm} 排布窗口时，其中的分屏、放大、浮窗和工作区都不起作用。"
    FootDone "foot.done"
        "t  open :EitriTutor now · Enter close"
        "t  现在打开 :EitriTutor · Enter 关闭"
    FailAppeared "fail.appeared"
        "an init.lua appeared meanwhile (another window?); nothing was written"
        "期间出现了一个 init.lua（另一个窗口？）；未写入任何内容"
    FailDir "fail.dir"
        "{path} is not a directory only you can write; nothing was written"
        "{path} 不是只有你能写的目录；未写入任何内容"
    FailIo "fail.io"
        "could not write {path}: {why}; nothing was written"
        "无法写入 {path}：{why}；未写入任何内容"
    NotwrittenFs "notwritten.fs"
        "this file system cannot create init.lua safely"
        "这个文件系统无法安全地创建 init.lua"
    NotwrittenCheck "notwritten.check"
        "Eitri could not check these lines; nothing was written"
        "Eitri 无法检查这些行；未写入任何内容"
    ShowExists "show.exists"
        "You already have an init.lua; these lines are not written"
        "你已有 init.lua；这些行不会写入"
    FootShow "foot.show"
        "o  open them in a scratch buffer · Esc close · j/k scroll · ? help"
        "o  在临时缓冲区打开 · Esc 关闭 · j/k 滚动 · ? 帮助"
    FootFailed "foot.failed"
        "Backspace back · Esc close"
        "Backspace 返回 · Esc 关闭"
    HelpLine "help.line"
        "Keys here are read before the editor. Nothing is written until the last step. PageUp and PageDown scroll; on the first page r reads Neovim's mappings again."
        "这里的按键先于编辑器读取。最后一步之前不会写入任何内容。PageUp、PageDown 滚动；第一页按 r 重新读取 Neovim 映射。"
    CHeader1 "c.header1"
        "Eitri configuration, written by Eitri's first-start screen on {date} (preset: {preset})."
        "Eitri 配置，由 Eitri 首次启动界面于 {date} 写入（预设：{preset}）。"
    CHeader2 "c.header2"
        "Eitri reads it at startup and never changes it. Every setting is explained in the guide, \"Configuration\"."
        "Eitri 启动时读取它，从不修改。每项设置见指南的“配置”一章。"
    CTmuxKeys "c.tmux_keys"
        "Keys: tmux's. The prefix and the keys after it come from your tmux configuration (now {key})."
        "按键：沿用 tmux。前缀和之后的键来自你的 tmux 配置（目前是 {key}）。"
    CTmuxNone "c.tmux_none"
        "Keys: tmux's. The prefix and the keys after it come from your tmux configuration (none found, so {key})."
        "按键：沿用 tmux。前缀和之后的键来自你的 tmux 配置（未找到，所以是 {key}）。"
    CPanelOpt "c.panel_opt"
        "A panel listing the keys after the prefix (milliseconds before it shows, or \"off\"):"
        "列出前缀之后按键的面板（显示前等待的毫秒数，或 \"off\"）："
    CNoTmux "c.no_tmux"
        "Keys: tmux-style, prefix {key}, whatever a tmux configuration on this computer says."
        "按键：tmux 风格，前缀 {key}，不管这台电脑上的 tmux 配置怎么写。"
    CPanelOn "c.panel_on"
        "After the prefix a panel lists the next keys; set it to \"off\" once you know them."
        "按下前缀后，面板会列出接下来的键；熟悉后可设为 \"off\"。"
    CPanelOnCm "c.panel_on_cm"
        "After the prefix a panel lists the keys after the prefix that still work here."
        "按下前缀后，面板列出这里仍然可用的键。"
    CLeader "c.leader"
        "The same window keys in Neovim under {lhs}, which which-key lists (:Eitri runs them too):"
        "在 Neovim 的 {lhs} 下也有这些窗口键，which-key 会列出（:Eitri 也能运行）："
    CLeaderOff "c.leader_off"
        "No report from Neovim yet; uncomment to get the window keys under {lhs}:"
        "还没有收到 Neovim 的报告；取消注释即可在 {lhs} 下使用窗口键："
    COtherPrefix "c.other_prefix"
        "Keys: tmux-style with {key} as the prefix; {key} twice sends {key} itself."
        "按键：tmux 风格，前缀 {key}；按两次 {key} 发送 {key} 本身。"
    CDelDefault "c.del_default"
        "{key} goes back to whatever has the keys"
        "{key} 交还给当前持有按键的模块"
    CWas "c.was"
        "was {action}"
        "原为 {action}"
    CTmuxLoses "c.tmux_loses"
        "Your tmux prefix ({key}) is not used in Eitri; this line wins."
        "你的 tmux 前缀（{key}）在 Eitri 里不用；以这一行为准。"
    CPressFailed "c.press_failed"
        "The press test did not see this key reach Eitri; it was written anyway."
        "按键测试没有看到这个键到达 Eitri；仍按要求写入。"
    CI3Alt "c.i3_alt"
        "Keys: tmux-style, prefix {key}. Your window manager's $mod is Alt, so the Alt keys after the prefix move to Ctrl+Alt (the same moves as docs/keymap/i3-alt-mod.lua):"
        "按键：tmux 风格，前缀 {key}。你的窗口管理器 $mod 是 Alt，所以前缀之后的 Alt 键改到 Ctrl+Alt（与 docs/keymap/i3-alt-mod.lua 相同）："
    CI3Super "c.i3_super"
        "$mod is Super: nothing after the prefix collides with it."
        "$mod 是 Super：前缀之后的键不与它冲突。"
    CTilingEitri "c.tiling_eitri"
        "Eitri arranges its own modules"
        "Eitri 排布自己的模块"
    CWindowsEitri "c.windows_eitri"
        "Windows: Eitri arranges its own modules (window.tiling \"auto\" would have picked {wm} here)."
        "窗口：Eitri 排布自己的模块（这里 window.tiling \"auto\" 本会选 {wm}）。"
    CWindowsWm "c.windows_wm"
        "Windows: {wm} arranges chats and pages as its own windows; Eitri's layout keys are off."
        "窗口：{wm} 把对话和页面当作自己的窗口排布；Eitri 的布局键关闭。"
}

/// The hash of the English column the Chinese was translated from: a changed English string fails a test
/// until the Chinese has been brought up to date and this is set again.
pub const TRANSLATED_FROM: &str = "12c7dc04fd7be55fad6315c3df35fd9e2c3f455b712010169400f4db027a536f";

/// The template of `id` in `lang`.
pub fn text(lang: Lang, id: TextId) -> &'static str {
    match lang {
        Lang::En => english(id),
        Lang::ZhCn => chinese(id),
    }
}

/// `template` with each `{name}` of `args` replaced by its value, in one pass, so a value that holds a `{...}`
/// of its own stays as it is. A placeholder `args` does not name stays too.
pub fn fill(template: &str, args: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        rest = &rest[open + 1..];
        let field = args
            .iter()
            .find(|(name, _)| rest.strip_prefix(name).is_some_and(|after| after.starts_with('}')));
        match field {
            Some((name, value)) => {
                out.push_str(value);
                rest = &rest[name.len() + 1..];
            }
            None => out.push('{'),
        }
    }
    out.push_str(rest);
    out
}

/// The columns `c` takes on a grid: East Asian wide and fullwidth characters take two.
pub(crate) fn char_columns(c: char) -> usize {
    match c as u32 {
        0x2e80..=0xa4cf | 0xac00..=0xd7a3 | 0xf900..=0xfaff | 0xfe30..=0xfe6f | 0xff00..=0xff60 | 0xffe0..=0xffe6 => 2,
        _ => 1,
    }
}

/// The columns `text` takes on a grid.
pub(crate) fn columns(text: &str) -> usize {
    text.chars().map(char_columns).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::collections::BTreeSet;

    fn placeholders(template: &str) -> BTreeSet<String> {
        let mut found = BTreeSet::new();
        let mut rest = template;
        while let Some(open) = rest.find('{') {
            let close = rest[open..].find('}').unwrap_or_else(|| panic!("unclosed: {template}"));
            found.insert(rest[open + 1..open + close].to_string());
            rest = &rest[open + close + 1..];
        }
        found
    }

    #[test]
    fn the_two_tables_have_the_same_ids_and_placeholders() {
        let mut names = BTreeSet::new();
        for id in TextId::ALL {
            assert!(names.insert(id.name()), "{} twice", id.name());
            assert!(!text(Lang::En, *id).is_empty(), "{}", id.name());
            assert!(!text(Lang::ZhCn, *id).is_empty(), "{}", id.name());
            assert_eq!(
                placeholders(text(Lang::En, *id)),
                placeholders(text(Lang::ZhCn, *id)),
                "{}",
                id.name()
            );
        }
    }

    #[test]
    fn every_chinese_string_stays_within_the_english_width_plus_a_fifth() {
        for id in TextId::ALL {
            let (en, zh) = (text(Lang::En, *id), text(Lang::ZhCn, *id));
            assert!(
                columns(zh) * 5 <= columns(en) * 6,
                "{}: {} columns against {}: {zh}",
                id.name(),
                columns(zh),
                columns(en)
            );
        }
    }

    #[test]
    fn the_translation_follows_the_english_table() {
        let mut table = String::new();
        for id in TextId::ALL {
            table.push_str(id.name());
            table.push('\t');
            table.push_str(text(Lang::En, *id));
            table.push('\n');
        }
        let hash = format!("{:x}", Sha256::digest(table.as_bytes()));
        assert_eq!(
            TRANSLATED_FROM, hash,
            "the English wording changed: bring the Chinese up to date, then set TRANSLATED_FROM to {hash}"
        );
    }

    #[test]
    fn fill_replaces_every_named_field() {
        assert_eq!(fill("{a} and {b} and {a}", &[("a", "1"), ("b", "2")]), "1 and 2 and 1");
        assert_eq!(fill("no fields", &[("a", "1")]), "no fields");
    }

    #[test]
    fn fill_leaves_a_field_it_is_not_given() {
        assert_eq!(fill("{a} {other}", &[("a", "1")]), "1 {other}");
        assert_eq!(fill("{ {", &[("a", "1")]), "{ {");
    }

    #[test]
    fn a_value_that_holds_a_field_is_not_filled_again() {
        assert_eq!(fill("{a} {b}", &[("a", "{b}"), ("b", "2")]), "{b} 2");
    }

    #[test]
    fn fill_keeps_text_that_is_not_ascii() {
        assert_eq!(fill("前缀 {key}。", &[("key", "Ctrl+a")]), "前缀 Ctrl+a。");
    }
}
