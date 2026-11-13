//! The agent panel's own BROWSE key table: a leader, tab-as-buffer keys, and
//! what `init.lua`'s `"panel"` table and the embedded nvim's mappings do to it.
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
    Motion(PanelMotion),
    Tab(TabAction),
    Search,
    Keymap,
    Handoff,
    ModeCycle,
    ModelPick,
    EffortPick,
    /// Stop the commands the tab's agent left running after its turns ended (by stopping its session),
    /// after a y/n question.
    StopBackground,
    Follow,
    /// The remote-control device list (only offered while remote control is on in the window).
    RemoteDevices,
    /// The person's own move into or out of Claude's plan mode (`/plan` too).
    PlanToggle,
}

/// BROWSE actions resolved as built-in motions, preserving counts and key repeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelMotion {
    RowDown,
    RowUp,
    ItemNext,
    ItemPrev,
    LineDown,
    LineUp,
    Top,
    Bottom,
    PromptNext,
    PromptPrev,
    ToolNext,
    ToolPrev,
    ErrorNext,
    ErrorPrev,
    EditNext,
    EditPrev,
    HunkNext,
    HunkPrev,
    SearchForward,
    SearchBackward,
    SearchNext,
    SearchPrev,
    JumpOlder,
    JumpNewer,
    DetailedToggle,
    Help,
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
                keys.push(PanelKey::Space);
            } else {
                keys.push(PanelKey::Char(c));
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
        "row.down" => PanelAction::Motion(PanelMotion::RowDown),
        "row.up" => PanelAction::Motion(PanelMotion::RowUp),
        "item.next" => PanelAction::Motion(PanelMotion::ItemNext),
        "item.prev" => PanelAction::Motion(PanelMotion::ItemPrev),
        "line.down" => PanelAction::Motion(PanelMotion::LineDown),
        "line.up" => PanelAction::Motion(PanelMotion::LineUp),
        "top" => PanelAction::Motion(PanelMotion::Top),
        "bottom" => PanelAction::Motion(PanelMotion::Bottom),
        "prompt.next" => PanelAction::Motion(PanelMotion::PromptNext),
        "prompt.prev" => PanelAction::Motion(PanelMotion::PromptPrev),
        "tool.next" => PanelAction::Motion(PanelMotion::ToolNext),
        "tool.prev" => PanelAction::Motion(PanelMotion::ToolPrev),
        "error.next" => PanelAction::Motion(PanelMotion::ErrorNext),
        "error.prev" => PanelAction::Motion(PanelMotion::ErrorPrev),
        "edit.next" => PanelAction::Motion(PanelMotion::EditNext),
        "edit.prev" => PanelAction::Motion(PanelMotion::EditPrev),
        "hunk.next" => PanelAction::Motion(PanelMotion::HunkNext),
        "hunk.prev" => PanelAction::Motion(PanelMotion::HunkPrev),
        "search.forward" => PanelAction::Motion(PanelMotion::SearchForward),
        "search.backward" => PanelAction::Motion(PanelMotion::SearchBackward),
        "search.next" => PanelAction::Motion(PanelMotion::SearchNext),
        "search.prev" => PanelAction::Motion(PanelMotion::SearchPrev),
        "jump.older" => PanelAction::Motion(PanelMotion::JumpOlder),
        "jump.newer" => PanelAction::Motion(PanelMotion::JumpNewer),
        "detailed.toggle" => PanelAction::Motion(PanelMotion::DetailedToggle),
        "help" => PanelAction::Motion(PanelMotion::Help),
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
        "model.pick" => PanelAction::ModelPick,
        "effort.pick" => PanelAction::EffortPick,
        "tab.stop-background" => PanelAction::StopBackground,
        "tab.follow" => PanelAction::Follow,
        "remote.devices" => PanelAction::RemoteDevices,
        "plan.toggle" => PanelAction::PlanToggle,
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
            PanelAction::Motion(motion) => match motion {
                PanelMotion::RowDown => "row.down",
                PanelMotion::RowUp => "row.up",
                PanelMotion::ItemNext => "item.next",
                PanelMotion::ItemPrev => "item.prev",
                PanelMotion::LineDown => "line.down",
                PanelMotion::LineUp => "line.up",
                PanelMotion::Top => "top",
                PanelMotion::Bottom => "bottom",
                PanelMotion::PromptNext => "prompt.next",
                PanelMotion::PromptPrev => "prompt.prev",
                PanelMotion::ToolNext => "tool.next",
                PanelMotion::ToolPrev => "tool.prev",
                PanelMotion::ErrorNext => "error.next",
                PanelMotion::ErrorPrev => "error.prev",
                PanelMotion::EditNext => "edit.next",
                PanelMotion::EditPrev => "edit.prev",
                PanelMotion::HunkNext => "hunk.next",
                PanelMotion::HunkPrev => "hunk.prev",
                PanelMotion::SearchForward => "search.forward",
                PanelMotion::SearchBackward => "search.backward",
                PanelMotion::SearchNext => "search.next",
                PanelMotion::SearchPrev => "search.prev",
                PanelMotion::JumpOlder => "jump.older",
                PanelMotion::JumpNewer => "jump.newer",
                PanelMotion::DetailedToggle => "detailed.toggle",
                PanelMotion::Help => "help",
            },
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
            PanelAction::ModelPick => "model.pick",
            PanelAction::EffortPick => "effort.pick",
            PanelAction::StopBackground => "tab.stop-background",
            PanelAction::Follow => "tab.follow",
            PanelAction::RemoteDevices => "remote.devices",
            PanelAction::PlanToggle => "plan.toggle",
        }
    }

    pub fn desc(&self) -> &'static str {
        match self {
            PanelAction::Motion(motion) => match motion {
                PanelMotion::RowDown => "next row",
                PanelMotion::RowUp => "previous row",
                PanelMotion::ItemNext => "next row start",
                PanelMotion::ItemPrev => "previous row start",
                PanelMotion::LineDown => "one line down",
                PanelMotion::LineUp => "one line up",
                PanelMotion::Top => "first row",
                PanelMotion::Bottom => "last row",
                PanelMotion::PromptNext => "next prompt",
                PanelMotion::PromptPrev => "previous prompt",
                PanelMotion::ToolNext => "next tool call",
                PanelMotion::ToolPrev => "previous tool call",
                PanelMotion::ErrorNext => "next error",
                PanelMotion::ErrorPrev => "previous error",
                PanelMotion::EditNext => "next edit",
                PanelMotion::EditPrev => "previous edit",
                PanelMotion::HunkNext => "next hunk",
                PanelMotion::HunkPrev => "previous hunk",
                PanelMotion::SearchForward => "search forward",
                PanelMotion::SearchBackward => "search backward",
                PanelMotion::SearchNext => "next search match",
                PanelMotion::SearchPrev => "previous search match",
                PanelMotion::JumpOlder => "older jump",
                PanelMotion::JumpNewer => "newer jump",
                PanelMotion::DetailedToggle => "detailed view",
                PanelMotion::Help => "all keys",
            },
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
            PanelAction::ModelPick => "model",
            PanelAction::EffortPick => "effort",
            PanelAction::StopBackground => "stop background work",
            PanelAction::Follow => "follow in the editor",
            PanelAction::RemoteDevices => "phones (remote control)",
            PanelAction::PlanToggle => "plan mode",
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

/// For non-motion bindings, alone in BROWSE these already mean something (keymap.ts `resolveKey`). `o` and `A` enter INPUT
/// like `i`, so a panel binding or nvim `mapleader`/mapping must not shadow them. `:` opens the
/// panel's own command line and is protected the same way. `{` and `}` are reserved ahead of
/// their BROWSE bindings so a user mapping cannot claim them first.
const FIXED: &str = "jkhladioAyYDfrnNG/?:0123456789{}";
const PENDING: &str = "gz[]";
/// The BROWSE pairs a reserved prefix already completes, or will (the web side's `FIXED_PAIRS`):
/// `gx`, `zz`/`zt`/`zb`, `za`/`zo`/`zc`, `[p`/`]p`, `g?`/`gd`/`gy`, `zi`,
/// `[t`/`]t`, `[c`/`]c`, `[d`/`]d` and `[m`/`]m` are reserved ahead of the keys that bind them,
/// so a table binding can never claim one first and change its meaning later.
const TAKEN_PAIRS: &[&str] = &[
    "gg", "gf", "gx", "zh", "zl", "zz", "zt", "zb", "za", "zo", "zc", "[[", "]]", "[]", "][", "[p", "]p", "g?", "gd",
    "gy", "zi", "]t", "[t", "]c", "[c", "]d", "[d", "]m", "[m",
];

/// Why `seq` may never be a non-motion panel binding, or `None` when it is free.
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
/// `<leader>r` is `+remote`, the remote-control keys (an Eitri-only group).
pub const DEFAULT_GROUPS: &[(&str, &str)] = &[("<leader>b", "+tab"), ("<leader>f", "+new"), ("<leader>r", "+remote")];

/// The 22 default rows, in table order: direct
/// tab-as-buffer keys first, then the `<leader>` rows. Every row's source is the
/// LazyVim/nvim/which-key line it copies, or `eitri-only` with why, so no row is invented.
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
        // vim: `:help gt` / `:help gT` (next/previous tab page). A vim user presses them expecting the
        // next tab; a session tab is the panel's tab page, so they step like
        // `L`/`H`. `g` is a reserved prefix, so these reach the table through `resolveKey`'s pending
        // pair lookup, the same way `[b`/`]b` do.
        PanelBinding {
            seq: parse_seq("gt").unwrap(),
            action: PanelAction::Tab(TabAction::Next),
            source: PanelSource::Default,
        },
        PanelBinding {
            seq: parse_seq("gT").unwrap(),
            action: PanelAction::Tab(TabAction::Prev),
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
        // lazyvim: keymaps.lua:43-45 ("Delete Other Buffers"): close every other tab, after a y/n
        // prompt naming the count and how many are running.
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
        // eitri-only: LazyVim has no session-info key; `i` is `prefix i`'s letter
        PanelBinding {
            seq: parse_seq("<leader>i").unwrap(),
            action: PanelAction::Tab(TabAction::Info),
            source: PanelSource::Default,
        },
        // eitri-only: handoff is on the leader key `Space t` rather than a bottom-bar button.
        PanelBinding {
            seq: parse_seq("<leader>t").unwrap(),
            action: PanelAction::Handoff,
            source: PanelSource::Default,
        },
        // eitri-only: cycle the permission mode, like Shift+Tab.
        PanelBinding {
            seq: parse_seq("<leader>m").unwrap(),
            action: PanelAction::ModeCycle,
            source: PanelSource::Default,
        },
        // eitri-only: model uses capital M because lowercase m stays the mode key.
        PanelBinding {
            seq: parse_seq("<leader>M").unwrap(),
            action: PanelAction::ModelPick,
            source: PanelSource::Default,
        },
        // eitri-only: effort uses capital E because lowercase e stays the following key.
        PanelBinding {
            seq: parse_seq("<leader>E").unwrap(),
            action: PanelAction::EffortPick,
            source: PanelSource::Default,
        },
        // eitri-only: LazyVim binds no `<leader>k`; `k` as in tmux's kill-* verbs. It asks y/n first, since
        // what it stops is the whole session.
        PanelBinding {
            seq: parse_seq("<leader>k").unwrap(),
            action: PanelAction::StopBackground,
            source: PanelSource::Default,
        },
        // eitri-only: Zed's agent panel has such a toggle; e as in editor
        PanelBinding {
            seq: parse_seq("<leader>e").unwrap(),
            action: PanelAction::Follow,
            source: PanelSource::Default,
        },
        // eitri-only: LazyVim binds no `<leader>r` group; `r` as in remote, `d` as in devices. The page offers it only
        // while remote control is on in the window.
        PanelBinding {
            seq: parse_seq("<leader>rd").unwrap(),
            action: PanelAction::RemoteDevices,
            source: PanelSource::Default,
        },
        // eitri-only: LazyVim binds no `<leader>p`; p as in plan, Claude Code's `/plan` (its Shift+Tab stop is the
        // mode key here, which cycles Eitri's own modes).
        PanelBinding {
            seq: parse_seq("<leader>p").unwrap(),
            action: PanelAction::PlanToggle,
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

/// `init.lua`'s recorded `eitri.keymap.set("panel", ...)`/`.del("panel", ...)` calls: applied over
/// the defaults and, before this, over nvim's own mappings in [`effective`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PanelUserTable {
    pub sets: Vec<PanelBinding>,
    pub dels: Vec<PanelSeq>,
    /// The defaults a `set` took over, as `(the key as the call wrote it, the action it replaced)`: said once at
    /// startup, since the person may not know the key was taken.
    pub overrides: Vec<(String, String)>,
}

impl PanelUserTable {
    /// `eitri.keymap.set("panel", key, action)`: fails naming the call's own key/action text when
    /// the key does not parse, is one of the reserved BROWSE keys, or the action name is unknown --
    /// `table.rs` wraps the message as `KeymapError::Panel`. A key a default or an earlier `set` already
    /// binds is the call's: the last `set` wins, as vim's maps do, and a default it takes over is
    /// recorded in `overrides`.
    pub fn set(&mut self, key: &str, action: &str) -> Result<(), String> {
        let action = parse_action(action)?;
        let seq = parse_seq(key)?;
        if matches!(action, PanelAction::Motion(_)) {
            if matches!(seq.0[0], PanelKey::Char(c) if c.is_ascii_digit()) {
                return Err("a motion sequence must not start with a digit (BROWSE counts)".into());
            }
            if !matches!(seq.0.as_slice(), [PanelKey::Char(_)] | [PanelKey::Space])
                && !matches!(
                    seq.0.as_slice(),
                    [
                        PanelKey::Char('g' | 'z' | '[' | ']'),
                        PanelKey::Char(_) | PanelKey::Space
                    ]
                )
            {
                return Err("a motion requires one key or a two-key sequence starting with g z [ ]".into());
            }
            if matches!(seq.0.as_slice(), [PanelKey::Char('i' | 'o' | 'A' | ':' | 'y' | 'Y')]) {
                return Err("a motion must not take i o A : y Y (input, command line or copy operator)".into());
            }
            if matches!(seq.0.as_slice(), [PanelKey::Char('g' | 'z' | '[' | ']')]) {
                return Err("a motion must not take g z [ ] (they start pairs; bind a pair instead)".into());
            }
        } else if let Some(why) = reserved(&seq) {
            return Err(why);
        }
        self.sets.retain(|b| b.seq != seq);
        self.overrides
            .retain(|(taken, _)| parse_seq(taken).ok() != Some(seq.clone()));
        if let Some(default) = default_bindings()
            .into_iter()
            .find(|b| b.seq == seq && !self.dels.contains(&b.seq))
        {
            self.overrides
                .push((key.to_string(), default.action.name().to_string()));
        }
        self.sets.push(PanelBinding {
            seq,
            action,
            source: PanelSource::User,
        });
        Ok(())
    }

    /// `eitri.keymap.del("panel", key)`: removes an earlier `set`, or records a deletion of a
    /// default; `"nothing binds {key}"` when neither binds it.
    pub fn del(&mut self, key: &str) -> Result<(), String> {
        let seq = parse_seq(key)?;
        if let Some(at) = self.sets.iter().position(|b| b.seq == seq) {
            self.sets.remove(at);
            self.overrides
                .retain(|(taken, _)| parse_seq(taken).ok() != Some(seq.clone()));
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

/// Where the panel's leader key came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaderSource {
    /// No nvim report has arrived yet.
    Default,
    /// nvim's `g:mapleader`, usable as the panel's leader.
    Mapleader,
    /// nvim reported, but `g:mapleader` is unset or empty.
    Unset,
    /// nvim reported a `g:mapleader` the panel cannot lead with.
    Unusable,
}

/// The merged table `shell` ships in the `keymap` envelope (sent only on change, hence
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

/// nvim's `mapleader`, already `keytrans`'d: no report yet is `Default`; a
/// report with no `mapleader` is `Unset`; one key, `Space` or a `Char` free of the reservations,
/// is `Mapleader`; anything else -- more than one key, an unparsable token, a reserved key -- falls
/// back to Space as `Unusable` and is logged.
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

/// Merges the defaults, nvim's own mappings and `init.lua`'s `"panel"` table: defaults <
/// nvim < `init.lua`; an unrecognised nvim mapping never shadows anything; an `init.lua`
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
    // An `init.lua` `set` replaces whatever is left on its keys (a default, or nvim's). Keep one
    // row per sequence even when the table was built without `set`, since the panel uses the first match.
    bindings.retain(|b| !user.sets.iter().any(|s| s.seq == b.seq));
    bindings.extend(user.sets.iter().cloned());
    // The leader is known only after nvim reports. A motion must leave its commands reachable.
    bindings.retain(|b| {
        if matches!(b.action, PanelAction::Motion(_)) && b.seq.0.as_slice() == [leader] {
            log.push(format!(
                "[keys] {}: the panel leader; a motion cannot take it",
                b.seq.human(&leader_label)
            ));
            return false;
        }
        true
    });
    // A single-key motion runs immediately, so it cannot also start another surviving binding.
    let motion_prefixes: Vec<PanelSeq> = bindings
        .iter()
        .filter(|b| matches!(b.action, PanelAction::Motion(_)) && b.seq.0.len() == 1)
        .filter(|b| {
            bindings
                .iter()
                .any(|other| other.seq.0.len() > 1 && other.seq.0.starts_with(&b.seq.0))
        })
        .map(|b| b.seq.clone())
        .collect();
    bindings.retain(|b| {
        if motion_prefixes.contains(&b.seq) {
            log.push(format!(
                "[keys] {}: starts another binding; a motion cannot take it (bind a pair instead)",
                b.seq.human(&leader_label)
            ));
            return false;
        }
        true
    });
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
    fn plan_toggle_is_leader_p_by_default_and_named_plan_toggle() {
        let row = default_bindings()
            .into_iter()
            .find(|b| b.action == PanelAction::PlanToggle)
            .expect("a default row for plan mode");
        assert_eq!(row.seq, parse_seq("<leader>p").unwrap());
        assert_eq!(parse_action("plan.toggle").unwrap(), PanelAction::PlanToggle);
        assert_eq!(PanelAction::PlanToggle.name(), "plan.toggle");
        assert_eq!(PanelAction::PlanToggle.desc(), "plan mode");
        // No other default row starts with `<leader>p`, so the key runs at once.
        let others = default_bindings()
            .into_iter()
            .filter(|b| b.seq.0.starts_with(&[PanelKey::Leader, PanelKey::Char('p')]))
            .count();
        assert_eq!(others, 1);
    }

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
        // `o` aliases `i`, and `A` opens INPUT with the caret at the end. Both are fixed BROWSE keys,
        // so Lua bindings and nvim mappings must not shadow them. `:` opens the panel command line
        // and must be protected the same way.
        for key in [
            "j", "k", "h", "l", "a", "d", "i", "o", "A", "y", "Y", "D", "f", "r", "n", "N", "G", "/", "?", ":", "5",
            "{", "}",
        ] {
            assert!(reserved(&parse_seq(key).unwrap()).is_some(), "{key} alone");
            assert!(reserved(&parse_seq(&format!("{key}x")).unwrap()).is_some(), "{key}x");
        }
        for taken in [
            "gg", "gf", "zh", "zl", "[[", "]]", "[]", "][", "gx", "zz", "zt", "zb", "za", "zo", "zc", "[p", "]p", "g?",
            "gd", "gy", "zi", "]t", "[t", "]c", "[c", "]d", "[d", "]m", "[m",
        ] {
            assert!(reserved(&parse_seq(taken).unwrap()).is_some(), "{taken}");
        }
        assert!(reserved(&parse_seq("g").unwrap()).is_some(), "a lone pending key");
        assert!(reserved(&parse_seq("[bx").unwrap()).is_some(), "three keys after [");
        for free in ["[b", "]b", "H", "L", "<leader>bd", "<Space>x", "gt", "gT", "zq"] {
            assert_eq!(reserved(&parse_seq(free).unwrap()), None, "{free}");
        }
    }

    /// The public API used by `eitri.keymap.set("panel", "o", ...)` must refuse both `o` and `A`
    /// so a user binding cannot silently shadow a fixed BROWSE key.
    #[test]
    fn o_and_a_are_refused_by_set_the_same_way_i_is() {
        let mut table = PanelUserTable::default();
        assert!(table.set("o", "tab.next").is_err(), "o");
        assert!(table.set("A", "tab.next").is_err(), "A");
        assert!(table.set("i", "tab.next").is_err(), "i, for comparison");
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
            ("g t", "tab.next"),
            ("g T", "tab.prev"),
            ("Space b b", "tab.last"),
            ("Space b d", "tab.close"),
            ("Space b o", "tab.close-others"),
            ("Space f n", "tab.new"),
            ("Space ,", "tab.choose"),
            ("Space /", "panel.search"),
            ("Space i", "tab.info"),
            ("Space t", "panel.handoff"),
            ("Space m", "mode.cycle"),
            ("Space M", "model.pick"),
            ("Space E", "effort.pick"),
            ("Space k", "tab.stop-background"),
            ("Space e", "tab.follow"),
            ("Space r d", "remote.devices"),
            ("Space p", "plan.toggle"),
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
            "model.pick",
            "effort.pick",
            "tab.stop-background",
            "tab.follow",
            "remote.devices",
            "plan.toggle",
        ] {
            assert_eq!(parse_action(name).unwrap().name(), name);
        }
        assert!(parse_action("tab.rename").is_err());
    }

    #[test]
    fn motions_parse_round_trip_and_have_descriptions() {
        for name in [
            "row.down",
            "row.up",
            "item.next",
            "item.prev",
            "line.down",
            "line.up",
            "top",
            "bottom",
            "prompt.next",
            "prompt.prev",
            "tool.next",
            "tool.prev",
            "error.next",
            "error.prev",
            "edit.next",
            "edit.prev",
            "hunk.next",
            "hunk.prev",
            "search.forward",
            "search.backward",
            "search.next",
            "search.prev",
            "jump.older",
            "jump.newer",
            "detailed.toggle",
            "help",
        ] {
            let action = parse_action(name).unwrap();
            assert!(matches!(action, PanelAction::Motion(_)));
            assert_eq!(action.name(), name);
            assert!(!action.desc().is_empty());
        }
    }

    #[test]
    fn motion_bindings_relax_reservations_only_for_resolvable_sequences() {
        let mut table = PanelUserTable::default();
        for (key, action) in [("j", "line.down"), ("]t", "tool.prev"), ("}", "row.down"), ("G", "top")] {
            table.set(key, action).unwrap();
        }
        for (key, rule) in [
            ("1", "digit"),
            ("1j", "digit"),
            ("<leader>j", "one key"),
            ("jj", "one key"),
            ("gjt", "one key"),
            ("<leader>", "one key"),
            ("g<leader>", "one key"),
            ("i", "input, command line or copy operator"),
            ("o", "input, command line or copy operator"),
            ("A", "input, command line or copy operator"),
            (":", "input, command line or copy operator"),
            ("y", "input, command line or copy operator"),
            ("Y", "input, command line or copy operator"),
            ("g", "they start pairs"),
            ("z", "they start pairs"),
            ("[", "they start pairs"),
            ("]", "they start pairs"),
        ] {
            let error = table.set(key, "row.down").unwrap_err();
            assert!(error.contains(rule), "{key}: {error}");
        }
        assert!(table.set("y", "top").unwrap_err().contains("copy operator"));
        assert!(table.set("j", "tab.next").unwrap_err().contains("BROWSE"));
        assert!(table.set("j", "unknown").unwrap_err().contains("not a panel action"));
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
            probe: None,
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
    fn effective_refuses_a_motion_on_the_effective_leader() {
        for (key, report) in [
            ("<Space>", None),
            ("<Space>", Some(report(Some("jk"), &[]))),
            ("x", Some(report(Some("x"), &[]))),
        ] {
            let mut user = PanelUserTable::default();
            user.set(key, "row.down").unwrap();
            let (map, log) = effective(&user, report.as_ref());
            assert!(!map.bindings.iter().any(|b| b.seq == parse_seq(key).unwrap()));
            assert_eq!(map.bindings, default_bindings());
            assert!(log.contains(&format!(
                "[keys] {}: the panel leader; a motion cannot take it",
                map.leader_label
            )));
        }
        let mut user = PanelUserTable::default();
        user.set("<Space>", "row.down").unwrap();
        let (map, log) = effective(&user, Some(&report(Some("x"), &[])));
        assert!(find(&map, "Space").is_some());
        assert!(log.is_empty());
    }

    #[test]
    fn effective_refuses_motion_prefixes_of_surviving_user_or_nvim_bindings() {
        for motion_first in [false, true] {
            let mut user = PanelUserTable::default();
            if motion_first {
                user.set("x", "row.down").unwrap();
            }
            user.set("xa", "tab.next").unwrap();
            if !motion_first {
                user.set("x", "row.down").unwrap();
            }
            let (map, log) = effective(&user, None);
            assert!(find(&map, "x").is_none());
            assert!(find(&map, "x a").is_some());
            assert!(log
                .iter()
                .any(|line| line.starts_with("[keys] x: starts another binding")));
        }
        let mut user = PanelUserTable::default();
        user.set("x", "row.down").unwrap();
        let report = report(None, &[("xa", Some(":bn<CR>"), None)]);
        let (map, _) = effective(&user, Some(&report));
        assert!(find(&map, "x").is_none());
        assert_eq!(find(&map, "x a").unwrap().source, PanelSource::Nvim);
        user.dels.push(parse_seq("xa").unwrap());
        let (map, log) = effective(&user, Some(&report));
        assert!(find(&map, "x").is_some());
        assert!(log.is_empty());
    }

    #[test]
    fn a_set_on_a_default_takes_it_and_records_what_it_replaced() {
        let mut user = PanelUserTable::default();
        user.set("<leader>M", "tab.next").unwrap();
        assert_eq!(user.overrides, [("<leader>M".to_string(), "model.pick".to_string())]);
        user.set("<leader>M", "tab.prev").unwrap();
        assert_eq!(user.overrides.len(), 1, "the same key twice is one takeover");
        assert_eq!(user.sets.len(), 1, "the last call wins");
        let (map, _) = effective(&user, None);
        assert_eq!(find(&map, "Space M").unwrap().action.name(), "tab.prev");
        assert_eq!(
            map.bindings
                .iter()
                .filter(|b| b.seq == parse_seq("<leader>M").unwrap())
                .count(),
            1
        );
        user.del("<leader>M").unwrap();
        assert!(user.overrides.is_empty());
    }

    #[test]
    fn the_model_picker_can_be_rebound_after_deleting_its_default() {
        let mut user = PanelUserTable::default();
        user.del("<leader>M").unwrap();
        user.set("<leader>x", "model.pick").unwrap();
        let (map, _) = effective(&user, None);
        assert_eq!(find(&map, "Space x").unwrap().action.name(), "model.pick");
        assert!(find(&map, "Space M").is_none());
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
            ..PanelUserTable::default()
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
    fn an_nvim_mapping_on_a_new_pair_is_dropped_and_logged() {
        let r = report(
            Some("<Space>"),
            &[("]d", Some(":bn<CR>"), None), ("gd", Some(":bn<CR>"), None)],
        );
        let (k, log) = effective(&PanelUserTable::default(), Some(&r));
        assert_eq!(k.bindings, default_bindings());
        assert_eq!(log.len(), 2, "{log:?}");
        for pair in ["]d", "gd"] {
            assert!(log.iter().any(|line| line.contains(pair)), "{pair}: {log:?}");
        }
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
