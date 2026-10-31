//! What Eitri makes of an nvim report's probe: who owns, or probably owns, each probed chord, who
//! answers `Ctrl+h/j/k/l` in Normal mode, which known plugins are only declared, and which
//! distribution is in use. Pure: a report in, findings out; nothing here reads nvim or the disk.
//!
//! A completion engine that is loaded or declared is inferred from `plugins`, not from the maps,
//! because its own Insert mappings usually appear only on `InsertEnter` and the probe is not re-read
//! then. So while an engine is present no finding says an Insert chord is free.

use super::{nvim_leader_token, NvimProbe, NvimReport, PluginState, ProbeKey, Scope, PROBED_CHORDS, PROBED_MODES};
use crate::keymap::KeySpec;

/// A plugin the probe knows by name (the plugin half of [`super::KNOWN_PLUGINS`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum KnownPlugin {
    VimTmuxNavigator,
    SmartSplits,
    Navigator,
    WhichKey,
    BlinkCmp,
    NvimCmp,
    LazyNvim,
}

impl KnownPlugin {
    pub const ALL: [KnownPlugin; 7] = [
        KnownPlugin::VimTmuxNavigator,
        KnownPlugin::SmartSplits,
        KnownPlugin::Navigator,
        KnownPlugin::WhichKey,
        KnownPlugin::BlinkCmp,
        KnownPlugin::NvimCmp,
        KnownPlugin::LazyNvim,
    ];

    /// The name the plugin's own project uses, as the probe reports it.
    pub fn name(self) -> &'static str {
        match self {
            KnownPlugin::VimTmuxNavigator => "vim-tmux-navigator",
            KnownPlugin::SmartSplits => "smart-splits.nvim",
            KnownPlugin::Navigator => "Navigator.nvim",
            KnownPlugin::WhichKey => "which-key.nvim",
            KnownPlugin::BlinkCmp => "blink.cmp",
            KnownPlugin::NvimCmp => "nvim-cmp",
            KnownPlugin::LazyNvim => "lazy.nvim",
        }
    }

    pub fn from_name(name: &str) -> Option<KnownPlugin> {
        KnownPlugin::ALL.into_iter().find(|plugin| plugin.name() == name)
    }
}

/// A Neovim distribution the probe knows by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum KnownDistro {
    LazyVim,
    NvChad,
    AstroNvim,
    LunarVim,
}

impl KnownDistro {
    pub const ALL: [KnownDistro; 4] = [
        KnownDistro::LazyVim,
        KnownDistro::NvChad,
        KnownDistro::AstroNvim,
        KnownDistro::LunarVim,
    ];

    pub fn name(self) -> &'static str {
        match self {
            KnownDistro::LazyVim => "LazyVim",
            KnownDistro::NvChad => "NvChad",
            KnownDistro::AstroNvim => "AstroNvim",
            KnownDistro::LunarVim => "LunarVim",
        }
    }
}

/// The nvim modes the probe reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    Normal,
    Visual,
    Insert,
    Terminal,
}

impl Mode {
    /// The mode letter the probe uses (`n`, `x`, `i`, `t`).
    pub fn letter(self) -> &'static str {
        match self {
            Mode::Normal => "n",
            Mode::Visual => "x",
            Mode::Insert => "i",
            Mode::Terminal => "t",
        }
    }

    fn from_letter(letter: &str) -> Option<Mode> {
        [Mode::Normal, Mode::Visual, Mode::Insert, Mode::Terminal]
            .into_iter()
            .find(|mode| mode.letter() == letter)
    }
}

/// Whether the finding comes from a mapping the probe saw, or is inferred from a plugin that is loaded
/// or declared and has not mapped the chord yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Certainty {
    Observed,
    Probably,
}

/// Who answers a chord. `Origin` and `Desc` carry the probe's text unchanged (at most 200 characters,
/// control characters included): whoever shows them escapes them. `Desc` also stands for a mapping
/// that has no description, carrying its `rhs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    NvimDefault,
    Eitri,
    Plugin(KnownPlugin),
    Origin(String),
    Desc(String),
    LuaFunction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChordOwner {
    pub chord: KeySpec,
    pub mode: Mode,
    pub owner: Owner,
    pub certainty: Certainty,
}

