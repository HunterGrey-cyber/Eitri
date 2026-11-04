//! The window's appearance as a plugin page receives it: the colours, font stacks and colour scheme of
//! [`ThemeTokens::appearance_vars`], each checked by one grammar ([`page_var_ok`]) before it may reach a
//! page, either as a `--nv-*` custom property set by the bridge or inside the host's user style sheet.
//! Sizes are left out of the appearance set on purpose: a page has its own zoom, so a size change tells it nothing.
//! The one size a page may be given is [`TEXT_SIZE_VAR`], the editor's own text size, which every Eitri window but one
//! beside a person's own Neovim adds ([`PageTheme::with_text_size`]).

use serde_json::{Map, Value};

use crate::theme::tokens::{ThemeTokens, FONT_FAMILY_MAX_CHARS, FONT_LIST_MAX_CHARS, FONT_LIST_MAX_FAMILIES};

/// Longest name a page value may have, in Unicode scalar values.
const NAME_MAX_CHARS: usize = 64;
/// Longest bare (unquoted) font family, such as `system-ui`.
const BARE_FAMILY_MAX_CHARS: usize = 40;

/// The custom property that carries the editor's text size, in the page's CSS px, to a page that wants its prose
/// the editor's size. It is not part of `ThemeTokens::appearance_vars`: a window beside a person's own
/// Neovim (a terminal) has no Eitri editor font to give, and a page without it keeps its own. Every other Eitri window
/// gives it (window 2 and a page window follow window 1's `guifont`).
pub const TEXT_SIZE_VAR: &str = "--nv-text-size";

/// The appearance set a plugin page is given, already checked, and the background its view shows before
/// the page paints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageTheme {
    vars: Vec<(String, String)>,
    background: [u8; 3],
}

impl PageTheme {
    /// `tokens.appearance_vars()`, each checked with [`page_var_ok`] (a pair that fails is dropped and
    /// logged); the background is `tokens.bg`.
    pub fn from_tokens(tokens: &ThemeTokens) -> PageTheme {
        let vars = tokens
            .appearance_vars()
            .into_iter()
            .filter(|(name, value)| {
                let ok = page_var_ok(name, value);
                if !ok {
                    eprintln!("[plugin] theme: dropped {name}: not a page value");
                }
                ok
            })
            .collect();
        PageTheme {
            vars,
            background: [tokens.bg.r, tokens.bg.g, tokens.bg.b],
        }
    }

    /// The set for the fallback tokens: what a page has before the editor reported its colorscheme.
    pub fn fallback() -> PageTheme {
        PageTheme::from_tokens(&ThemeTokens::fallback())
    }

    /// This set with [`TEXT_SIZE_VAR`] set to `css_px` (rounded to two decimals), or taken out for `None`. A size
    /// that is not a finite number of at least 1 and under 1000 is taken out too: the page then keeps its own.
    pub fn with_text_size(mut self, css_px: Option<f32>) -> PageTheme {
        self.vars.retain(|(name, _)| name != TEXT_SIZE_VAR);
        if let Some(px) = css_px.filter(|px| px.is_finite() && (1.0..1000.0).contains(px)) {
            // `{}` on an `f32` prints `14`, not `14.0`, so the common case reads as it was typed.
            let value = format!("{}px", (px * 100.0).round() / 100.0);
            if page_var_ok(TEXT_SIZE_VAR, &value) {
                self.vars.push((TEXT_SIZE_VAR.to_string(), value));
            }
        }
        self
    }

    pub fn vars(&self) -> &[(String, String)] {
        &self.vars
    }

    pub fn background(&self) -> [u8; 3] {
        self.background
    }

    /// `{"vars": {<name>: <value>, ...}}`: the `theme` event's data and the hello reply's `theme`.
    pub fn to_json(&self) -> Value {
        let vars: Map<String, Value> = self
            .vars
            .iter()
            .map(|(name, value)| (name.clone(), Value::String(value.clone())))
            .collect();
        let mut out = Map::new();
        out.insert("vars".to_string(), Value::Object(vars));
        Value::Object(out)
    }

