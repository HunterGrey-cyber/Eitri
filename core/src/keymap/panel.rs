//! The agent panel's own BROWSE key table (spec 2026-09-26 §2): a leader, tab-as-buffer keys, and
//! what `init.lua`'s `"panel"` table and the embedded nvim's mappings do to it (§3.6, Task 3).
//! Keys are vim's `<>` notation (nvim: intro.txt), not tmux's: these are vim keys.

use super::TabAction;
use crate::nvim_keys::{classify, nvim_leader_token, translate_lhs, NvimReport};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelKey {
    Leader,
    Space,
    Char(char), // never ' ' -- a literal space always parses to `Space`
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelSeq(pub Vec<PanelKey>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelAction {
    Tab(TabAction),
    Search,
    Keymap,
    Handoff,
    ModeCycle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelSource {
    Default,
    Nvim,
    User,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelBinding {
    pub seq: PanelSeq,
    pub action: PanelAction,
    pub source: PanelSource,
}

/// Vim's `<>` notation (nvim: intro.txt, `*<>*`). Only the names the panel can act on are
/// recognised; a Ctrl/Alt chord or a special key (`<Tab>`, `<CR>`, `<F1>`, ...) belongs to `shell`'s
/// own accelerators, never to a panel sequence, so it is a parse error here.
pub fn parse_seq(text: &str) -> Result<PanelSeq, String> {
    let mut keys = Vec::new();
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if c == '<' {
            let end = rest.find('>').ok_or_else(|| format!("{text:?}: an unclosed <"))?;
            let name = &rest[1..end];
            keys.push(match name.to_ascii_lowercase().as_str() {
                "leader" => PanelKey::Leader,
                "space" => PanelKey::Space,
                "lt" => PanelKey::Char('<'),
                "bslash" => PanelKey::Char('\\'),
                "bar" => PanelKey::Char('|'),
                _ => {
                    return Err(format!(
                        "{text:?}: <{name}> is not a key the panel can take \
                    (Ctrl/Alt chords and special keys belong to shell)"
                    ))
                }
            });
            rest = &rest[end + 1..];
        } else {
            if c == ' ' {
                keys.push(PanelKey::Space)
            } else {
                keys.push(PanelKey::Char(c))
            }
            rest = &rest[c.len_utf8()..];
        }
    }
    if keys.is_empty() {
        return Err("an empty key sequence".into());
    }
    Ok(PanelSeq(keys))
}

pub fn parse_action(name: &str) -> Result<PanelAction, String> {
    Ok(match name {
        "tab.new" => PanelAction::Tab(TabAction::New),
        "tab.next" => PanelAction::Tab(TabAction::Next),
        "tab.prev" => PanelAction::Tab(TabAction::Prev),
        "tab.last" => PanelAction::Tab(TabAction::Last),
        "tab.close" => PanelAction::Tab(TabAction::Close),
        "tab.close-others" => PanelAction::Tab(TabAction::CloseOthers),
        "tab.choose" => PanelAction::Tab(TabAction::Choose),
        "tab.info" => PanelAction::Tab(TabAction::Info),
        "panel.search" => PanelAction::Search,
        "panel.keymap" => PanelAction::Keymap,
        "panel.handoff" => PanelAction::Handoff,
        "mode.cycle" => PanelAction::ModeCycle,
        other => {
            return Err(format!(
                "{other:?} is not a panel action (init.lua's panel table names)"
            ))
        }
    })
}

impl PanelAction {
    pub fn name(&self) -> &'static str {
        match self {
            PanelAction::Tab(action) => match action {
                TabAction::New => "tab.new",
                TabAction::Next => "tab.next",
                TabAction::Prev => "tab.prev",
                TabAction::Last => "tab.last",
                TabAction::Close => "tab.close",
                TabAction::CloseOthers => "tab.close-others",
                TabAction::Choose => "tab.choose",
                TabAction::Info => "tab.info",
                TabAction::Select(_) | TabAction::Rename => {
                    unreachable!("the panel only uses New Next Prev Last Close CloseOthers Choose Info")
                }
            },
            PanelAction::Search => "panel.search",
            PanelAction::Keymap => "panel.keymap",
            PanelAction::Handoff => "panel.handoff",
            PanelAction::ModeCycle => "mode.cycle",
        }
    }

    pub fn desc(&self) -> &'static str {
        match self {
            PanelAction::Tab(action) => match action {
                TabAction::Prev => "previous tab",
                TabAction::Next => "next tab",
                TabAction::Last => "last tab",
                TabAction::Close => "close tab",
                TabAction::CloseOthers => "close other tabs",
                TabAction::New => "new tab",
                TabAction::Choose => "switch tab",
                TabAction::Info => "tab details",
                TabAction::Select(_) | TabAction::Rename => {
                    unreachable!("the panel only uses New Next Prev Last Close CloseOthers Choose Info")
                }
            },
            PanelAction::Search => "search conversation",
            PanelAction::Keymap => "all keys",
            PanelAction::Handoff => "continue in a terminal",
            PanelAction::ModeCycle => "mode",
        }
    }
}

impl PanelSeq {
    pub fn human(&self, leader_label: &str) -> String {
        self.0
            .iter()
            .map(|k| match k {
                PanelKey::Leader => leader_label.to_string(),
                PanelKey::Space => "Space".to_string(),
                PanelKey::Char(c) => c.to_string(),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn wire(&self) -> Vec<String> {
        self.0
            .iter()
            .map(|k| match k {
                PanelKey::Leader => "<leader>".to_string(),
                PanelKey::Space => " ".to_string(),
                PanelKey::Char(c) => c.to_string(),
            })
            .collect()
    }
}

/// Alone in BROWSE these already mean something (keymap.ts `resolveKey`, spec §2.3).
const FIXED: &str = "jkhladiyYDfrnNG/?0123456789";
const PENDING: &str = "gz[]";
const TAKEN_PAIRS: &[&str] = &["gg", "gf", "zh", "zl", "[[", "]]", "[]", "]["];

/// Why `seq` may never be a panel binding (spec §2.3), or `None` when it is free.
pub fn reserved(seq: &PanelSeq) -> Option<String> {
    let PanelKey::Char(first) = seq.0[0] else { return None };
    if FIXED.contains(first) {
        return Some(format!("{first:?} alone already means something in BROWSE"));
    }
    if PENDING.contains(first) {
        if seq.0.len() != 2 {
            return Some(format!(
                "after {first:?} only one more key fits (it waits for exactly one)"
            ));
        }
        if let PanelKey::Char(second) = seq.0[1] {
            let pair: String = [first, second].iter().collect();
            if TAKEN_PAIRS.contains(&pair.as_str()) {
                return Some(format!("{pair:?} is already a BROWSE key"));
            }
        }
    }
    None
}

/// which-key's `+` groups (`wk: config.lua:125`): `<leader>b` is `+tab` (LazyVim's `+buffer`,
/// `lazyvim: plugins/editor.lua:84-91`, renamed since the thing here is a tab, not a buffer);
/// `<leader>f` is `+new` (LazyVim's `+file/find`, `editor.lua:72`; the only entry here makes a
/// new tab).
pub const DEFAULT_GROUPS: &[(&str, &str)] = &[("<leader>b", "+tab"), ("<leader>f", "+new")];

/// The 13 default rows (spec §2.2), in table order: direct tab-as-buffer keys first, then the
/// `<leader>` rows. Every row's source is the LazyVim/nvim/which-key line it copies, or
/// `neovibe-only` with why (spec, "Rule for every line").
pub fn default_bindings() -> Vec<PanelBinding> {
    vec![
        // lazyvim: config/keymaps.lua:34-35 (<S-h>/<S-l> = bprevious/bnext, "Prev/Next Buffer")
        PanelBinding {
            seq: parse_seq("H").unwrap(),
            action: PanelAction::Tab(TabAction::Prev),
            source: PanelSource::Default,
        },
        PanelBinding {
            seq: parse_seq("L").unwrap(),
            action: PanelAction::Tab(TabAction::Next),
            source: PanelSource::Default,
        },
        // lazyvim: keymaps.lua:36-37; also nvim 0.11+'s own defaults (`nvim --clean`: `[b` desc
        // `:bprevious`, `]b` desc `:bnext`)
        PanelBinding {
            seq: parse_seq("[b").unwrap(),
            action: PanelAction::Tab(TabAction::Prev),
            source: PanelSource::Default,
        },
        PanelBinding {
            seq: parse_seq("]b").unwrap(),
            action: PanelAction::Tab(TabAction::Next),
            source: PanelSource::Default,
        },
        // lazyvim: keymaps.lua:38 ("Switch to Other Buffer")
        PanelBinding {
            seq: parse_seq("<leader>bb").unwrap(),
            action: PanelAction::Tab(TabAction::Last),
            source: PanelSource::Default,
        },
        // lazyvim: keymaps.lua:40-42 ("Delete Buffer")
        PanelBinding {
            seq: parse_seq("<leader>bd").unwrap(),
            action: PanelAction::Tab(TabAction::Close),
            source: PanelSource::Default,
        },
        // lazyvim: keymaps.lua:43-45 ("Delete Other Buffers"); panel round 2 plan's Owner answers
        // Q2: close every other tab, after a y/n prompt naming the count and how many are running.
        PanelBinding {
            seq: parse_seq("<leader>bo").unwrap(),
            action: PanelAction::Tab(TabAction::CloseOthers),
            source: PanelSource::Default,
        },
        // lazyvim: keymaps.lua:98 (<leader>fn = enew, "New File")
        PanelBinding {
            seq: parse_seq("<leader>fn").unwrap(),
            action: PanelAction::Tab(TabAction::New),
            source: PanelSource::Default,
        },
        // LazyVim's <leader>, buffer picker (lazyvim: plugins/extras/editor/{fzf,telescope,snacks_picker}.lua)
        PanelBinding {
            seq: parse_seq("<leader>,").unwrap(),
            action: PanelAction::Tab(TabAction::Choose),
            source: PanelSource::Default,
        },
        // LazyVim's <leader>/ grep (lazyvim: plugins/extras/editor/*), narrowed to what the panel
        // can search
        PanelBinding {
            seq: parse_seq("<leader>/").unwrap(),
            action: PanelAction::Search,
            source: PanelSource::Default,
        },
        // neovibe-only: LazyVim has no session-info key; `i` is `prefix i`'s letter
        PanelBinding {
            seq: parse_seq("<leader>i").unwrap(),
            action: PanelAction::Tab(TabAction::Info),
            source: PanelSource::Default,
        },
        // neovibe-only: the handoff button leaves the bottom (spec §5.4); mock: bottom.html names
        // `Space t`
        PanelBinding {
            seq: parse_seq("<leader>t").unwrap(),
            action: PanelAction::Handoff,
            source: PanelSource::Default,
        },
        // neovibe-only, the same effect as Shift+Tab (Claude Code, docs: permission-modes.md);
        // mock: leader.html draws `m ➜ mode: auto, fixed`
        PanelBinding {
            seq: parse_seq("<leader>m").unwrap(),
            action: PanelAction::ModeCycle,
            source: PanelSource::Default,
        },
        // lazyvim: plugins/editor.lua:105-110 ("Buffer Keymaps (which-key)")
        PanelBinding {
            seq: parse_seq("<leader>?").unwrap(),
            action: PanelAction::Keymap,
            source: PanelSource::Default,
        },
    ]
}

/// `init.lua`'s recorded `neovibe.keymap.set("panel", ...)`/`.del("panel", ...)` calls (spec §3.6,
/// Task 3): applied over the defaults and, before this, over nvim's own mappings in [`effective`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PanelUserTable {
    pub sets: Vec<PanelBinding>,
    pub dels: Vec<PanelSeq>,
}

impl PanelUserTable {
    /// `neovibe.keymap.set("panel", key, action)`: fails naming the call's own key/action text when
    /// the key does not parse, is one of §2.3's reserved BROWSE keys, the action name is unknown, or
    /// the sequence is already bound (by a default not yet `del`'d, or by an earlier `set`) --
    /// `table.rs` wraps the message as `KeymapError::Panel`.
    pub fn set(&mut self, key: &str, action: &str) -> Result<(), String> {
        let seq = parse_seq(key)?;
        if let Some(why) = reserved(&seq) {
            return Err(why);
        }
        let action = parse_action(action)?;
        let human = seq.human("<leader>");
        if let Some(default) = default_bindings()
            .into_iter()
            .find(|b| b.seq == seq && !self.dels.contains(&b.seq))
        {
            return Err(format!(
                "{human} is already bound to {} (default); neovibe.keymap.del(\"panel\", {key:?}) first",
                default.action.name()
            ));
        }
        if let Some(existing) = self.sets.iter().find(|b| b.seq == seq) {
            return Err(format!(
                "{human} is already bound to {} (set earlier in init.lua); neovibe.keymap.del(\"panel\", {key:?}) first",
                existing.action.name()
            ));
        }
        self.sets.push(PanelBinding {
            seq,
            action,
            source: PanelSource::User,
        });
        Ok(())
    }

    /// `neovibe.keymap.del("panel", key)`: removes an earlier `set`, or records a deletion of a
    /// default; `"nothing binds {key}"` when neither binds it.
    pub fn del(&mut self, key: &str) -> Result<(), String> {
        let seq = parse_seq(key)?;
        if let Some(at) = self.sets.iter().position(|b| b.seq == seq) {
            self.sets.remove(at);
            self.dels.push(seq);
            return Ok(());
        }
        if default_bindings().iter().any(|b| b.seq == seq) {
            self.dels.push(seq);
            return Ok(());
        }
        Err(format!("nothing binds {key}"))
    }
}

/// Where the panel's leader key came from (spec §2.1, decision 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaderSource {
    /// No nvim report has arrived yet.
    Default,
    /// nvim's `g:mapleader`, usable as the panel's leader.
    Mapleader,
    /// nvim reported, but `g:mapleader` is unset or empty.
    Unset,
    /// nvim reported a `g:mapleader` the panel cannot lead with (spec Review Focus 4).
    Unusable,
}

/// The merged table `shell` ships in the `keymap` envelope (Task 6 sends it only on change, hence
/// `PartialEq`).
#[derive(Debug, Clone, PartialEq)]
pub struct PanelKeymap {
    pub leader: PanelKey,
    pub leader_label: String,
    pub leader_source: LeaderSource,
    pub timeoutlen_ms: u32,
    pub timeout: bool,
    pub bindings: Vec<PanelBinding>,
}

/// nvim's `mapleader`, already `keytrans`'d (spec §2.1, decision 5): no report yet is `Default`; a
/// report with no `mapleader` is `Unset`; one key, `Space` or a `Char` free of §2.3's reservations,
/// is `Mapleader`; anything else -- more than one key, an unparsable token, a reserved key -- falls
/// back to Space as `Unusable` and is logged (spec Review Focus 4).
fn leader_of(report: Option<&NvimReport>, log: &mut Vec<String>) -> (PanelKey, String, LeaderSource) {
    let Some(report) = report else {
        return (PanelKey::Space, "Space".to_string(), LeaderSource::Default);
    };
    let Some(raw) = report.mapleader.as_deref() else {
        return (PanelKey::Space, "Space".to_string(), LeaderSource::Unset);
    };
    let usable = parse_seq(raw)
        .ok()
        .filter(|seq| seq.0.len() == 1)
        .and_then(|seq| match seq.0[0] {
            PanelKey::Space => Some((PanelKey::Space, "Space".to_string())),
            PanelKey::Char(c) if !FIXED.contains(c) && !PENDING.contains(c) => Some((PanelKey::Char(c), c.to_string())),
            _ => None,
        });
    match usable {
        Some((key, label)) => (key, label, LeaderSource::Mapleader),
        None => {
            log.push(format!(
                "[nvim-keys] mapleader {raw:?} cannot lead in the panel; using Space"
            ));
            (PanelKey::Space, "Space".to_string(), LeaderSource::Unusable)
        }
    }
}

/// Merges the defaults, nvim's own mappings and `init.lua`'s `"panel"` table (spec §3.6): defaults <
/// nvim < `init.lua`; an unrecognised nvim mapping never shadows anything (ruling R3); an `init.lua`
/// `del` suppresses the sequence at every level, nvim's included; an `init.lua` `set` on a sequence
/// nvim also maps wins and is logged. Returns the merged table plus every log line (dropped nvim
/// mappings, an unusable leader, an `init.lua` override), for `shell` to print.
pub fn effective(user: &PanelUserTable, report: Option<&NvimReport>) -> (PanelKeymap, Vec<String>) {
    let mut log = Vec::new();
    let (leader, leader_label, leader_source) = leader_of(report, &mut log);
    let mut bindings: Vec<PanelBinding> = default_bindings()
        .into_iter()
        .filter(|b| !user.dels.contains(&b.seq))
        .collect();
    if let Some(report) = report {
        let nvim_leader = nvim_leader_token(report);
        for map in &report.maps {
            let Some(action) = classify(map) else { continue };
            let seq = match translate_lhs(&map.lhs, nvim_leader) {
                Ok(seq) => seq,
                Err(why) => {
                    log.push(format!("[nvim-keys] {}: {why}", map.lhs));
                    continue;
                }
            };
            if let Some(why) = reserved(&seq) {
                log.push(format!("[nvim-keys] {}: {why}", map.lhs));
                continue;
            }
            if user.dels.contains(&seq) {
                continue;
            }
            if user.sets.iter().any(|b| b.seq == seq) {
                log.push(format!("[nvim-keys] {}: init.lua binds it; init.lua wins", map.lhs));
                continue;
            }
            let binding = PanelBinding {
                seq,
                action: PanelAction::Tab(action),
                source: PanelSource::Nvim,
            };
            match bindings.iter_mut().find(|b| b.seq == binding.seq) {
                Some(slot) => *slot = binding,
                None => bindings.push(binding),
            }
        }
    }
    bindings.extend(user.sets.iter().cloned());
    let (timeoutlen_ms, timeout) = report.map_or((1000, true), |r| (r.timeoutlen, r.timeout));
    (
        PanelKeymap {
            leader,
            leader_label,
            leader_source,
            timeoutlen_ms,
            timeout,
            bindings,
        },
        log,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vim_notation_parses_and_round_trips() {
        let seq = parse_seq("<leader>bd").unwrap();
        assert_eq!(seq.0, vec![PanelKey::Leader, PanelKey::Char('b'), PanelKey::Char('d')]);
        assert_eq!(seq.wire(), vec!["<leader>", "b", "d"]);
        assert_eq!(seq.human("Space"), "Space b d");
        assert_eq!(
            parse_seq("<Space>x").unwrap().0,
            vec![PanelKey::Space, PanelKey::Char('x')]
        );
        assert_eq!(
            parse_seq("<lt><Bslash><Bar>").unwrap().0,
            vec![PanelKey::Char('<'), PanelKey::Char('\\'), PanelKey::Char('|')]
        );
        assert_eq!(parse_seq("<LEADER>fn").unwrap().0[0], PanelKey::Leader); // case-insensitive, as vim's <>
        for bad in ["", "<C-a>", "<Tab>", "<CR>", "<F1>", "<nope>", "a<"] {
            assert!(parse_seq(bad).is_err(), "{bad:?} must not parse");
        }
    }

    #[test]
    fn the_fixed_browse_keys_are_reserved() {
        for key in [
            "j", "k", "h", "l", "a", "d", "i", "y", "Y", "D", "f", "r", "n", "N", "G", "/", "?", "5",
        ] {
            assert!(reserved(&parse_seq(key).unwrap()).is_some(), "{key} alone");
            assert!(reserved(&parse_seq(&format!("{key}x")).unwrap()).is_some(), "{key}x");
        }
        for taken in ["gg", "gf", "zh", "zl", "[[", "]]", "[]", "]["] {
            assert!(reserved(&parse_seq(taken).unwrap()).is_some(), "{taken}");
        }
        assert!(reserved(&parse_seq("g").unwrap()).is_some(), "a lone pending key");
        assert!(reserved(&parse_seq("[bx").unwrap()).is_some(), "three keys after [");
        for free in ["[b", "]b", "H", "L", "<leader>bd", "<Space>x", "gt", "zb"] {
            assert_eq!(reserved(&parse_seq(free).unwrap()), None, "{free}");
        }
    }

    #[test]
    fn the_default_table_is_the_specs() {
        let rows: Vec<(String, &str)> = default_bindings()
            .iter()
            .map(|b| (b.seq.human("Space"), b.action.name()))
            .collect();
        let expect = [
            ("H", "tab.prev"),
            ("L", "tab.next"),
            ("[ b", "tab.prev"),
            ("] b", "tab.next"),
            ("Space b b", "tab.last"),
            ("Space b d", "tab.close"),
            ("Space b o", "tab.close-others"),
            ("Space f n", "tab.new"),
            ("Space ,", "tab.choose"),
            ("Space /", "panel.search"),
            ("Space i", "tab.info"),
            ("Space t", "panel.handoff"),
            ("Space m", "mode.cycle"),
            ("Space ?", "panel.keymap"),
        ];
        assert_eq!(
            rows,
            expect.iter().map(|(k, a)| (k.to_string(), *a)).collect::<Vec<_>>()
        );
        assert!(default_bindings().iter().all(|b| reserved(&b.seq).is_none()));
    }

    #[test]
    fn actions_parse_by_their_init_lua_names() {
        for name in [
            "tab.new",
            "tab.next",
            "tab.prev",
            "tab.last",
            "tab.close",
            "tab.close-others",
            "tab.choose",
            "tab.info",
            "panel.search",
            "panel.keymap",
            "panel.handoff",
            "mode.cycle",
        ] {
            assert_eq!(parse_action(name).unwrap().name(), name);
        }
        assert!(parse_action("tab.rename").is_err());
    }

    fn report(leader: Option<&str>, maps: &[(&str, Option<&str>, Option<&str>)]) -> NvimReport {
        NvimReport {
            v: 1,
            mapleader: leader.map(Into::into),
            timeoutlen: 300,
            timeout: true,
            maps: maps
                .iter()
                .map(|(l, r, d)| crate::nvim_keys::NvimMap {
                    lhs: l.to_string(),
                    rhs: r.map(Into::into),
                    desc: d.map(Into::into),
                    callback: r.is_none(),
                })
                .collect(),
        }
    }
    fn find<'a>(k: &'a PanelKeymap, human: &str) -> Option<&'a PanelBinding> {
        k.bindings.iter().find(|b| b.seq.human("Space") == human)
    }

    #[test]
    fn with_no_report_it_is_the_defaults_under_space() {
        let (k, _) = effective(&PanelUserTable::default(), None);
        assert_eq!(
            (k.leader, k.leader_source, k.timeoutlen_ms, k.timeout),
            (PanelKey::Space, LeaderSource::Default, 1000, true)
        );
        assert_eq!(k.bindings.len(), default_bindings().len());
    }

    #[test]
    fn lazyvim_adds_only_its_second_close_key() {
        let r = report(
            Some("<Space>"),
            &[
                ("H", Some("<Cmd>bprevious<CR>"), Some("Prev Buffer")),
                ("<Space>bd", None, Some("Delete Buffer")),
                ("<Space>bD", Some("<Cmd>:bd<CR>"), Some("Delete Buffer and Window")),
                ("<Space>fn", Some("<Cmd>enew<CR>"), Some("New File")),
                ("<Space>/", None, Some("Grep (Root Dir)")), // unrecognised: shadows nothing
            ],
        );
        let (k, _) = effective(&PanelUserTable::default(), Some(&r));
        assert_eq!(k.bindings.len(), default_bindings().len() + 1);
        assert_eq!(find(&k, "Space b D").unwrap().action.name(), "tab.close");
        assert_eq!(find(&k, "Space /").unwrap().action.name(), "panel.search");
        assert_eq!(find(&k, "H").unwrap().source, PanelSource::Nvim);
    }

    #[test]
    fn nvim_replaces_a_default_on_the_same_keys() {
        let r = report(Some("<Space>"), &[("<Space>bb", Some(":bnext<CR>"), None)]);
        let (k, _) = effective(&PanelUserTable::default(), Some(&r));
        assert_eq!(find(&k, "Space b b").unwrap().action.name(), "tab.next");
    }

    #[test]
    fn init_lua_wins_and_its_del_suppresses_nvim() {
        let user = PanelUserTable {
            sets: vec![PanelBinding {
                seq: parse_seq("gt").unwrap(),
                action: parse_action("tab.last").unwrap(),
                source: PanelSource::User,
            }],
            dels: vec![parse_seq("H").unwrap()],
        };
        let r = report(
            Some("<Space>"),
            &[("H", Some(":bp<CR>"), None), ("gt", Some(":bn<CR>"), None)],
        );
        let (k, log) = effective(&user, Some(&r));
        assert!(find(&k, "H").is_none(), "del removes nvim's H too");
        assert_eq!(find(&k, "g t").unwrap().action.name(), "tab.last");
        assert!(log.iter().any(|l| l.contains("init.lua")), "{log:?}");
    }

    #[test]
    fn a_reserved_or_untranslatable_nvim_key_is_dropped_and_logged() {
        let r = report(
            Some("<Space>"),
            &[("j", Some(":bn<CR>"), None), ("<C-n>", Some(":bn<CR>"), None)],
        );
        let (k, log) = effective(&PanelUserTable::default(), Some(&r));
        assert_eq!(k.bindings.len(), default_bindings().len());
        assert_eq!(log.len(), 2, "{log:?}");
    }

    #[test]
    fn an_unusable_leader_falls_back_to_space_but_its_mappings_still_count() {
        for bad in ["j", "<C-a>", ",,", "g"] {
            let r = report(Some(bad), &[]);
            let (k, _) = effective(&PanelUserTable::default(), Some(&r));
            assert_eq!(
                (k.leader, k.leader_source),
                (PanelKey::Space, LeaderSource::Unusable),
                "{bad}"
            );
        }
        let r = report(Some(","), &[(",x", Some(":bn<CR>"), None)]);
        let (k, _) = effective(&PanelUserTable::default(), Some(&r));
        assert_eq!((k.leader, k.leader_label.as_str()), (PanelKey::Char(','), ","));
        assert!(k
            .bindings
            .iter()
            .any(|b| b.seq.0 == vec![PanelKey::Leader, PanelKey::Char('x')]));
        let unset = report(None, &[]);
        assert_eq!(
            effective(&PanelUserTable::default(), Some(&unset)).0.leader_source,
            LeaderSource::Unset
        );
    }

    #[test]
    fn an_ambiguous_node_is_kept() {
        let r = report(Some("<Space>"), &[("<Space>b", Some(":bn<CR>"), None)]);
        let (k, _) = effective(&PanelUserTable::default(), Some(&r));
        assert!(find(&k, "Space b").is_some() && find(&k, "Space b d").is_some());
    }
}