/// Who answers `Ctrl+h/j/k/l` in Normal mode, which decides what Eitri says and recommends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavOwner {
    /// Nothing maps them: Eitri's own moves apply.
    EitriFallback,
    VimTmuxNavigator,
    SmartSplits,
    Navigator,
    /// A mapping of the user's own.
    UserMapping(Owner),
    /// lazy.nvim's placeholder for a plugin it has not loaded yet; which plugin is not known.
    LazyStub,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvimFindings {
    /// The leader as the classifier reads it.
    pub leader: String,
    /// Every probed chord in every probed mode that has something to say; an Insert chord is missing
    /// while an engine is present and nothing is known about it.
    pub chords: Vec<ChordOwner>,
    pub nav: NavOwner,
    /// Declared by lazy.nvim, not loaded yet.
    pub pending: Vec<KnownPlugin>,
    pub distro: Option<KnownDistro>,
}

/// Each completion engine's default Insert chords among the probed ones, and what the row was read
/// from. nvim-cmp sets no mapping itself: its row is the last kickstart.nvim `init.lua` that used it.
/// `<C-f>` is not probed and stays as data.
pub const ENGINE_INSERT_DEFAULTS: &[(KnownPlugin, &[&str], &str)] = &[
    (
        KnownPlugin::BlinkCmp,
        &["<C-Space>", "<C-b>", "<C-f>", "<C-k>"],
        "blink.cmp v1.10.2, lua/blink/cmp/keymap/presets.lua `default` (blob identical at 716967ca)",
    ),
    (
        KnownPlugin::NvimCmp,
        &["<C-Space>", "<C-b>", "<C-f>", "<C-l>", "<C-h>"],
        "kickstart.nvim 9929044f init.lua, the last one using nvim-cmp (blink.cmp replaced it in d350db24)",
    ),
];

/// A probed chord as a key: `<C-Space>` is `C-Space`.
fn chord_spec(chord: &str) -> KeySpec {
    let inner = chord.trim_start_matches('<').trim_end_matches('>');
    KeySpec::parse(inner).expect("every probed chord is a tmux key name")
}

/// A chord the probe was asked for beyond the fixed list, as a key: `<C-O>` is `C-o`. `None` for an lhs tmux has no
/// name for.
fn extra_chord_spec(lhs: &str) -> Option<KeySpec> {
    KeySpec::parse(lhs.strip_prefix('<')?.strip_suffix('>')?).ok()
}

/// A mapping's lhs in one spelling: nvim writes the Ctrl+Space key `<Nul>` or `<C-@>` as often as
/// `<C-Space>`, and `keytrans` capitalises letters (`<C-B>`).
fn canonical(lhs: &str) -> String {
    let lower = lhs.to_ascii_lowercase();
    match lower.as_str() {
        "<nul>" | "<c-@>" => "<c-space>".to_string(),
        _ => lower,
    }
}

fn owner_of(key: &ProbeKey) -> Owner {
    if key
        .desc
        .as_deref()
        .is_some_and(|desc| desc.starts_with(NAV_FALLBACK_DESC_PREFIX))
    {
        return Owner::Eitri;
    }
    if let Some(origin) = key.origin.as_deref().filter(|origin| !origin.is_empty()) {
        return KnownPlugin::from_name(origin)
            .map(Owner::Plugin)
            .unwrap_or_else(|| Owner::Origin(origin.to_string()));
    }
    if let Some(desc) = key.desc.as_deref().filter(|desc| !desc.is_empty()) {
        return Owner::Desc(desc.to_string());
    }
    match key.rhs.as_deref() {
        Some(rhs) if !key.callback => Owner::Desc(rhs.to_string()),
        _ => Owner::LuaFunction,
    }
}