    /// One `:root` rule: the page's base colours, scheme and prose face read from the variables, then every
    /// variable. Every value passed `page_var_ok`, so none can close the rule, open a comment or a block.
    pub fn user_style_sheet(&self) -> String {
        let mut sheet = String::from(
            ":root{color-scheme:var(--nv-color-scheme);background-color:var(--nv-bg);color:var(--nv-fg);\
             font-family:var(--nv-font-prose);",
        );
        for (name, value) in &self.vars {
            sheet.push_str(name);
            sheet.push(':');
            sheet.push_str(value);
            sheet.push(';');
        }
        sheet.push('}');
        sheet
    }
}

/// Whether `value` may be given to a page as the custom property `name`.
///
/// The name is `--nv-` and dash-separated groups of lowercase ASCII letters and digits, at most 64
/// characters. Its shape decides the value's kind, so nothing has to list the set: `--nv-color-scheme` is
/// `light` or `dark`, a name starting `--nv-font-` is a font list, a name ending `-size` is a length in whole or
/// hundredths of CSS px (`14px`, `18.67px`), and every other name is a `#rrggbb` colour in lowercase hex.
/// Lengths count Unicode scalar values.
pub fn page_var_ok(name: &str, value: &str) -> bool {
    if !name_ok(name) {
        return false;
    }
    if name == "--nv-color-scheme" {
        value == "light" || value == "dark"
    } else if name.starts_with("--nv-font-") {
        font_list_ok(value)
    } else if name.ends_with("-size") {
        size_ok(value)
    } else {
        colour_ok(value)
    }
}

/// `<1 to 3 digits, no leading zero>[.<1 or 2 digits>]px`.
fn size_ok(value: &str) -> bool {
    let Some(number) = value.strip_suffix("px") else {
        return false;
    };
    let (whole, fraction) = match number.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (number, None),
    };
    let digits = |text: &str, most: usize| (1..=most).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_digit());
    digits(whole, 3) && !whole.starts_with('0') && fraction.is_none_or(|f| digits(f, 2))
}

fn name_ok(name: &str) -> bool {
    if name.chars().count() > NAME_MAX_CHARS {
        return false;
    }
    let Some(rest) = name.strip_prefix("--nv-") else {
        return false;
    };
    rest.split('-')
        .all(|group| !group.is_empty() && group.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()))
}