/// The map the probe saw for a chord in a mode: the current buffer's when there is one, since it
/// shadows the global map.
fn observed<'a>(probe: &'a NvimProbe, chord: &str, mode: &str) -> Option<&'a ProbeKey> {
    let want = canonical(chord);
    let mut matching = probe
        .keys
        .iter()
        .filter(|key| key.mode == mode && canonical(&key.lhs) == want);
    let first = matching.next()?;
    match first.scope {
        Scope::Buffer => Some(first),
        Scope::Global => matching.find(|key| key.scope == Scope::Buffer).or(Some(first)),
    }
}

fn present(probe: &NvimProbe, plugin: KnownPlugin) -> bool {
    probe.plugins.contains_key(plugin.name())
}

/// The description Eitri's own window-or-pane moves carry (`core/src/nav_fallback.lua` builds each one
/// as this prefix and a direction).
pub(crate) const NAV_FALLBACK_DESC_PREFIX: &str = "eitri: window or pane ";

/// Who answers `Ctrl+h/j/k/l` in Normal mode. What the probe observed on those chords decides first:
/// a plugin's own map, then a map of the user's own (which overrides a plugin that is merely present),
/// and only then a plugin that is loaded or declared with nothing observed, lazy.nvim's placeholder,
/// or Eitri's own moves.
fn nav_owner(probe: &NvimProbe) -> NavOwner {
    let mapped: Vec<Owner> = ["<C-h>", "<C-j>", "<C-k>", "<C-l>"]
        .iter()
        .filter_map(|chord| observed(probe, chord, "n"))
        .map(owner_of)
        .collect();
    for owner in &mapped {
        match owner {
            Owner::Plugin(KnownPlugin::VimTmuxNavigator) => return NavOwner::VimTmuxNavigator,
            Owner::Plugin(KnownPlugin::SmartSplits) => return NavOwner::SmartSplits,
            Owner::Plugin(KnownPlugin::Navigator) => return NavOwner::Navigator,
            _ => {}
        }
    }
    if let Some(user) = mapped
        .iter()
        .find(|owner| !matches!(owner, Owner::Eitri | Owner::Plugin(KnownPlugin::LazyNvim)))
    {
        return NavOwner::UserMapping(user.clone());
    }
    for (plugin, nav) in [
        (KnownPlugin::VimTmuxNavigator, NavOwner::VimTmuxNavigator),
        (KnownPlugin::SmartSplits, NavOwner::SmartSplits),
        (KnownPlugin::Navigator, NavOwner::Navigator),
    ] {
        if present(probe, plugin) {
            return nav;
        }
    }
    if mapped.contains(&Owner::Plugin(KnownPlugin::LazyNvim)) {
        return NavOwner::LazyStub;
    }
    NavOwner::EitriFallback
}

/// The findings for one report. A report without a probe gives only its leader.
pub fn findings(report: &NvimReport) -> NvimFindings {
    let leader = nvim_leader_token(report).to_string();
    let Some(probe) = &report.probe else {
        return NvimFindings {
            leader,
            chords: Vec::new(),
            nav: NavOwner::EitriFallback,
            pending: Vec::new(),
            distro: None,
        };
    };
    let engines: Vec<KnownPlugin> = ENGINE_INSERT_DEFAULTS
        .iter()
        .map(|(engine, _, _)| *engine)
        .filter(|engine| present(probe, *engine))
        .collect();
    let mut chords = Vec::new();
    for chord in PROBED_CHORDS {
        for mode_letter in PROBED_MODES {
            let Some(mode) = Mode::from_letter(mode_letter) else {
                continue;
            };
            let (owner, certainty) = if let Some(key) = observed(probe, chord, mode_letter) {
                (owner_of(key), Certainty::Observed)
            } else if mode == Mode::Insert && !engines.is_empty() {
                let claimed = ENGINE_INSERT_DEFAULTS
                    .iter()
                    .filter(|(engine, _, _)| engines.contains(engine))
                    .find(|(_, claims, _)| claims.iter().any(|claim| canonical(claim) == canonical(chord)));
                match claimed {
                    Some((engine, _, _)) => (Owner::Plugin(*engine), Certainty::Probably),
                    None => continue,
                }
            } else {
                (Owner::NvimDefault, Certainty::Observed)
            };
            chords.push(ChordOwner {
                chord: chord_spec(chord),
                mode,
                owner,
                certainty,
            });
        }
    }
    // A chord a rescan asked for, beyond the fixed list, says only what the probe saw: no engine claims it.
    let mut extra: Vec<&str> = Vec::new();
    for key in &probe.keys {
        let fixed = PROBED_CHORDS
            .iter()
            .any(|chord| canonical(chord) == canonical(&key.lhs));
        if !fixed && !extra.iter().any(|lhs| canonical(lhs) == canonical(&key.lhs)) {
            extra.push(&key.lhs);
        }
    }
    for lhs in extra {
        let Some(chord) = extra_chord_spec(lhs) else {
            continue;
        };
        for mode_letter in PROBED_MODES {
            if let (Some(mode), Some(key)) = (Mode::from_letter(mode_letter), observed(probe, lhs, mode_letter)) {
                chords.push(ChordOwner {
                    chord,
                    mode,
                    owner: owner_of(key),
                    certainty: Certainty::Observed,
                });
            }
        }
    }
    let pending = KnownPlugin::ALL
        .into_iter()
        .filter(|plugin| probe.plugins.get(plugin.name()) == Some(&PluginState::Declared))
        .collect();
    let distro = KnownDistro::ALL
        .into_iter()
        .find(|distro| probe.plugins.contains_key(distro.name()));
    NvimFindings {
        leader,
        chords,
        nav: nav_owner(probe),
        pending,
        distro,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nvim_keys::{parse_report, KNOWN_PLUGINS};

    fn report(probe: &str) -> NvimReport {
        let json = format!(r#"{{"v":1,"mapleader":" ","timeoutlen":300,"timeout":true,"maps":[],"probe":{probe}}}"#);
        parse_report(json.as_bytes()).expect("a recorded report parses")
    }

    fn key(mode: &str, lhs: &str, extra: &str) -> String {
        format!(r#"{{"mode":"{mode}","lhs":"{lhs}","scope":"global"{extra}}}"#)
    }

    fn probe(keys: &[String], plugins: &str) -> NvimReport {
        report(&format!(r#"{{"keys":[{}],"plugins":{{{plugins}}}}}"#, keys.join(",")))
    }

    fn chord_in(found: &NvimFindings, chord: &str, mode: Mode) -> Option<ChordOwner> {
        let spec = chord_spec(chord);
        found.chords.iter().find(|c| c.chord == spec && c.mode == mode).cloned()
    }

    #[test]
    fn a_report_without_a_probe_has_only_a_leader() {
        let json = r#"{"v":1,"mapleader":" ","timeoutlen":300,"timeout":true,"maps":[]}"#;
        let found = findings(&parse_report(json.as_bytes()).unwrap());
        assert_eq!(found.leader, " ");
        assert!(found.chords.is_empty() && found.pending.is_empty() && found.distro.is_none());
        assert_eq!(found.nav, NavOwner::EitriFallback);
    }

    #[test]
    fn nothing_mapped_leaves_the_chords_to_nvim_and_the_moves_to_eitri() {
        let found = findings(&probe(&[], ""));
        assert_eq!(found.nav, NavOwner::EitriFallback);
        assert_eq!(found.chords.len(), PROBED_CHORDS.len() * PROBED_MODES.len());
        assert!(found
            .chords
            .iter()
            .all(|c| c.owner == Owner::NvimDefault && c.certainty == Certainty::Observed));
    }

    #[test]
    fn a_plain_move_mapping_is_the_users_own_and_carries_its_text() {
        let found = findings(&probe(
            &[
                key("n", "<C-H>", r#","rhs":"<C-w>h""#),
                key("n", "<C-J>", r#","rhs":null,"callback":true,"desc":"down""#),
            ],
            "",
        ));
        assert_eq!(found.nav, NavOwner::UserMapping(Owner::Desc("<C-w>h".into())));
        let h = chord_in(&found, "<C-h>", Mode::Normal).unwrap();
        assert_eq!(
            (h.owner, h.certainty),
            (Owner::Desc("<C-w>h".into()), Certainty::Observed)
        );
        assert_eq!(
            chord_in(&found, "<C-j>", Mode::Normal).unwrap().owner,
            Owner::Desc("down".into())
        );
    }

    #[test]
    fn a_callback_without_a_description_or_origin_is_a_lua_function() {
        let found = findings(&probe(&[key("n", "<C-k>", r#","rhs":null,"callback":true"#)], ""));
        assert_eq!(found.nav, NavOwner::UserMapping(Owner::LuaFunction));
    }

    #[test]
    fn vim_tmux_navigator_owns_the_moves_by_its_global_or_its_maps() {
        let by_plugin = findings(&probe(&[], r#""vim-tmux-navigator":"loaded""#));
        assert_eq!(by_plugin.nav, NavOwner::VimTmuxNavigator);
        let by_map = findings(&probe(
            &[key(
                "n",
                "<C-H>",
                r#","rhs":":TmuxNavigateLeft<CR>","origin":"vim-tmux-navigator""#,
            )],
            "",
        ));
        assert_eq!(by_map.nav, NavOwner::VimTmuxNavigator);
        assert_eq!(
            chord_in(&by_map, "<C-h>", Mode::Normal).unwrap().owner,
            Owner::Plugin(KnownPlugin::VimTmuxNavigator)
        );
    }

    #[test]
    fn smart_splits_and_navigator_own_the_moves_when_loaded_or_mapped() {
        let mapped = |origin: &str| {
            findings(&probe(
                &[key("n", "<C-L>", &format!(r#","callback":true,"origin":"{origin}""#))],
                "",
            ))
            .nav
        };
        assert_eq!(
            findings(&probe(&[], r#""smart-splits.nvim":"loaded""#)).nav,
            NavOwner::SmartSplits
        );
        assert_eq!(mapped("smart-splits.nvim"), NavOwner::SmartSplits);
        assert_eq!(
            findings(&probe(&[], r#""Navigator.nvim":"loaded""#)).nav,
            NavOwner::Navigator
        );
        assert_eq!(mapped("Navigator.nvim"), NavOwner::Navigator);
    }

    #[test]
    fn eitris_installed_moves_are_eitris_and_leave_the_fallback_in_charge() {
        let desc = |dir: &str| format!(r#","callback":true,"desc":"{NAV_FALLBACK_DESC_PREFIX}{dir}""#);
        let found = findings(&probe(
            &[
                key("n", "<C-H>", &desc("left")),
                key("n", "<C-J>", &desc("down")),
                key("x", "<C-H>", &desc("left")),
                key("i", "<C-L>", &desc("right")),
            ],
            "",
        ));
        assert_eq!(found.nav, NavOwner::EitriFallback);
        for (chord, mode) in [
            ("<C-h>", Mode::Normal),
            ("<C-j>", Mode::Normal),
            ("<C-h>", Mode::Visual),
            ("<C-l>", Mode::Insert),
        ] {
            let owner = chord_in(&found, chord, mode).unwrap();
            assert_eq!(
                (owner.owner, owner.certainty),
                (Owner::Eitri, Certainty::Observed),
                "{chord}"
            );
        }
    }

    #[test]
    fn the_fallback_description_is_the_one_the_lua_snippet_installs() {
        let expected = format!("local DESC = \"{NAV_FALLBACK_DESC_PREFIX}\"");
        assert!(crate::pane_switch::NAV_FALLBACK_LUA.contains(&expected));
    }

    #[test]
    fn an_observed_plugin_map_beats_a_different_plugin_that_is_only_declared() {
        let found = findings(&probe(
            &[key("n", "<C-H>", r#","callback":true,"origin":"smart-splits.nvim""#)],
            r#""vim-tmux-navigator":"declared","smart-splits.nvim":"loaded""#,
        ));
        assert_eq!(found.nav, NavOwner::SmartSplits);
    }

    #[test]
    fn a_users_own_move_overrides_a_plugin_that_is_present() {
        for state in ["loaded", "declared"] {
            let found = findings(&probe(
                &[key("n", "<C-H>", r#","rhs":"<C-w>h""#)],
                &format!(r#""vim-tmux-navigator":"{state}""#),
            ));
            assert_eq!(
                found.nav,
                NavOwner::UserMapping(Owner::Desc("<C-w>h".into())),
                "{state}"
            );
        }
    }

    #[test]
    fn a_plugin_that_is_present_with_only_placeholders_or_nothing_observed_owns_the_moves() {
        let stub = findings(&probe(
            &[key("n", "<C-H>", r#","callback":true,"origin":"lazy.nvim""#)],
            r#""smart-splits.nvim":"declared""#,
        ));
        assert_eq!(stub.nav, NavOwner::SmartSplits);
    }

    #[test]
    fn a_lazy_placeholder_on_a_move_is_a_lazy_stub() {
        let found = findings(&probe(
            &[key("n", "<C-H>", r#","callback":true,"origin":"lazy.nvim""#)],
            r#""lazy.nvim":"loaded""#,
        ));
        assert_eq!(found.nav, NavOwner::LazyStub);
    }

    #[test]
    fn a_plugin_directory_outside_the_known_list_is_an_origin() {
        let found = findings(&probe(
            &[key("n", "<C-H>", r#","callback":true,"origin":"my-moves""#)],
            "",
        ));
        assert_eq!(found.nav, NavOwner::UserMapping(Owner::Origin("my-moves".into())));
    }

    #[test]
    fn a_chord_a_rescan_asked_for_is_found_as_the_key_it_is() {
        let found = findings(&report(&format!(
            r#"{{"keys":[{}, {}, {}],"plugins":{{}}}}"#,
            key("i", "<C-O>", r#","desc":"mine""#),
            key("n", "<M-a>", r#","desc":"alt""#),
            key("n", "<ScrollWheelUp>", "")
        )));
        let extra: Vec<&ChordOwner> = found
            .chords
            .iter()
            .filter(|c| c.chord == KeySpec::parse("C-o").unwrap() || c.chord == KeySpec::parse("M-a").unwrap())
            .collect();
        assert_eq!(extra.len(), 2, "{:?}", found.chords);
        assert_eq!(extra[0].mode, Mode::Insert);
        assert_eq!(extra[0].owner, Owner::Desc("mine".into()));
        assert_eq!(extra[0].certainty, Certainty::Observed);
        assert_eq!(
            found.chords.len(),
            PROBED_CHORDS.len() * PROBED_MODES.len() + 2,
            "an lhs tmux has no name for is left out"
        );
    }

    #[test]
    fn a_declared_engine_with_no_map_probably_owns_its_probed_insert_chords() {
        let found = findings(&probe(&[], r#""blink.cmp":"declared""#));
        for chord in ["<C-Space>", "<C-b>", "<C-k>"] {
            let owner = chord_in(&found, chord, Mode::Insert).unwrap();
            assert_eq!(
                (owner.owner, owner.certainty),
                (Owner::Plugin(KnownPlugin::BlinkCmp), Certainty::Probably),
                "{chord}"
            );
        }
        assert_eq!(found.pending, vec![KnownPlugin::BlinkCmp]);
        // The engine's chords in other modes are still plain.
        assert_eq!(
            chord_in(&found, "<C-b>", Mode::Normal).unwrap().owner,
            Owner::NvimDefault
        );
    }

    #[test]
    fn nvim_cmp_claims_the_chords_its_distribution_setup_maps() {
        let found = findings(&probe(&[], r#""nvim-cmp":"loaded""#));
        for chord in ["<C-Space>", "<C-b>", "<C-l>", "<C-h>"] {
            assert_eq!(
                chord_in(&found, chord, Mode::Insert).unwrap().owner,
                Owner::Plugin(KnownPlugin::NvimCmp),
                "{chord}"
            );
        }
    }

    #[test]
    fn an_observed_insert_map_replaces_the_inference() {
        let found = findings(&probe(
            &[
                key(
                    "i",
                    "<C-B>",
                    r#","rhs":null,"callback":true,"desc":"mine","origin":"config""#,
                ),
                key("i", "<Nul>", r#","callback":true,"origin":"blink.cmp""#),
            ],
            r#""blink.cmp":"loaded""#,
        ));
        let b = chord_in(&found, "<C-b>", Mode::Insert).unwrap();
        assert_eq!(
            (b.owner, b.certainty),
            (Owner::Origin("config".into()), Certainty::Observed)
        );
        let space = chord_in(&found, "<C-Space>", Mode::Insert).unwrap();
        assert_eq!(
            (space.owner, space.certainty),
            (Owner::Plugin(KnownPlugin::BlinkCmp), Certainty::Observed)
        );
    }

    #[test]
    fn a_buffer_local_map_shadows_the_global_one() {
        let found = findings(&probe(
            &[
                key("i", "<C-B>", r#","desc":"global""#),
                r#"{"mode":"i","lhs":"<C-B>","scope":"buffer","desc":"local"}"#.to_string(),
            ],
            "",
        ));
        assert_eq!(
            chord_in(&found, "<C-b>", Mode::Insert).unwrap().owner,
            Owner::Desc("local".into())
        );
    }

    #[test]
    fn with_an_engine_loaded_or_declared_no_insert_chord_is_free() {
        for state in ["loaded", "declared"] {
            for engine in ["blink.cmp", "nvim-cmp"] {
                let found = findings(&probe(&[], &format!(r#""{engine}":"{state}""#)));
                let free = found
                    .chords
                    .iter()
                    .filter(|c| c.mode == Mode::Insert && c.owner == Owner::NvimDefault)
                    .count();
                assert_eq!(free, 0, "{engine} {state}");
            }
        }
        // Without an engine the same chords are free.
        let bare = findings(&probe(&[], ""));
        assert!(chord_in(&bare, "<C-b>", Mode::Insert).is_some_and(|c| c.owner == Owner::NvimDefault));
    }

    #[test]
    fn the_engine_table_names_probed_chords_and_a_source() {
        for (engine, chords, source) in ENGINE_INSERT_DEFAULTS {
            assert!(matches!(engine, KnownPlugin::BlinkCmp | KnownPlugin::NvimCmp));
            assert!(!source.is_empty());
            let probed = chords.iter().filter(|c| PROBED_CHORDS.contains(c)).count();
            assert!(probed >= 2, "{engine:?} names no probed chords");
        }
    }

    #[test]
    fn a_distribution_and_the_declared_plugins_are_reported() {
        let found = findings(&probe(
            &[],
            r#""LazyVim":"loaded","lazy.nvim":"loaded","smart-splits.nvim":"declared","which-key.nvim":"declared","vim-tmux-navigator":"loaded""#,
        ));
        assert_eq!(found.distro, Some(KnownDistro::LazyVim));
        assert_eq!(found.pending, vec![KnownPlugin::SmartSplits, KnownPlugin::WhichKey]);
        assert_eq!(found.leader, " ");
        assert_eq!(
            findings(&probe(&[], r#""NvChad":"loaded""#)).distro,
            Some(KnownDistro::NvChad)
        );
        assert_eq!(findings(&probe(&[], "")).distro, None);
    }

    #[test]
    fn the_known_names_are_the_probes_closed_list() {
        for plugin in KnownPlugin::ALL {
            assert_eq!(KnownPlugin::from_name(plugin.name()), Some(plugin));
        }
        let mut have: Vec<&str> = KnownPlugin::ALL
            .iter()
            .map(|p| p.name())
            .chain(KnownDistro::ALL.iter().map(|d| d.name()))
            .collect();
        let mut listed: Vec<&str> = KNOWN_PLUGINS.to_vec();
        have.sort_unstable();
        listed.sort_unstable();
        assert_eq!(have, listed);
    }
}