fn colour_ok(value: &str) -> bool {
    let Some(hex) = value.strip_prefix('#') else {
        return false;
    };
    hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A character a quoted family may not hold: one that would end the string or the declaration, open a
/// block, a group or a comment, or is a control character.
fn refused_in_family(c: char) -> bool {
    matches!(
        c,
        '"' | '\\' | ';' | '{' | '}' | '<' | '>' | '(' | ')' | '[' | ']' | '/' | '*'
    ) || c < ' '
        || ('\u{7f}'..='\u{9f}').contains(&c)
}

/// 1 to 16 families separated by `,` and at most one space, at most 1024 characters in all. Read left to
/// right, so a comma inside a quoted family stays part of it.
fn font_list_ok(value: &str) -> bool {
    if value.chars().count() > FONT_LIST_MAX_CHARS {
        return false;
    }
    let mut chars = value.chars().peekable();
    let mut families = 0;
    loop {
        if !family_ok(&mut chars) {
            return false;
        }
        families += 1;
        if families > FONT_LIST_MAX_FAMILIES {
            return false;
        }
        match chars.next() {
            None => return true,
            Some(',') => {
                if chars.peek() == Some(&' ') {
                    chars.next();
                }
            }
            Some(_) => return false,
        }
    }
}

/// One family, consumed from `chars` up to (not including) the separator after it.
fn family_ok(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> bool {
    match chars.peek() {
        Some('"') => {
            chars.next();
            let mut len = 0;
            loop {
                match chars.next() {
                    None => return false,
                    Some('"') => return (1..=FONT_FAMILY_MAX_CHARS).contains(&len),
                    Some(c) if refused_in_family(c) => return false,
                    Some(_) => len += 1,
                }
            }
        }
        Some(c) if c.is_ascii_alphabetic() => {
            let mut len = 0;
            while let Some(&c) = chars.peek() {
                if c.is_ascii_alphanumeric() || c == '-' {
                    len += 1;
                    chars.next();
                } else {
                    break;
                }
            }
            len <= BARE_FAMILY_MAX_CHARS
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::protocol::ThemeEvent;
    use crate::theme::payload::{HlAttrs, NvimOptions, NvimThemePayload, PAYLOAD_VERSION};
    use crate::theme::tokens::mono_font_stack;
    use std::collections::HashMap;

    fn hl(fg: Option<u32>, bg: Option<u32>) -> HlAttrs {
        HlAttrs { fg, bg, reverse: false }
    }

    fn payload(background: &str, guifont: &str, groups: &[(&str, HlAttrs)]) -> NvimThemePayload {
        NvimThemePayload {
            v: PAYLOAD_VERSION,
            groups: groups
                .iter()
                .map(|(n, a)| (n.to_string(), a.clone()))
                .collect::<HashMap<_, _>>(),
            options: NvimOptions {
                background: background.into(),
                guifont: guifont.into(),
                colors_name: "t".into(),
                cmdheight: None,
                start_scale: None,
            },
        }
    }

    fn rose_pine_dawn() -> ThemeTokens {
        ThemeTokens::derive(&payload(
            "light",
            "",
            &[
                ("Normal", hl(Some(0x575279), Some(0xfaf4ed))),
                ("NormalFloat", hl(Some(0x575279), Some(0xfffaf3))),
                ("Visual", hl(None, Some(0xdfdad9))),
                ("Comment", hl(Some(0x9893a5), None)),
                ("IncSearch", hl(Some(0xfaf4ed), Some(0xd7827e))),
            ],
        ))
    }

    /// The sheet is one `:root` rule that nothing inside it can close, escape or comment out.
    fn assert_one_root_rule(sheet: &str) {
        assert!(sheet.starts_with(":root{"), "{sheet}");
        assert!(sheet.ends_with('}'), "{sheet}");
        assert!(!sheet.contains('<'), "{sheet}");
        assert!(!sheet.contains('\\'), "{sheet}");
        assert!(!sheet.contains("/*"), "{sheet}");
        assert_eq!(sheet.matches('{').count(), 1, "{sheet}");
        assert_eq!(sheet.matches('}').count(), 1, "{sheet}");
    }

    #[test]
    fn every_shared_example_gets_its_listed_answer() {
        let examples: Value = serde_json::from_str(include_str!("theme_values.json")).unwrap();
        let accepted = examples["accepted"].as_array().unwrap();
        let refused = examples["refused"].as_array().unwrap();
        assert!(!accepted.is_empty() && !refused.is_empty());
        for (list, want) in [(accepted, true), (refused, false)] {
            for example in list {
                let id = example["id"].as_str().unwrap();
                let name = example["name"].as_str().unwrap();
                let value = example["value"].as_str().unwrap();
                assert_eq!(page_var_ok(name, value), want, "{id}");
            }
        }
    }

    #[test]
    fn the_fallback_page_theme_is_the_appearance_set() {
        let theme = PageTheme::fallback();
        let names: Vec<&str> = theme.vars().iter().map(|(n, _)| n.as_str()).collect();
        let appearance = ThemeTokens::fallback().appearance_vars();
        let want: Vec<&str> = appearance.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, want);
        for (name, value) in theme.vars() {
            assert!(page_var_ok(name, value), "{name}: {value}");
        }
        assert_eq!(theme.background(), [0x14, 0x16, 0x1b]);
    }

    #[test]
    fn a_hostile_guifont_still_gives_the_complete_set() {
        let mut pool: Vec<char> = "\"\\;{}<>()[]/*".chars().collect();
        pool.extend(['\u{0}', '\n', '\u{1f}', '\u{7f}', '\u{85}', '\u{9f}', 'a', '\u{1D538}']);
        let family: String = (0..150).map(|i| pool[i % pool.len()]).collect();
        let guifont = vec![family; 40].join(",");
        let tokens = ThemeTokens::derive(&payload("dark", &guifont, &[]));
        let theme = PageTheme::from_tokens(&tokens);
        assert_eq!(theme.vars().len(), 55);
        assert!(theme.vars().iter().any(|(n, _)| n == "--nv-font-mono"));
        assert_one_root_rule(&theme.user_style_sheet());
    }

    /// Long and hostile guifont values, from the same fixed seed and pool as the font stack's own property
    /// test: every stack the editor's `guifont` can give is a page value.
    #[test]
    fn every_guifont_output_passes_the_page_rule() {
        const POOL: &[char] = &[
            'a',
            'Z',
            '0',
            ' ',
            '_',
            '-',
            '.',
            '@',
            '\'',
            '\u{e9}',
            '\u{4e2d}',
            '\u{1D538}',
            '\u{2028}',
            '\u{a0}',
            '\u{5c}',
            '"',
            ';',
            '{',
            '}',
            '<',
            '>',
            '(',
            ')',
            '[',
            ']',
            '/',
            '*',
            '\u{0}',
            '\n',
            '\t',
            '\u{1f}',
            '\u{7f}',
            '\u{80}',
            '\u{85}',
            '\u{9f}',
        ];
        struct Rng(u64);
        impl Rng {
            fn below(&mut self, bound: usize) -> usize {
                self.0 ^= self.0 << 13;
                self.0 ^= self.0 >> 7;
                self.0 ^= self.0 << 17;
                (self.0 % bound as u64) as usize
            }
        }
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for case in 0..2000 {
            let mut families = Vec::new();
            for _ in 0..1 + rng.below(40) {
                let mut family = String::new();
                for _ in 0..1 + rng.below(150) {
                    family.push(POOL[rng.below(POOL.len())]);
                }
                families.push(family);
            }
            let guifont = families.join(",");
            let stack = mono_font_stack(&guifont);
            assert!(page_var_ok("--nv-font-mono", &stack), "{guifont:?} -> {stack:?}");
            if case % 100 == 0 {
                let tokens = ThemeTokens::derive(&payload("dark", &guifont, &[]));
                assert_one_root_rule(&PageTheme::from_tokens(&tokens).user_style_sheet());
            }
        }
    }

    #[test]
    fn a_size_only_change_gives_an_equal_page_theme() {
        let before = ThemeTokens::fallback();
        let mut after = ThemeTokens::fallback();
        after.font_size_px = 18.0;
        after.editor_row_px = Some(30.0);
        after.editor_cmd_rows = Some(3);
        assert_eq!(PageTheme::from_tokens(&before), PageTheme::from_tokens(&after));
    }

    #[test]
    fn the_editors_text_size_is_one_more_var_and_only_when_given() {
        let base = PageTheme::fallback();
        assert_eq!(base.clone().with_text_size(None), base);
        let sized = base.clone().with_text_size(Some(18.0));
        assert_eq!(sized.vars().len(), base.vars().len() + 1);
        assert!(sized.vars().contains(&(TEXT_SIZE_VAR.to_string(), "18px".to_string())));
        assert!(sized.vars().iter().all(|(name, value)| page_var_ok(name, value)));
        let again = sized.clone().with_text_size(Some(56.0 / 3.0));
        assert!(again
            .vars()
            .contains(&(TEXT_SIZE_VAR.to_string(), "18.67px".to_string())));
        assert_eq!(again.vars().iter().filter(|(n, _)| n == TEXT_SIZE_VAR).count(), 1);
        assert_eq!(sized.with_text_size(None), base, "taking it out restores the set");
        for bad in [f32::NAN, f32::INFINITY, -3.0, 0.0, 0.5, 1000.0, 1e9] {
            assert_eq!(base.clone().with_text_size(Some(bad)), base, "{bad}");
        }
        assert_one_root_rule(&base.with_text_size(Some(12.5)).user_style_sheet());
    }

    #[test]
    fn a_size_name_takes_a_length_and_nothing_else() {
        for ok in ["14px", "9px", "18.67px", "32.5px", "999.99px", "1px"] {
            assert!(page_var_ok("--nv-text-size", ok), "{ok}");
        }
        for bad in [
            "",
            "14",
            "px",
            "14 px",
            "14PX",
            "14em",
            "014px",
            "0px",
            "1000px",
            "14.px",
            ".5px",
            "14.123px",
            "-14px",
            "+14px",
            "1e1px",
            "#14161b",
            "14px;",
            "14px}",
            "calc(1px)",
            "14px ",
            " 14px",
            "１４px",
        ] {
            assert!(!page_var_ok("--nv-text-size", bad), "{bad:?}");
        }
        assert!(
            !page_var_ok("--nv-bg", "14px"),
            "a name that does not end -size is a colour"
        );
    }

    #[test]
    fn the_user_style_sheet_is_one_root_rule() {
        for theme in [PageTheme::fallback(), PageTheme::from_tokens(&rose_pine_dawn())] {
            let mut want = String::from(
                ":root{color-scheme:var(--nv-color-scheme);background-color:var(--nv-bg);color:var(--nv-fg);\
                 font-family:var(--nv-font-prose);",
            );
            for (name, value) in theme.vars() {
                want.push_str(&format!("{name}:{value};"));
            }
            want.push('}');
            let sheet = theme.user_style_sheet();
            assert_eq!(sheet, want);
            assert_one_root_rule(&sheet);
        }
    }

    #[test]
    fn to_json_round_trips_through_the_theme_event() {
        for theme in [PageTheme::fallback(), PageTheme::from_tokens(&rose_pine_dawn())] {
            let event: ThemeEvent = serde_json::from_value(theme.to_json()).unwrap();
            let mut want: Vec<(String, String)> = theme.vars().to_vec();
            want.sort();
            let got: Vec<(String, String)> = event.vars.into_iter().collect();
            assert_eq!(got, want);
        }
    }

    /// Run by hand when the appearance set changes:
    /// `EITRI_WRITE_PLUGIN_FIXTURES=1 cargo test -p eitri-core --lib -- --ignored write_theme_fixtures`.
    /// Generated files stay in `target/plugin-fixtures/` for diff inspection before copying them into
    /// `core/src/plugin/fixtures/`.
    #[test]
    #[ignore = "writes fixtures; needs EITRI_WRITE_PLUGIN_FIXTURES=1"]
    fn write_theme_fixtures() {
        if std::env::var("EITRI_WRITE_PLUGIN_FIXTURES").as_deref() != Ok("1") {
            return;
        }
        let out = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../target/plugin-fixtures"));
        std::fs::create_dir_all(out).unwrap();
        let event = serde_json::json!({
            "v": 1,
            "event": "theme",
            "data": PageTheme::from_tokens(&rose_pine_dawn()).to_json(),
        });
        let mut hello: Value = serde_json::from_str(include_str!("fixtures/rep-hello.json")).unwrap();
        hello["result"]["theme"] = PageTheme::fallback().to_json();
        let mut hello_settings: Value = serde_json::from_str(include_str!("fixtures/rep-hello-settings.json")).unwrap();
        hello_settings["result"]["theme"] = PageTheme::fallback().to_json();
        for (name, value) in [
            ("ev-theme.json", event),
            ("rep-hello.json", hello),
            ("rep-hello-settings.json", hello_settings),
        ] {
            let text = serde_json::to_string_pretty(&value).unwrap() + "\n";
            std::fs::write(out.join(name), text).unwrap();
        }
    }
}
