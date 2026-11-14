//! The session tabs a project had open, kept between launches so the next launch can offer them
//! back: `$XDG_STATE_HOME/eitri/tabs/<16 hex>.json`, named, written and set aside the way the layout
//! file is (`state_paths`): 0600 in a 0700 directory, a `.tmp` beside it and then a rename, a file
//! that cannot be used moved whole to `<name>.json.unusable` and read as nothing. Never inside the
//! project.
//!
//! **What is listed:** only tabs that have a session (an empty tab has nothing to bring back), in
//! tab-bar order, each with its agent, the rename it carries and the permission mode it was in, and
//! the tab that was active. In memory that is one list, [`SavedTabs`]. In the file Claude tabs are in
//! `tabs`; a tab running another agent is in `external_tabs`, a separate array with the agent named,
//! so that an older build -- which reads `tabs` only and ignores what it does not know -- never
//! resumes another agent's session as a Claude one. Such a build drops the array on its next write,
//! which loses only the offer to bring those tabs back. The format's version stays 1 for that reason.
//! A tab whose agent runs on a server is in `remote_tabs`, for the same reason: an older build must never
//! resume it as a local Claude tab. It names where its agent is ([`agent::HelperHandoff`]) and is taken up
//! from the server's journal, never resumed; it does not cross to another window of a pair.
//! **Which directory.** A window of a project on a server keeps its files in a directory of that server's own below
//! this one ([`state_dir_for`]): it shares its root with a window of the project's copy on this machine, and the two
//! are different projects.
//! **Whose file.** Each window of a project's pair writes its own: window 1 (or a window alone) today's
//! `<16 hex>.json`, window 2 `<16 hex>-w2.json`, a window that runs unpaired `<16 hex>-u.json`
//! ([`file_name_as`]). Unpaired windows share theirs and the last writer wins; the session leases, not
//! this file, are what stop two windows driving one session. A window may also offer another window's
//! saved tabs after its own ([`startup_with`]): that file is first claimed, moved to `<name>.json.claiming` and
//! only then read (so what is offered is exactly what was moved), then to `<name>.json.offered`; both names only
//! the offering window touches, so the window it belonged to can write its own again without
//! losing them; the claim is renamed `<name>.json.taken` once this window's own file holds every one of
//! them ([`TabWriter::take_over_when_written`]), never before, so no saved tab is ever out of every file.
//!
//! **A group's file.** In compositor mode every chat is a window of its own and none of them writes: each tells window
//! 1 its tabs, and window 1 writes today's `<16 hex>.json` for them all ([`merge_windows`]). Each extra window's tab on
//! screen is saved as that view, so `eitri` mode reads the file as it always did: that tab in its view, every other tab
//! in the base view, nothing hidden. A tab that view cannot say the window of carries its window in `window` as well,
//! which `eitri` mode does not read and compositor mode does ([`tabs_for`]); an older build ignores the key.
//!
//! **When it is written** is [`TabMemory`]'s job, run on its own thread by [`TabWriter`]: whenever the set it is shown differs from the set
//! it last saw, and never because the window is closing -- closing closes every tab, and recording
//! "no tabs" then would erase exactly what the next launch wants.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::agent_backend::AgentId;
use crate::agent_bridge::SessionModeChoice;
use crate::state_paths::{create_state_dir, state_subdir, temporary};
use crate::window_link::names::WindowName;
use crate::window_link::pair::{tabs_file_name, Role};

/// The file format's version. A file of any other version is not read.
pub const VERSION: u32 = 1;

/// The most of one tab's draft the file holds. The file is read whole and written at every change, so a
/// long paste is not kept here: the window's close asks about it instead.
pub const DRAFT_SAVE_MAX: usize = 256 * 1024;

/// A composer's text as the file keeps it: nothing for blank text or text past [`DRAFT_SAVE_MAX`].
pub fn draft_to_save(text: &str) -> Option<String> {
    (!text.trim().is_empty() && text.len() <= DRAFT_SAVE_MAX).then(|| text.to_string())
}

/// One tab worth bringing back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedTab {
    /// The agent the tab ran. A session id is only ever one agent's: two agents may use the same one.
    pub agent: AgentId,
    pub conversation_id: String,
    /// The agent's own id for the session (Claude's session id, a Codex thread id): the key its record,
    /// its lease and its resume all use.
    pub provider_session_id: String,
    /// The tab's rename, if it has one.
    pub name: Option<String>,
    /// The mode the tab was in. `bypass` is kept here because the person who chose it should see it
    /// again -- and be asked about it, never silently given it back, when the tabs are restored.
    pub mode: SessionModeChoice,
    /// The chat view the tab was shown in, 2 to 9 (`agent#N`); absent for the base view.
    pub view: Option<u8>,
    /// The chat window the tab was in, 2 to 9, when `view` does not say so: one tab of a window is its view, and the
    /// others are saved in the base view for `eitri` mode to find. Absent otherwise.
    pub window: Option<u8>,
    /// A turn was running as this was written, and written again when it ended: what is read back says a window went with
    /// no chance to finish it, or to ask about it. Absent from the file otherwise.
    pub turn_running: bool,
    /// The composer's unsent text, for `s` to put back. Absent with nothing there, or with more than
    /// [`DRAFT_SAVE_MAX`]. This is what the person typed: nothing may log it.
    pub draft: Option<String>,
    /// The tab's row key (`k-` and 16 lowercase hex digits), so the tab keeps its name when it is broken out into
    /// another window or brought back after a restart. Absent from a file or a peer that predates keys, and read as
    /// absent when it is no key's shape: the tab that comes back then keeps the key it was made with.
    pub row_key: Option<String>,
    /// The mode plan mode was entered from, when the tab was planning as this was written. `mode` stays the
    /// execution mode underneath; a tab restored with this set asks for plan mode before its first message.
    /// Absent from the file otherwise.
    pub plan_origin: Option<SessionModeChoice>,
}

impl SavedTab {
    /// The chat window this tab belongs to: `c1` holds every tab with no number.
    fn chat_window(&self) -> u8 {
        self.window.or(self.view).unwrap_or(1)
    }

    /// This tab with no place of its own: the window it is in says where it shows.
    fn in_base_view(self) -> SavedTab {
        SavedTab {
            view: None,
            window: None,
            ..self
        }
    }

    /// The agent and session that name this tab.
    pub(crate) fn key(&self) -> (AgentId, String) {
        (self.agent.clone(), self.provider_session_id.clone())
    }

    /// This tab as it crosses the link to another window of the project.
    pub fn to_wire(&self) -> serde_json::Value {
        let entry = OtherEntry {
            agent: self.agent.clone(),
            conversation_id: self.conversation_id.clone(),
            provider_session_id: self.provider_session_id.clone(),
            name: self.name.clone(),
            mode: self.mode,
            view: self.view,
            window: self.window,
            turn_running: self.turn_running,
            draft: self.draft.clone(),
            row_key: self.row_key.clone(),
            plan_origin: self.plan_origin,
        };
        serde_json::to_value(entry).expect("a saved tab always serializes")
    }

    /// A tab another window sent: its input, so a shape that is not a tab or a tab with no session is refused, as a
    /// blank entry of the file is dropped.
    pub fn from_wire(value: &serde_json::Value) -> Result<SavedTab, String> {
        let entry = OtherEntry::deserialize(value).map_err(|e| format!("not a saved tab ({e})"))?;
        if entry.provider_session_id.trim().is_empty() || entry.conversation_id.trim().is_empty() {
            return Err("a saved tab with no session".to_string());
        }
        Ok(SavedTab {
            agent: entry.agent,
            conversation_id: entry.conversation_id,
            provider_session_id: entry.provider_session_id,
            name: entry.name,
            mode: entry.mode,
            view: entry.view,
            window: entry.window,
            turn_running: entry.turn_running,
            draft: entry.draft,
            row_key: entry.row_key,
            plan_origin: entry.plan_origin,
        })
    }
}

/// The tabs of one window: the Claude tabs, then the other agents' tabs, each in tab-bar order.
///
/// Which tab comes back on screen follows two rules, one per kind, because an older build reads only
/// the Claude tabs and must still find one of them active: a Claude tab is active when it was on
/// screen and otherwise when it was the Claude tab on screen most recently, so the first Claude tab
/// at least; another agent's tab is active only when it was on screen, and then it wins.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavedTabs {
    pub tabs: Vec<SavedTab>,
    /// Index into `tabs` of the tab that comes back on screen; `None` only when no Claude tab is listed
    /// and none of the other agents' tabs was on screen.
    pub active: Option<usize>,
    /// Index into `tabs` of the Claude tab a build that reads only the Claude tabs puts on screen:
    /// `active` when that is a Claude tab, else the Claude tab on screen most recently. `None` when no
    /// Claude tab is listed. Kept so the file says to such a build what it always said.
    pub claude_active: Option<usize>,
    /// The tabs of both kinds in the order the tab bar had them, as `(agent, session)`; empty when that is not
    /// known (a file from before it was kept, a set built from several windows). `tabs` itself always lists the
    /// Claude tabs first, so this is where a mix of agents keeps its order: [`SavedTabs::in_bar_order`] reads it.
    pub bar: Vec<(AgentId, String)>,
    /// The tabs whose agent runs on a server, in tab-bar order: none of `tabs`, `active` or `bar` names one.
    pub remote: Vec<SavedRemoteTab>,
}

/// A tab whose agent runs on a server, worth bringing back: where the agent is, so a reopened window takes it up
/// from the server's journal (never a resume), and the CLI's own session, to continue it through a new sidecar if
/// the server no longer runs that agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedRemoteTab {
    pub helper: agent::HelperHandoff,
    /// The CLI's session id, once the agent reported it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub mode: SessionModeChoice,
    /// Owning compositor chat window; absent means the first window or a standalone panel.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "lenient_view")]
    pub window: Option<u8>,
}

impl SavedRemoteTab {
    /// Whether `other` names the same agent: the same session of the same sidecar under the same helper installation
    /// on the same server. A file read
    /// back and two files joined keep one entry per agent by this.
    fn same_agent(&self, other: &SavedRemoteTab) -> bool {
        self.helper.host_key_sha256 == other.helper.host_key_sha256
            && self.helper.daemon == other.helper.daemon
            && self.helper.sidecar_id == other.helper.sidecar_id
            && self.helper.session_id == other.helper.session_id
    }
}

impl SavedTabs {
    /// The set of `claude`'s tabs followed by `others`', with `claude_active` the index among `claude`
    /// of the Claude tab on screen (or on screen most recently) and `others_active` the index among
    /// `others` of the tab on screen, if one of them was. An index past its list's end is read as the
    /// first Claude tab, and as no other agent's tab.
    pub fn from_parts(
        claude: Vec<SavedTab>,
        claude_active: usize,
        others: Vec<SavedTab>,
        others_active: Option<usize>,
    ) -> Self {
        let claude_count = claude.len();
        let claude_active = match claude_count {
            0 => None,
            count if claude_active < count => Some(claude_active),
            _ => Some(0),
        };
        let others_active = others_active.filter(|i| *i < others.len()).map(|i| claude_count + i);
        let mut tabs = claude;
        tabs.extend(others);
        SavedTabs {
            tabs,
            active: others_active.or(claude_active),
            claude_active,
            bar: Vec::new(),
            remote: Vec::new(),
        }
    }

    /// These tabs with the tab bar's order recorded: `bar` names the tabs of both kinds, first to last. A tab it
    /// does not name keeps its place behind the ones it does.
    pub fn with_bar(mut self, bar: Vec<(AgentId, String)>) -> SavedTabs {
        self.bar = bar;
        self
    }

    /// The tabs in the order the tab bar had them when that is recorded, else Claude's then the other agents',
    /// which is how `tabs` lists them. A restore brings them back in this order.
    pub fn in_bar_order(&self) -> Vec<&SavedTab> {
        let mut tabs: Vec<&SavedTab> = self.tabs.iter().collect();
        tabs.sort_by_key(|tab| {
            self.bar
                .iter()
                .position(|(agent, id)| *agent == tab.agent && *id == tab.provider_session_id)
                .unwrap_or(usize::MAX)
        });
        tabs
    }

    /// These tabs with no place of their own: the window they are given to says where they show.
    pub fn in_base_view(mut self) -> SavedTabs {
        self.tabs = self.tabs.into_iter().map(SavedTab::in_base_view).collect();
        for tab in &mut self.remote {
            tab.window = None;
        }
        self
    }

    /// A window following a verified replacement key keeps loaded handoffs aligned with its saved file.
    pub fn rekey_remote(&mut self, old: &str, new: &str) {
        for tab in &mut self.remote {
            if tab.helper.host_key_sha256 == old {
                tab.helper.host_key_sha256 = new.to_owned();
            }
        }
    }

    /// No tab of any agent, on this machine or a server.
    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty() && self.remote.is_empty()
    }

    /// These tabs with `remote`'s tabs whose agents run on a server.
    pub fn with_remote(mut self, remote: Vec<SavedRemoteTab>) -> SavedTabs {
        self.remote = remote;
        self
    }

    /// These tabs as they cross the link to another window of the project.
    pub fn to_wire(&self) -> serde_json::Value {
        let mut wire = serde_json::json!({
            "tabs": self.tabs.iter().map(SavedTab::to_wire).collect::<Vec<_>>(),
            "active": self.active,
            "claude_active": self.claude_active,
        });
        // The tab bar's order across agents, only where it is known: a peer that predates it reads none.
        if !self.bar.is_empty() {
            wire["bar"] = self
                .bar
                .iter()
                .map(|(agent, id)| serde_json::json!({ "agent": agent, "provider_session_id": id }))
                .collect();
        }
        if !self.remote.is_empty() {
            wire["remote_tabs"] = serde_json::to_value(&self.remote).expect("saved remote tabs serialize");
        }
        wire
    }

    /// Tabs another window sent: its input, so a shape that is not a set of tabs, or one entry that is no tab, refuses
    /// the set, and an index past its end reads as absent.
    pub fn from_wire(value: &serde_json::Value) -> Result<SavedTabs, String> {
        let tabs = value
            .get("tabs")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| "not a set of saved tabs".to_string())?
            .iter()
            .map(SavedTab::from_wire)
            .collect::<Result<Vec<SavedTab>, String>>()?;
        let index = |key: &str| {
            value
                .get(key)
                .and_then(serde_json::Value::as_u64)
                .and_then(|index| usize::try_from(index).ok())
                .filter(|index| *index < tabs.len())
        };
        let (active, claude_active) = (index("active"), index("claude_active"));
        // The order is a hint: a place that is no tab of this shape is skipped, never a refusal of the set.
        let bar = value
            .get("bar")
            .and_then(serde_json::Value::as_array)
            .map(|places| {
                places
                    .iter()
                    .filter_map(|place| {
                        let agent = AgentId::deserialize(place.get("agent")?).ok()?;
                        let id = place.get("provider_session_id")?.as_str()?;
                        Some((agent, id.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(SavedTabs {
            tabs,
            active,
            claude_active,
            bar,
            remote: match value.get("remote_tabs") {
                None => Vec::new(),
                Some(remote) => serde_json::from_value(remote.clone()).map_err(|e| format!("remote tabs: {e}"))?,
            },
        })
    }

    /// The tab that comes back on screen.
    pub fn active_tab(&self) -> Option<&SavedTab> {
        self.active.and_then(|i| self.tabs.get(i))
    }

    /// Whether any tab ran an agent other than Claude.
    pub fn has_other_agents(&self) -> bool {
        self.tabs.iter().any(|t| t.agent.is_external())
    }

    /// The Claude tabs, in order.
    pub fn claude_tabs(&self) -> impl Iterator<Item = &SavedTab> {
        self.tabs.iter().filter(|t| !t.agent.is_external())
    }

    /// The other agents' tabs, in order.
    pub fn other_tabs(&self) -> impl Iterator<Item = &SavedTab> {
        self.tabs.iter().filter(|t| t.agent.is_external())
    }

    /// Where the tab at `index` stands among the tabs of its own kind (Claude's, or the other agents'),
    /// when it is of the kind `external` says.
    fn rank(&self, index: Option<usize>, external: bool) -> Option<usize> {
        let index = index?;
        let tab = self.tabs.get(index)?;
        (tab.agent.is_external() == external).then(|| {
            self.tabs[..index]
                .iter()
                .filter(|t| t.agent.is_external() == external)
                .count()
        })
    }
}

fn lenient_view<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u8>, D::Error> {
    let value = Option::<serde_json::Value>::deserialize(d)?;
    Ok(value
        .and_then(|v| v.as_u64())
        .and_then(|n| u8::try_from(n).ok())
        .filter(|n| (2..=crate::layout::module::MAX_INSTANCE).contains(n)))
}

/// A row key of any other shape is read as absent, so a hand edit or another build's value never makes the file or a
/// peer's record unusable; the tab that comes back then keeps the key it was made with.
fn lenient_row_key<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    Ok(value
        .as_str()
        .and_then(crate::tab_set::RowKey::parse)
        .map(|key| key.as_str().to_string()))
}

/// A draft of any other type is read as absent: the error that would say otherwise quotes the value, and a
/// file's error goes to the log.
fn lenient_draft<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::String(text) => Some(text),
        _ => None,
    })
}

#[derive(Serialize, Deserialize)]
struct TabsFile {
    version: u32,
    project_root: String,
    tabs: Vec<ClaudeEntry>,
    /// Index into `tabs`: what a build that reads only the Claude tabs puts on screen.
    active: usize,
    /// Absent from a file with no such tab, so the file of a window that ran only Claude is what it
    /// always was.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    external_tabs: Vec<ExternalEntry>,
    /// Index into `external_tabs` when the tab on screen was another agent's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active_external: Option<usize>,
    /// The tabs of both kinds in tab-bar order, written only when a window ran another agent beside Claude: `tabs`
    /// and `external_tabs` each keep their own order, so this is the one place the mix is. A build that does not
    /// know it ignores it and restores the Claude tabs first, as it always did.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tab_bar: Vec<BarSlot>,
    /// The tabs whose agent runs on a server, in an array of their own so an older build, which reads `tabs` and
    /// ignores what it does not know, never resumes one as a local Claude tab. Absent when there is none. An entry
    /// this build cannot read (a field missing, a later build's shape) is a hole, as in `external_tabs`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    remote_tabs: Vec<RemoteEntry>,
}

/// One entry of `remote_tabs`.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum RemoteEntry {
    Known(SavedRemoteTab),
    Unknown(serde_json::Value),
}

/// One place in `tab_bar`. An agent this build does not know (a later build's) is a place nobody here fills, as in
/// `external_tabs`, and never makes the file unusable.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum BarSlot {
    Known {
        agent: AgentId,
        provider_session_id: String,
    },
    Unknown(serde_json::Value),
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// One entry of `tabs`: a Claude tab, its agent implied by the array it is in.
#[derive(Serialize, Deserialize)]
struct ClaudeEntry {
    conversation_id: String,
    provider_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    mode: SessionModeChoice,
    /// A value no chat view can have is read as absent, so a hand edit never makes the file unusable.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "lenient_view")]
    view: Option<u8>,
    /// A value no chat window beside `c1` can have is read as absent, as `view`'s is.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "lenient_view")]
    window: Option<u8>,
    #[serde(default, skip_serializing_if = "is_false")]
    turn_running: bool,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "lenient_draft")]
    draft: Option<String>,
    /// Absent from a file written before tabs had keys; an older build ignores it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient_row_key"
    )]
    row_key: Option<String>,
    /// Absent unless the tab was planning; an older build ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plan_origin: Option<SessionModeChoice>,
}

/// One entry of `external_tabs`: another agent's tab, the agent named as its records name it
/// (`codex`, `acp:<name>`).
#[derive(Serialize, Deserialize)]
struct OtherEntry {
    agent: AgentId,
    conversation_id: String,
    provider_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    mode: SessionModeChoice,
    /// A value no chat view can have is read as absent, so a hand edit never makes the file unusable.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "lenient_view")]
    view: Option<u8>,
    /// A value no chat window beside `c1` can have is read as absent, as `view`'s is.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "lenient_view")]
    window: Option<u8>,
    #[serde(default, skip_serializing_if = "is_false")]
    turn_running: bool,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "lenient_draft")]
    draft: Option<String>,
    /// Absent from a file written before tabs had keys; an older build ignores it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient_row_key"
    )]
    row_key: Option<String>,
    /// Absent unless the tab was planning; an older build ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plan_origin: Option<SessionModeChoice>,
}

/// One entry of `external_tabs` as read: an agent this build does not know (a later build's) is kept
/// out of the restore without making the whole file unusable, which would lose the Claude tabs too.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum ExternalEntry {
    Known(OtherEntry),
    Unknown(serde_json::Value),
}

/// `<state home>/eitri/tabs`, by the same rule as the layout directory.
pub fn state_dir(xdg_state_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    state_subdir(xdg_state_home, home, "tabs")
}

/// Where a window of `origin` keeps its saved tabs: [`state_dir`] for a local project and a remote project's copy,
/// and below it, for a project on a server, a directory named by the server's key
/// ([`crate::remote_project::origin_key`]). A window of a remote project and a `--local` window of its copy share a
/// root, and each keeps its own file.
pub fn state_dir_for(
    xdg_state_home: Option<&OsStr>,
    home: Option<&OsStr>,
    origin: &crate::remote_project::TabOrigin,
) -> Option<PathBuf> {
    let base = state_dir(xdg_state_home, home)?;
    Some(match crate::remote_project::tab_origin_key(origin) {
        Some(key) => base.join(key),
        None => base,
    })
}

/// The marker a remote window leaves in its server's directory once the shared file's remote tabs of that server were
/// carried for `project_root` and `role`, copied or not: `<file>.carried` beside the window's file.
fn carried_marker(project_root: &Path, role: Role) -> String {
    let file = file_name_as(project_root, role);
    format!("{}.carried", file.strip_suffix(".json").unwrap_or(&file))
}

/// A remote window's first start, once per root and role: the remote tabs of the server of `host_key` that the
/// project's file in `base` (of the same `role`) holds, written there before files were kept by origin, are copied
/// into the window's file in `keyed` when it has none, and the carry is marked done ([`carried_marker`]) whatever it
/// copied. A marked carry never runs again, so a remote tab claimed or set aside later is not copied back; the copies
/// left in `base` are dropped by the next write of a window of this machine ([`carried_elsewhere`]). Returns how
/// many were copied. Disk work, before the window shows.
pub fn carry_remote_tabs(
    base: &Path,
    keyed: &Path,
    project_root: &Path,
    role: Role,
    host_key: &str,
) -> std::io::Result<usize> {
    let marker = keyed.join(carried_marker(project_root, role));
    if marker.exists() {
        return Ok(0);
    }
    let mut carried = 0;
    if load_as(keyed, project_root, role) == Loaded::Missing {
        if let Loaded::Saved(saved) = load_as(base, project_root, role) {
            let remote: Vec<SavedRemoteTab> = saved
                .remote
                .into_iter()
                .filter(|tab| tab.helper.host_key_sha256 == host_key)
                .collect();
            if !remote.is_empty() {
                carried = remote.len();
                save_as(keyed, project_root, role, &SavedTabs::default().with_remote(remote))?;
            }
        }
    }
    create_state_dir(keyed)?;
    agent::private_fs::write_private(&marker, b"")?;
    Ok(carried)
}

/// Whether the remote tabs of the server of `host_key` saved in the shared file of `project_root` and `role` now live
/// in that server's own directory below `base` (a remote window carried them, [`carry_remote_tabs`]): a window of this
/// machine no longer keeps them in its file, so no agent is taken up by two windows.
pub fn carried_elsewhere(base: &Path, project_root: &Path, role: Role, host_key: &str) -> bool {
    base.join(crate::remote_project::origin_key(host_key))
        .join(carried_marker(project_root, role))
        .exists()
}

/// After a server's host key changed from `old_key` to `new_key` (the project's record took the new one): each saved
/// tabs file of `project_root` in the old key's directory below `base` moves to the new key's, unless the new one has
/// that role's file already, its remote tabs taking the new key, so a window under the new key takes them up again.
/// Returns how many remote tabs moved. Disk work, on the worker that compared the keys.
pub fn carry_to_new_host_key(base: &Path, project_root: &Path, old_key: &str, new_key: &str) -> std::io::Result<usize> {
    let old_dir = base.join(crate::remote_project::origin_key(old_key));
    let new_dir = base.join(crate::remote_project::origin_key(new_key));
    let mut moved = 0;
    for role in [Role::Main, Role::Second, Role::Unpaired] {
        let Loaded::Saved(mut saved) = load_as(&old_dir, project_root, role) else {
            continue;
        };
        if load_as(&new_dir, project_root, role) != Loaded::Missing {
            continue;
        }
        for tab in &mut saved.remote {
            if tab.helper.host_key_sha256 == old_key {
                tab.helper.host_key_sha256 = new_key.to_owned();
                moved += 1;
            }
        }
        save_as(&new_dir, project_root, role, &saved)?;
        std::fs::remove_file(old_dir.join(file_name_as(project_root, role)))?;
        // The shared file's copies were carried under the old key already.
        let marker = old_dir.join(carried_marker(project_root, role));
        if marker.exists() {
            agent::private_fs::write_private(&new_dir.join(carried_marker(project_root, role)), b"")?;
        }
    }
    Ok(moved)
}

/// The file's text for `saved`: pretty JSON, so a person can read and fix it.
pub fn encode(saved: &SavedTabs, project_root: &Path) -> String {
    let file = TabsFile {
        version: VERSION,
        project_root: project_root.to_string_lossy().into_owned(),
        tabs: saved
            .claude_tabs()
            .map(|t| ClaudeEntry {
                conversation_id: t.conversation_id.clone(),
                provider_session_id: t.provider_session_id.clone(),
                name: t.name.clone(),
                mode: t.mode,
                view: t.view,
                window: t.window,
                turn_running: t.turn_running,
                draft: t.draft.clone(),
                row_key: t.row_key.clone(),
                plan_origin: t.plan_origin,
            })
            .collect(),
        active: saved.rank(saved.claude_active, false).unwrap_or(0),
        external_tabs: saved
            .other_tabs()
            .map(|t| {
                ExternalEntry::Known(OtherEntry {
                    agent: t.agent.clone(),
                    conversation_id: t.conversation_id.clone(),
                    provider_session_id: t.provider_session_id.clone(),
                    name: t.name.clone(),
                    mode: t.mode,
                    view: t.view,
                    window: t.window,
                    turn_running: t.turn_running,
                    draft: t.draft.clone(),
                    row_key: t.row_key.clone(),
                    plan_origin: t.plan_origin,
                })
            })
            .collect(),
        active_external: saved.rank(saved.active, true),
        tab_bar: if saved.has_other_agents() && !saved.bar.is_empty() {
            saved
                .in_bar_order()
                .into_iter()
                .map(|t| BarSlot::Known {
                    agent: t.agent.clone(),
                    provider_session_id: t.provider_session_id.clone(),
                })
                .collect()
        } else {
            Vec::new()
        },
        remote_tabs: saved.remote.iter().cloned().map(RemoteEntry::Known).collect(),
    };
    serde_json::to_string_pretty(&file).expect("saved tabs always serialize")
}

/// `text` read back as this project's saved tabs. A tab with no session id is dropped and a session
/// listed twice is kept once (its first place), because either would only ever be a hand edit gone
/// wrong; an `active` past the end becomes the first tab. `Err` says why the file cannot be used.
pub fn decode(text: &str, project_root: &Path) -> Result<SavedTabs, String> {
    let file: TabsFile = serde_json::from_str(text).map_err(|e| format!("not a saved-tabs file ({e})"))?;
    if file.version != VERSION {
        return Err(format!("version {} (this build reads {VERSION})", file.version));
    }
    let root = project_root.to_string_lossy();
    if file.project_root != root {
        return Err(format!("written for {}, not {root}", file.project_root));
    }
    let claude: Vec<SavedTab> = file
        .tabs
        .into_iter()
        .map(|t| SavedTab {
            agent: AgentId::Claude,
            conversation_id: t.conversation_id,
            provider_session_id: t.provider_session_id,
            name: t.name,
            mode: t.mode,
            view: t.view,
            window: t.window,
            turn_running: t.turn_running,
            draft: t.draft,
            row_key: t.row_key,
            plan_origin: t.plan_origin,
        })
        .collect();
    // An agent this build does not know stays a hole, so the active index still finds its tab.
    let others: Vec<Option<SavedTab>> = file
        .external_tabs
        .into_iter()
        .map(|entry| match entry {
            ExternalEntry::Known(t) => Some(SavedTab {
                agent: t.agent,
                conversation_id: t.conversation_id,
                provider_session_id: t.provider_session_id,
                name: t.name,
                mode: t.mode,
                view: t.view,
                window: t.window,
                turn_running: t.turn_running,
                draft: t.draft,
                row_key: t.row_key,
                plan_origin: t.plan_origin,
            }),
            ExternalEntry::Unknown(_) => None,
        })
        .collect();
    let claude_active = claude
        .get(file.active)
        .map(|t| (t.agent.clone(), t.provider_session_id.clone()));
    let others_active = file
        .active_external
        .and_then(|i| others.get(i))
        .and_then(Option::as_ref)
        .map(|t| (t.agent.clone(), t.provider_session_id.clone()));
    // The same rules for both kinds, a session being one agent's id of it; a Claude tab listed among
    // the other agents' is not one of them.
    let claude = usable(claude.into_iter().map(Some), false);
    let others = usable(others.into_iter(), true);
    // Where the tab that was active ended up, so dropping an earlier entry cannot move it.
    let at = |tabs: &[SavedTab], key: Option<(AgentId, String)>| {
        key.and_then(|(agent, id)| {
            tabs.iter()
                .position(|t| t.agent == agent && t.provider_session_id == id)
        })
    };
    let claude_at = at(&claude, claude_active).unwrap_or(0);
    let others_at = at(&others, others_active);
    let bar = file
        .tab_bar
        .into_iter()
        .filter_map(|slot| match slot {
            BarSlot::Known {
                agent,
                provider_session_id,
            } => Some((agent, provider_session_id)),
            BarSlot::Unknown(_) => None,
        })
        .collect();
    // A server's agent listed twice is kept once.
    let mut remote: Vec<SavedRemoteTab> = Vec::new();
    for entry in file.remote_tabs {
        if let RemoteEntry::Known(tab) = entry {
            if !remote.iter().any(|kept| kept.same_agent(&tab)) {
                remote.push(tab);
            }
        }
    }
    Ok(SavedTabs::from_parts(claude, claude_at, others, others_at)
        .with_bar(bar)
        .with_remote(remote))
}

/// `tabs` without a blank entry, a repeat of an earlier one, or one of the wrong kind.
fn usable(tabs: impl Iterator<Item = Option<SavedTab>>, external: bool) -> Vec<SavedTab> {
    let mut kept: Vec<SavedTab> = Vec::new();
    for tab in tabs.flatten() {
        let blank = tab.provider_session_id.trim().is_empty() || tab.conversation_id.trim().is_empty();
        let listed = kept
            .iter()
            .any(|k| k.agent == tab.agent && k.provider_session_id == tab.provider_session_id);
        if !blank && !listed && tab.agent.is_external() == external {
            kept.push(tab);
        }
    }
    kept
}

/// One file for a group's chat windows: each window's tabs in `parts`, windows in name order. `c1`'s are in the base
/// view; each later window's tab on screen is that window's view and its others name the window, so `eitri` mode
/// shows the first in its view and the rest in the base view, and no tab is hidden. The tab on screen is
/// `active_from`'s. Anything but a chat window holds no tabs, and a session a window listed first stays there.
pub fn merge_windows(parts: &[(WindowName, SavedTabs)], active_from: Option<&WindowName>) -> SavedTabs {
    let mut windows: Vec<(u8, &SavedTabs)> = parts
        .iter()
        .filter_map(|(name, part)| Some((name.chat_number()?, part)))
        .collect();
    windows.sort_by_key(|(number, _)| *number);
    let mut merged: Vec<SavedTab> = Vec::new();
    let mut remote: Vec<SavedRemoteTab> = Vec::new();
    for (number, part) in windows {
        for tab in &part.remote {
            if !remote.iter().any(|kept| {
                kept.helper.host_key_sha256 == tab.helper.host_key_sha256
                    && kept.helper.daemon == tab.helper.daemon
                    && kept.helper.sidecar_id == tab.helper.sidecar_id
                    && kept.helper.session_id == tab.helper.session_id
            }) {
                let mut tab = tab.clone();
                tab.window = (number != 1).then_some(number);
                remote.push(tab);
            }
        }
        let on_screen = part.active.or(part.claude_active).unwrap_or(0);
        for (index, tab) in part.tabs.iter().enumerate() {
            if merged.iter().any(|kept| kept.key() == tab.key()) {
                continue;
            }
            let (view, window) = match number {
                1 => (None, None),
                _ if index == on_screen => (Some(number), None),
                _ => (None, Some(number)),
            };
            merged.push(SavedTab {
                view,
                window,
                ..tab.clone()
            });
        }
    }
    let from = active_from.and_then(|name| parts.iter().find(|(part, _)| part == name).map(|(_, part)| part));
    let key_of = |index: fn(&SavedTabs) -> Option<usize>| {
        from.and_then(|part| index(part).and_then(|i| part.tabs.get(i)))
            .map(SavedTab::key)
    };
    let (on_screen, claude_on_screen) = (key_of(|part| part.active), key_of(|part| part.claude_active));
    // The tab bar's order, where any window recorded it: each window's tabs in turn, in the order that window had them.
    let bar: Vec<(AgentId, String)> = if parts.iter().any(|(_, part)| !part.bar.is_empty()) {
        let mut bar: Vec<(AgentId, String)> = Vec::new();
        for (_, part) in parts.iter().filter(|(name, _)| name.chat_number().is_some()) {
            for tab in part.in_bar_order() {
                if !bar.contains(&tab.key()) {
                    bar.push(tab.key());
                }
            }
        }
        bar
    } else {
        Vec::new()
    };
    from_tabs(merged, &on_screen, &claude_on_screen)
        .with_bar(bar)
        .with_remote(remote)
}

/// The window `name`'s part of `file`, each tab in the base view: the tabs whose window or view says it is theirs,
/// `c1`'s being the tabs that name none. The window's own view is the tab it had on screen; `c1` has the file's.
pub fn tabs_for(file: &SavedTabs, name: &WindowName) -> SavedTabs {
    let Some(number) = name.chat_number() else {
        return SavedTabs::default();
    };
    let at = |index: Option<usize>| {
        index
            .and_then(|i| file.tabs.get(i))
            .filter(|tab| tab.chat_window() == number)
            .map(SavedTab::key)
    };
    let shown = file
        .tabs
        .iter()
        .find(|tab| tab.chat_window() == number && tab.view == Some(number))
        .map(SavedTab::key);
    let on_screen = shown.clone().or_else(|| at(file.active));
    let claude_on_screen = shown.or_else(|| at(file.claude_active));
    let tabs: Vec<SavedTab> = file
        .tabs
        .iter()
        .filter(|tab| tab.chat_window() == number)
        .cloned()
        .map(SavedTab::in_base_view)
        .collect();
    let bar = if file.bar.is_empty() {
        Vec::new()
    } else {
        file.in_bar_order()
            .into_iter()
            .filter(|tab| tab.chat_window() == number)
            .map(SavedTab::key)
            .collect()
    };
    let remote = file
        .remote
        .iter()
        .filter(|tab| tab.window.unwrap_or(1) == number)
        .cloned()
        .map(|mut tab| {
            tab.window = None;
            tab
        })
        .collect();
    from_tabs(tabs, &on_screen, &claude_on_screen)
        .with_bar(bar)
        .with_remote(remote)
}

/// The chat windows `file` holds tabs for, by number.
pub fn windows_holding(file: &SavedTabs) -> Vec<WindowName> {
    let numbers: std::collections::BTreeSet<u8> = file
        .tabs
        .iter()
        .map(SavedTab::chat_window)
        .chain(file.remote.iter().map(|tab| tab.window.unwrap_or(1)))
        .collect();
    numbers.into_iter().map(WindowName::Chat).collect()
}

/// The extra chat views `file` has a tab saved in, by number.
pub fn views_holding(file: &SavedTabs) -> Vec<u8> {
    let numbers: std::collections::BTreeSet<u8> = file.tabs.iter().filter_map(|tab| tab.view).collect();
    numbers.into_iter().collect()
}

/// `tabs`, in any order, with `on_screen` the tab that comes back on screen when it is listed and `claude_on_screen`
/// the Claude tab a build that reads only those puts there.
fn from_tabs(
    tabs: Vec<SavedTab>,
    on_screen: &Option<(AgentId, String)>,
    claude_on_screen: &Option<(AgentId, String)>,
) -> SavedTabs {
    let (claude, others): (Vec<SavedTab>, Vec<SavedTab>) = tabs.into_iter().partition(|t| !t.agent.is_external());
    let at = |tabs: &[SavedTab], key: &Option<(AgentId, String)>| {
        key.as_ref().and_then(|key| tabs.iter().position(|t| t.key() == *key))
    };
    let claude_at = at(&claude, on_screen)
        .or_else(|| at(&claude, claude_on_screen))
        .unwrap_or(0);
    let others_at = at(&others, on_screen);
    SavedTabs::from_parts(claude, claude_at, others, others_at)
}

/// What [`load`] found.
#[derive(Debug, PartialEq, Eq)]
pub enum Loaded {
    Saved(SavedTabs),
    /// No file: a project whose tabs have not been kept yet. The normal first launch, and not logged.
    Missing,
    /// A file that cannot be used, and why.
    Unusable(String),
}

/// The tabs file of a window in `role`: window 1's (or a window alone) is today's name, so an older build
/// reads it.
pub fn file_name_as(project_root: &Path, role: Role) -> String {
    tabs_file_name(project_root, role)
}

/// Reads this project's file from `dir`.
pub fn load(dir: &Path, project_root: &Path) -> Loaded {
    load_as(dir, project_root, Role::Main)
}

/// [`load`] of the file of a window in `role`.
pub fn load_as(dir: &Path, project_root: &Path, role: Role) -> Loaded {
    load_path(&dir.join(file_name_as(project_root, role)), project_root)
}

fn load_path(path: &Path, project_root: &Path) -> Loaded {
    match agent::private_fs::read_private_to_string(path) {
        Ok(text) => match decode(&text, project_root) {
            Ok(saved) => Loaded::Saved(saved),
            Err(why) => Loaded::Unusable(format!("{}: {why}", path.display())),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Loaded::Missing,
        Err(e) => Loaded::Unusable(format!("{}: {e}", path.display())),
    }
}

/// Writes this project's file into `dir`, creating it: to a `.tmp` of this process's own beside it and
/// then renamed over it, so a process that dies mid-write leaves the previous file, never half of one.
pub fn save(dir: &Path, project_root: &Path, saved: &SavedTabs) -> std::io::Result<PathBuf> {
    save_as(dir, project_root, Role::Main, saved)
}

/// [`save`] to the file of a window in `role`.
pub fn save_as(dir: &Path, project_root: &Path, role: Role, saved: &SavedTabs) -> std::io::Result<PathBuf> {
    save_path(dir, dir.join(file_name_as(project_root, role)), project_root, saved)
}

fn save_path(dir: &Path, path: PathBuf, project_root: &Path, saved: &SavedTabs) -> std::io::Result<PathBuf> {
    create_state_dir(dir)?;
    let tmp = temporary(&path);
    let written = agent::private_fs::write_private(&tmp, encode(saved, project_root).as_bytes())
        .and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(err) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(path)
}

/// Moves this project's file, whole, to `<name>.json.unusable` beside it and returns where it went.
pub fn set_aside(dir: &Path, project_root: &Path) -> std::io::Result<PathBuf> {
    set_aside_as(dir, project_root, Role::Main)
}

/// [`set_aside`] of the file of a window in `role`.
pub fn set_aside_as(dir: &Path, project_root: &Path, role: Role) -> std::io::Result<PathBuf> {
    let path = dir.join(file_name_as(project_root, role));
    let aside = path.with_extension("json.unusable");
    std::fs::rename(&path, &aside)?;
    Ok(aside)
}

/// What the window learns about the saved tabs at launch, and what to log: `None` when there is
/// nothing to offer (no state directory, no file, a file that lists no tab, or one that cannot be
/// used -- which is set aside here, once, so the window's own later write does not replace it).
pub fn startup(dir: Option<&Path>, project_root: &Path) -> (Option<SavedTabs>, Vec<String>) {
    startup_as(dir, project_root, Role::Main)
}

/// [`startup`] of the file of a window in `role`.
pub fn startup_as(dir: Option<&Path>, project_root: &Path, role: Role) -> (Option<SavedTabs>, Vec<String>) {
    let Some(dir) = dir else { return (None, Vec::new()) };
    match load_as(dir, project_root, role) {
        Loaded::Saved(saved) if saved.is_empty() => (None, Vec::new()),
        Loaded::Saved(saved) => (Some(saved), Vec::new()),
        Loaded::Missing => (None, Vec::new()),
        Loaded::Unusable(why) => {
            let mut notes = vec![format!(
                "[tabs] the saved tabs are unusable ({why}); starting without them"
            )];
            match set_aside_as(dir, project_root, role) {
                Ok(aside) => notes.push(format!("[tabs] set aside as {}", aside.display())),
                Err(e) => notes.push(format!("[tabs] could not set the unusable file aside: {e}")),
            }
            (None, notes)
        }
    }
}

/// Another window's saved tabs offered by this one: the claim holding them (`<name>.json.offered`), renamed
/// `taken` (`<name>.json.taken`) once this window's own file holds every tab of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Takeover {
    pub path: PathBuf,
    pub taken: PathBuf,
    /// Each tab of the file, by agent and session.
    pub sessions: Vec<(AgentId, String)>,
}

/// What a window offers back at launch: its own saved tabs ([`startup_as`]), then those of each window of
/// `others` that has a usable file and is not this window's -- window 2's when window 1 takes back what window 2
/// held, an unpaired window's when the pair's window is alone on the project. A tab listed by an earlier file is
/// not offered twice. Which tab comes on screen is this window's own choice, or the first other file's when this
/// window saved none. Each other window's tabs are claimed here ([`claim`]), and each claim that offered a tab
/// comes back as a [`Takeover`], for the window's tab writer to rename once it has written them.
pub fn startup_with(
    dir: Option<&Path>,
    project_root: &Path,
    role: Role,
    others: &[Role],
) -> (Option<SavedTabs>, Vec<String>, Vec<Takeover>) {
    let (own, mut notes) = startup_as(dir, project_root, role);
    let (offer, takeovers) = offer_others(dir, project_root, role, others, own, &mut notes);
    (offer, notes, takeovers)
}

/// [`startup_with`] without this window's own file: only the other windows' of `others`, for a window that offered
/// its own earlier in the launch and has more to offer now.
pub fn startup_others(
    dir: Option<&Path>,
    project_root: &Path,
    role: Role,
    others: &[Role],
) -> (Option<SavedTabs>, Vec<String>, Vec<Takeover>) {
    let mut notes = Vec::new();
    let (offer, takeovers) = offer_others(dir, project_root, role, others, None, &mut notes);
    (offer, notes, takeovers)
}

/// `own`, then the tabs of each window of `others` that has a usable file and is not `role`, each claimed.
fn offer_others(
    dir: Option<&Path>,
    project_root: &Path,
    role: Role,
    others: &[Role],
    own: Option<SavedTabs>,
    notes: &mut Vec<String>,
) -> (Option<SavedTabs>, Vec<Takeover>) {
    let Some(dir) = dir else {
        return (own, Vec::new());
    };
    let mut offer = own;
    let mut takeovers = Vec::new();
    for other in others.iter().copied().filter(|other| *other != role) {
        let Some(saved) = claim(dir, project_root, other, notes) else {
            continue;
        };
        let name = file_name_as(project_root, other);
        let path = dir.join(claim_name(&name));
        notes.push(format!(
            "[tabs] offering {} saved tab(s) of {} after this window's own",
            saved.tabs.len(),
            path.display()
        ));
        takeovers.push(Takeover {
            path,
            taken: dir.join(format!("{name}.taken")),
            sessions: saved
                .tabs
                .iter()
                .map(|t| (t.agent.clone(), t.provider_session_id.clone()))
                .collect(),
        });
        offer = Some(match offer {
            None => saved,
            Some(first) => first.followed_by(saved),
        });
    }
    (offer, takeovers)
}

/// Where the offering window keeps another window's tabs it claimed: beside that window's file, a name no other
/// window writes.
fn claim_name(file_name: &str) -> String {
    format!("{file_name}.offered")
}

/// Where the offering window moves another window's file before reading it: a name no other window writes, so the
/// tabs read are exactly the tabs claimed, whatever that window writes meanwhile.
fn staging_name(file_name: &str) -> String {
    format!("{file_name}.claiming")
}

/// Another window's saved tabs, claimed for this window's offer: its file is moved to its claim
/// ([`claim_name`]), so the window it belonged to can write its own file again, from its own tabs, without the
/// claimed ones leaving every file while this window's offer is still pending. A claim an earlier launch left
/// (its tabs never all came back) is offered again, and a file the other window wrote since is added after it.
/// The other window's file is moved to [`staging_name`] before it is read, never read and then moved: that window
/// may write a newer file between the two, and a read-then-move would claim tabs it never offered. A staged file an
/// interrupted launch left is taken first.
/// `None`: nothing to offer, or nothing that can be offered without writing over a file a person may want to read.
fn claim(dir: &Path, project_root: &Path, other: Role, notes: &mut Vec<String>) -> Option<SavedTabs> {
    let name = file_name_as(project_root, other);
    let path = dir.join(&name);
    let claimed = dir.join(claim_name(&name));
    let staged = dir.join(staging_name(&name));
    let mut offer = match load_path(&claimed, project_root) {
        Loaded::Saved(saved) => Some(saved).filter(|s| !s.is_empty()),
        Loaded::Missing => None,
        Loaded::Unusable(why) => {
            notes.push(format!(
                "[tabs] the saved tabs claimed at an earlier launch are unusable ({why}); not offered"
            ));
            let aside = claimed.with_extension("offered.unusable");
            match std::fs::rename(&claimed, &aside) {
                Ok(()) => notes.push(format!("[tabs] set aside as {}", aside.display())),
                Err(e) => {
                    // Claiming again would write over the one copy of what a person may want to read.
                    notes.push(format!(
                        "[tabs] could not set them aside ({e}); {} is not offered",
                        path.display()
                    ));
                    return None;
                }
            }
            None
        }
    };
    // A file staged by a launch that ended mid-claim is taken as it stands; the other window's current file is
    // then moved onto the staged name, which is free again only once that one is dealt with.
    for current_file in [false, true] {
        if current_file {
            if staged.exists() {
                notes.push(format!(
                    "[tabs] {} is still being claimed; {} is not claimed this time",
                    staged.display(),
                    path.display()
                ));
                break;
            }
            match std::fs::rename(&path, &staged) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                Err(e) => {
                    notes.push(format!("[tabs] could not claim {} ({e}); not offered", path.display()));
                    break;
                }
            }
        }
        let current = match load_path(&staged, project_root) {
            Loaded::Saved(saved) if !saved.is_empty() => saved,
            Loaded::Missing => continue,
            Loaded::Saved(_) => {
                put_back(&staged, &path, None, notes);
                continue;
            }
            Loaded::Unusable(why) => {
                notes.push(format!(
                    "[tabs] another window's saved tabs are unusable ({why}); not offered"
                ));
                put_back(&staged, &path, Some(dir.join(format!("{name}.unusable"))), notes);
                continue;
            }
        };
        match offer.take() {
            None => match std::fs::rename(&staged, &claimed) {
                Ok(()) => offer = Some(current),
                Err(e) => {
                    notes.push(format!(
                        "[tabs] could not claim {} ({e}); not offered",
                        staged.display()
                    ));
                    break;
                }
            },
            Some(earlier) => {
                let both = earlier.clone().followed_by(current);
                // Removed only once the claim holds its tabs; a staged file left by a failed removal is added
                // again, once, at the next launch.
                match save_path(dir, claimed.clone(), project_root, &both).and_then(|_| std::fs::remove_file(&staged)) {
                    Ok(()) => offer = Some(both),
                    Err(e) => {
                        notes.push(format!(
                            "[tabs] could not add {} to the tabs claimed earlier ({e}); offering those",
                            staged.display()
                        ));
                        offer = Some(earlier);
                        break;
                    }
                }
            }
        }
    }
    offer
}

/// A staged file that is not claimed goes back where the other window keeps it, unless that window wrote a newer
/// file there meanwhile: the link fails rather than replace it. The staged one is then dropped if it held no tabs,
/// or kept at `aside` for a person to read; when neither can be done it stays staged, and no later launch claims
/// that window's file until a person moves it.
fn put_back(staged: &Path, path: &Path, aside: Option<PathBuf>, notes: &mut Vec<String>) {
    let back = match std::fs::hard_link(staged, path) {
        Ok(()) => std::fs::remove_file(staged),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => match &aside {
            // Linked, not renamed: an earlier unusable file already there is kept too.
            Some(aside) => std::fs::hard_link(staged, aside)
                .and_then(|()| std::fs::remove_file(staged))
                .map(|()| notes.push(format!("[tabs] set aside as {}", aside.display()))),
            None => std::fs::remove_file(staged),
        },
        Err(e) => Err(e),
    };
    if let Err(e) = back {
        notes.push(format!("[tabs] could not put {} back ({e})", staged.display()));
    }
}

/// Which saved tabs a window offers back and which file it writes: its own role's, and after its own the files of
/// `also_offer` (another window's, claimed, then renamed `.taken` once this window's file holds them). `offers_own` false: the
/// window offers nothing at launch, though it writes its own file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabFiles {
    pub role: Role,
    pub also_offer: Vec<Role>,
    pub offers_own: bool,
}

impl Default for TabFiles {
    /// A window alone, or the pair's window 1 with nothing else to offer: today's file.
    fn default() -> Self {
        TabFiles {
            role: Role::Main,
            also_offer: Vec::new(),
            offers_own: true,
        }
    }
}

impl TabFiles {
    /// The launch's offer, its notes, and the other windows' files to take over once written
    /// ([`startup_with`]).
    pub fn startup(&self, dir: Option<&Path>, project_root: &Path) -> (Option<SavedTabs>, Vec<String>, Vec<Takeover>) {
        if self.offers_own {
            startup_with(dir, project_root, self.role, &self.also_offer)
        } else {
            (None, Vec::new(), Vec::new())
        }
    }

    /// What these files offer beyond `given`, the files offered earlier in the launch: each other window's file not
    /// offered yet, and this window's own if it was not. With nothing given, [`TabFiles::startup`].
    pub fn startup_beyond(
        &self,
        given: Option<&TabFiles>,
        dir: Option<&Path>,
        project_root: &Path,
    ) -> (Option<SavedTabs>, Vec<String>, Vec<Takeover>) {
        let Some(given) = given else {
            return self.startup(dir, project_root);
        };
        let others: Vec<Role> = self
            .also_offer
            .iter()
            .copied()
            .filter(|role| !given.also_offer.contains(role))
            .collect();
        if self.offers_own && !given.offers_own {
            startup_with(dir, project_root, self.role, &others)
        } else {
            startup_others(dir, project_root, self.role, &others)
        }
    }

    /// The window's tab writer: its own role's file, taking over `takeovers` once written.
    pub fn writer(&self, dir: Option<PathBuf>, project_root: PathBuf, takeovers: Vec<Takeover>) -> TabWriter {
        let mut writer = TabWriter::new_as(dir, project_root, self.role);
        writer.take_over_when_written(takeovers);
        writer
    }
}

impl SavedTabs {
    /// These tabs, then `later`'s that are not among them, each kind in order; the tab on screen stays this set's.
    pub fn followed_by(self, later: SavedTabs) -> SavedTabs {
        let on_screen = |set: &SavedTabs, index: Option<usize>| {
            index
                .and_then(|i| set.tabs.get(i))
                .map(|t| (t.agent.clone(), t.provider_session_id.clone()))
        };
        let (active, claude_active) = (on_screen(&self, self.active), on_screen(&self, self.claude_active));
        // This set's tabs in the bar's order, then the later set's that are new, in the order that set had them.
        let mut bar: Vec<(AgentId, String)> = self.in_bar_order().into_iter().map(SavedTab::key).collect();
        for tab in later.in_bar_order() {
            if !bar.contains(&tab.key()) {
                bar.push(tab.key());
            }
        }
        let mut all = self.tabs;
        for tab in later.tabs {
            if !all
                .iter()
                .any(|t| t.agent == tab.agent && t.provider_session_id == tab.provider_session_id)
            {
                all.push(tab);
            }
        }
        let (claude, others): (Vec<SavedTab>, Vec<SavedTab>) = all.into_iter().partition(|t| !t.agent.is_external());
        let at = |tabs: &[SavedTab], key: &Option<(AgentId, String)>| {
            key.as_ref().and_then(|(agent, id)| {
                tabs.iter()
                    .position(|t| t.agent == *agent && t.provider_session_id == *id)
            })
        };
        let claude_at = at(&claude, &claude_active).unwrap_or(0);
        let others_at = at(&others, &active);
        let mut remote = self.remote;
        for tab in later.remote {
            if !remote.iter().any(|kept| kept.same_agent(&tab)) {
                remote.push(tab);
            }
        }
        SavedTabs::from_parts(claude, claude_at, others, others_at)
            .with_bar(bar)
            .with_remote(remote)
    }

    /// Whether every one of `sessions` is listed here.
    fn holds_all(&self, sessions: &[(AgentId, String)]) -> bool {
        sessions.iter().all(|(agent, id)| {
            self.tabs
                .iter()
                .any(|t| t.agent == *agent && t.provider_session_id == *id)
        })
    }
}

/// Keeps the saved file in step with the window's tabs.
///
/// It is shown the window's tabs as often as the window likes ([`TabMemory::observe`]) and writes only
/// when they differ from the last it saw. It starts from "no tabs", so a launch that has done nothing
/// yet writes nothing and leaves the previous window's file alone for the launch to offer.
pub struct TabMemory {
    /// `None`: no state directory, or the feature is off -- nothing is ever written.
    dir: Option<PathBuf>,
    project_root: PathBuf,
    /// Whose file in the pair this window writes.
    role: Role,
    last: SavedTabs,
}

impl TabMemory {
    pub fn new(dir: Option<PathBuf>, project_root: PathBuf) -> Self {
        Self::new_as(dir, project_root, Role::Main)
    }

    /// The memory of a window in `role`, which writes that role's file.
    pub fn new_as(dir: Option<PathBuf>, project_root: PathBuf, role: Role) -> Self {
        TabMemory {
            dir,
            project_root,
            role,
            last: SavedTabs::default(),
        }
    }

    /// Stops all writing, for good: a window that has been told not to remember its tabs.
    pub fn disable(&mut self) {
        self.dir = None;
    }

    pub fn is_enabled(&self) -> bool {
        self.dir.is_some()
    }

    /// `now` is the window's tabs, or `None` while the window is closing and has none to show. Returns
    /// what a write did (`Some`) or that none was needed (`None`).
    ///
    /// A write that fails is reported once and not tried again until the tabs change: this is
    /// called on every tick, and a full disk must not turn into thirty failures a second. The
    /// window's own [`TabWriter`] tries again by itself.
    ///
    /// A file found unusable just before the write -- hand-edited, or from another build, since the
    /// window opened -- is set aside first rather than overwritten, as the layout file's is.
    pub fn observe(&mut self, now: Option<SavedTabs>) -> Option<Result<Written, std::io::Error>> {
        let now = now?;
        if now == self.last {
            return None;
        }
        self.write(now)
    }

    /// Writes `now` whatever was written last; `None` when nothing is ever written. For a caller that has
    /// already decided `now` is a change: [`TabWriter`]'s thread, whose sets are compared on the window's
    /// thread, and which may never have been handed the set this one replaced.
    fn write(&mut self, now: SavedTabs) -> Option<Result<Written, std::io::Error>> {
        let dir = self.dir.as_ref()?;
        self.last = now.clone();
        // A file that cannot be moved aside is left where it is: writing over it would destroy the one
        // copy of what a person may want to read, so the write is given up and said.
        let aside = match load_as(dir, &self.project_root, self.role) {
            Loaded::Unusable(_) => match set_aside_as(dir, &self.project_root, self.role) {
                Ok(aside) => Some(aside),
                Err(e) => return Some(Err(e)),
            },
            _ => None,
        };
        Some(save_as(dir, &self.project_root, self.role, &now).map(|path| Written { path, set_aside: aside }))
    }
}

/// A completed write.
#[derive(Debug, PartialEq, Eq)]
pub struct Written {
    pub path: PathBuf,
    /// Where an unusable file that stood in the way went, if one did.
    pub set_aside: Option<PathBuf>,
}

/// [`TabMemory`] on a thread of its own, so the window's thread never touches the disk for it.
///
/// [`observe`](Self::observe) only compares the tabs with the last set handed over and, when they differ,
/// leaves the new set for the thread, replacing one it has not taken yet: the last set wins, and since one
/// thread writes them in the order it takes them, the file always ends at the newest. What a write did is
/// logged by the thread. [`finish`](Self::finish) at the window's close hands over the last set and
/// returns a receiver that reports once the thread has written it and ended, which the close watches
/// under a backstop: a write is a temporary file renamed over the old one, so a process that leaves
/// before it ends keeps the previous file whole.
pub struct TabWriter {
    /// `None`: nothing is ever written (no state directory, or remembering is off).
    dir: Option<PathBuf>,
    project_root: PathBuf,
    /// Whose file in the pair this window writes.
    role: Role,
    /// Other windows' files whose tabs this window offered, handed to the thread at its start.
    takeovers: Vec<Takeover>,
    /// The last set handed over, to hand over only a change.
    last: SavedTabs,
    /// Started at the first write.
    worker: Option<WriterHandle>,
    #[cfg(any(test, feature = "test-support"))]
    before_write: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

struct WriterHandle {
    shared: std::sync::Arc<WriterShared>,
    done: Option<std::sync::mpsc::Receiver<()>>,
}

#[derive(Default)]
struct WriterShared {
    queue: std::sync::Mutex<WriterQueue>,
    ready: std::sync::Condvar,
}

/// How long the writer waits before writing a set again whose write failed: space freed later brings the file up to
/// date with no change to the tabs and no tick to ask for it.
const FAILED_WRITE_RETRY: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Default)]
struct WriterQueue {
    dir: Option<PathBuf>,
    next: Option<SavedTabs>,
    /// Taken by the thread and being written now.
    writing: bool,
    /// No more sets come: the thread ends once `next` is written.
    finished: bool,
    /// Other windows' files to rename `.taken` once a set written holds all their tabs.
    takeovers: Vec<Takeover>,
    /// Remembering was turned off: what is queued is dropped and nothing more is written.
    off: bool,
}

impl TabWriter {
    pub fn new(dir: Option<PathBuf>, project_root: PathBuf) -> Self {
        Self::new_as(dir, project_root, Role::Main)
    }

    /// The writer of a window in `role`, which writes that role's file.
    pub fn new_as(dir: Option<PathBuf>, project_root: PathBuf, role: Role) -> Self {
        TabWriter {
            dir,
            project_root,
            role,
            takeovers: Vec::new(),
            last: SavedTabs::default(),
            worker: None,
            #[cfg(any(test, feature = "test-support"))]
            before_write: None,
        }
    }

    /// Whose file in the pair this writer writes.
    pub fn role(&self) -> Role {
        self.role
    }

    /// Writes into `dir` from now on. The existing worker serializes the move after any in-flight write, then flushes
    /// the latest set there. A disabled writer stays disabled. No disk work runs on the caller's thread.
    pub fn move_to(&mut self, dir: PathBuf) -> Result<(), &'static str> {
        if self.dir.is_some() {
            if let Some(worker) = &self.worker {
                let mut queue = worker.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
                if queue.finished {
                    return Err("the saved-tabs writer has finished");
                }
                queue.dir = Some(dir.clone());
                queue.next = Some(self.last.clone());
                worker.shared.ready.notify_all();
            }
            self.dir = Some(dir);
        }
        Ok(())
    }

    pub fn rekey_remote(&mut self, old: &str, new: &str) {
        self.last.rekey_remote(old, new);
    }

    /// `saved` is what the file holds already, read by the caller: the next set is a change from it, so one with no tabs
    /// left empties the file.
    pub fn starts_from(&mut self, saved: SavedTabs) {
        self.last = saved;
    }

    /// Stops all writing, for good, a set still queued included.
    pub fn disable(&mut self) {
        self.dir = None;
        if let Some(worker) = &self.worker {
            let mut queue = worker.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            queue.off = true;
            queue.next = None;
            worker.shared.ready.notify_all();
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.dir.is_some()
    }

    /// Other windows' tabs this window claimed and offered ([`startup_with`]): each claim is renamed `.taken` on
    /// the writer thread right after a write of this window's own file that holds every tab of it, and not before.
    /// One whose tabs never all come back here stays claimed, to be offered again.
    pub fn take_over_when_written(&mut self, takeovers: Vec<Takeover>) {
        match &self.worker {
            Some(worker) => {
                let mut queue = worker.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
                queue.takeovers.extend(takeovers);
            }
            None => self.takeovers.extend(takeovers),
        }
    }

    /// `now` is the window's tabs, or `None` while the window is closing and has none to show. Hands a
    /// changed set to the writer and returns at once; touches no disk.
    pub fn observe(&mut self, now: Option<SavedTabs>) {
        let Some(dir) = self.dir.clone() else { return };
        let Some(now) = now else { return };
        if now == self.last {
            return;
        }
        self.last = now.clone();
        let Some(worker) = self.worker_for(dir) else { return };
        let mut queue = worker.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.next = Some(now);
        worker.shared.ready.notify_all();
    }

    /// No more sets come: the writer writes what it was last handed and ends, trying once more a set whose write
    /// failed. The receiver reports once it has (or is gone when the thread died); `None` when no writer ever
    /// started, so nothing is owed.
    pub fn finish(&mut self) -> Option<std::sync::mpsc::Receiver<()>> {
        let worker = self.worker.as_mut()?;
        {
            let mut queue = worker.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            queue.finished = true;
            worker.shared.ready.notify_all();
        }
        worker.done.take()
    }

    /// Runs `hook` on the writer thread before each write, for a test that holds a write under way. Set before
    /// the first hand-over: the thread takes it when it starts.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_before_write(&mut self, hook: std::sync::Arc<dyn Fn() + Send + Sync>) {
        self.before_write = Some(hook);
    }

    /// Waits until the writer has written everything handed to it, for a test to read the file.
    #[cfg(any(test, feature = "test-support"))]
    pub fn wait_written(&self, within: std::time::Duration) -> bool {
        let Some(worker) = &self.worker else { return true };
        let deadline = std::time::Instant::now() + within;
        let mut queue = worker.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        while queue.next.is_some() || queue.writing {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return false;
            }
            queue = worker
                .shared
                .ready
                .wait_timeout(queue, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        true
    }

    fn worker_for(&mut self, dir: PathBuf) -> Option<&WriterHandle> {
        if self.worker.is_none() {
            let shared = std::sync::Arc::new(WriterShared::default());
            shared.queue.lock().unwrap_or_else(|e| e.into_inner()).takeovers = std::mem::take(&mut self.takeovers);
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let memory = TabMemory::new_as(Some(dir), self.project_root.clone(), self.role);
            let thread_shared = shared.clone();
            #[cfg(any(test, feature = "test-support"))]
            let before_write = self.before_write.clone();
            let spawned = std::thread::Builder::new()
                .name("eitri-saved-tabs".into())
                .spawn(move || {
                    #[cfg(any(test, feature = "test-support"))]
                    let hook = move || {
                        if let Some(hook) = &before_write {
                            hook();
                        }
                    };
                    #[cfg(not(any(test, feature = "test-support")))]
                    let hook = || {};
                    write_loop(&thread_shared, memory, hook, FAILED_WRITE_RETRY);
                    let _ = done_tx.send(());
                });
            if let Err(e) = spawned {
                eprintln!("[tabs] the saved-tabs writer could not start ({e}); the open tabs are not kept");
                self.dir = None;
                return None;
            }
            self.worker = Some(WriterHandle {
                shared,
                done: Some(done_rx),
            });
        }
        self.worker.as_ref()
    }
}

/// A writer dropped without [`TabWriter::finish`] (a window's state that went away some other way) still lets
/// its thread end: it writes what it was last handed and returns. Nothing here waits for it.
impl Drop for TabWriter {
    fn drop(&mut self) {
        if let Some(worker) = &self.worker {
            let mut queue = worker.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            queue.finished = true;
            worker.shared.ready.notify_all();
        }
    }
}

/// The writer thread: takes the newest set, writes it outside the lock, and ends once finished and empty.
///
/// A set whose write failed stays here and not in the queue: a newer set replaces it, a wait of `retry` writes it
/// again, and the close tries it once more rather than wait. The thread keeps its own clock because an idle panel
/// has no tick to ask.
fn write_loop(shared: &WriterShared, mut memory: TabMemory, before_write: impl Fn(), retry: std::time::Duration) {
    let mut failed: Option<SavedTabs> = None;
    let mut closing_try_made = false;
    loop {
        let next = {
            let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if queue.off {
                    queue.writing = false;
                    shared.ready.notify_all();
                    return;
                }
                if let Some(next) = queue.next.take() {
                    if let Some(dir) = queue.dir.take() {
                        memory = TabMemory::new_as(Some(dir), memory.project_root.clone(), memory.role);
                    }
                    queue.writing = true;
                    break next;
                }
                if queue.finished {
                    // Once: a close on a disk still full ends here, and the file keeps what it last held.
                    match failed.take().filter(|_| !closing_try_made) {
                        Some(owed) => {
                            closing_try_made = true;
                            queue.writing = true;
                            break owed;
                        }
                        None => return,
                    }
                }
                let Some(owed) = failed.take() else {
                    queue = shared.ready.wait(queue).unwrap_or_else(|e| e.into_inner());
                    continue;
                };
                let (again, waited) = shared
                    .ready
                    .wait_timeout(queue, retry)
                    .unwrap_or_else(|e| e.into_inner());
                queue = again;
                if waited.timed_out() {
                    queue.writing = true;
                    break owed;
                }
                failed = Some(owed);
            }
        };
        before_write();
        let attempted = next.clone();
        // Not `observe`: the window's thread compared this set with the last one it handed over, and the set
        // in between may never have been taken here (replaced while queued), so this thread's own last set
        // can be stale. An emptied window, the set before it never taken, must still empty the file.
        let result = memory.write(next);
        if matches!(result, Some(Ok(_))) {
            let due: Vec<Takeover> = {
                let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
                let (due, waiting) = std::mem::take(&mut queue.takeovers)
                    .into_iter()
                    .partition(|t| attempted.holds_all(&t.sessions));
                queue.takeovers = waiting;
                due
            };
            for takeover in due {
                match std::fs::rename(&takeover.path, &takeover.taken) {
                    Ok(()) => eprintln!(
                        "[tabs] this window's file holds them now; {} kept as {}",
                        takeover.path.display(),
                        takeover.taken.display()
                    ),
                    Err(e) => eprintln!("[tabs] could not mark {} taken: {e}", takeover.path.display()),
                }
            }
        }
        match result {
            Some(Err(e)) => {
                eprintln!("[tabs] could not save the open tabs: {e}");
                agent::save_health::failed(agent::save_health::SaveKind::Tabs, &e);
                failed = Some(attempted);
            }
            Some(Ok(written)) => {
                if let Some(aside) = written.set_aside {
                    eprintln!(
                        "[tabs] an unusable saved-tabs file was set aside as {}",
                        aside.display()
                    );
                }
                agent::save_health::saved(agent::save_health::SaveKind::Tabs);
                failed = None;
            }
            None => {}
        }
        let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.writing = false;
        shared.ready.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_paths::file_name;
    use crate::test_scratch_dir::ScratchDir;

    fn root() -> PathBuf {
        PathBuf::from("/home/user/project")
    }

    /// The bar's order is a list of keys beside the tabs, so each way a set is split, joined or read keeps each tab's
    /// draft on its own tab.
    #[test]
    fn a_draft_stays_on_its_tab_through_the_file_the_windows_and_the_bar() {
        let typed = |agent: AgentId, id: &str, window: Option<u8>| SavedTab {
            agent,
            window,
            draft: Some(format!("typed for {id}")),
            provider_session_id: id.to_string(),
            ..tab("unused", None, SessionModeChoice::Auto)
        };
        let drafts = |saved: &SavedTabs| -> Vec<(String, Option<String>)> {
            saved
                .in_bar_order()
                .into_iter()
                .map(|t| (t.provider_session_id.clone(), t.draft.clone()))
                .collect()
        };
        let want = |ids: &[&str]| -> Vec<(String, Option<String>)> {
            ids.iter()
                .map(|id| (id.to_string(), Some(format!("typed for {id}"))))
                .collect()
        };
        let saved = SavedTabs::from_parts(
            vec![typed(AgentId::Claude, "a", None), typed(AgentId::Claude, "b", Some(2))],
            0,
            vec![typed(AgentId::Codex, "x", None)],
            None,
        )
        .with_bar(vec![
            (AgentId::Claude, "b".to_string()),
            (AgentId::Codex, "x".to_string()),
            (AgentId::Claude, "a".to_string()),
        ]);
        assert_eq!(drafts(&saved), want(&["b", "x", "a"]));
        let read = decode(&encode(&saved, &root()), &root()).unwrap();
        assert_eq!(drafts(&read), want(&["b", "x", "a"]), "the file");
        let crossed = SavedTabs::from_wire(&saved.to_wire()).unwrap();
        assert_eq!(drafts(&crossed), want(&["b", "x", "a"]), "the link");
        assert_eq!(
            drafts(&tabs_for(&saved, &WindowName::Chat(2))),
            want(&["b"]),
            "one window's part"
        );
        let later = SavedTabs::from_parts(vec![typed(AgentId::Claude, "c", None)], 0, Vec::new(), None);
        assert_eq!(
            drafts(&saved.clone().followed_by(later)),
            want(&["b", "x", "a", "c"]),
            "tabs still unopened behind the window's"
        );
    }

    /// A draft lives in the file and on the link, a tab with none reads as it always did, and a value of any other
    /// type is read as absent, since the error that would refuse it quotes it and a file's error goes to the log.
    #[test]
    fn a_draft_survives_the_file_and_the_link_and_never_reaches_an_error() {
        let mut tab = tab("s-a", None, SessionModeChoice::Auto);
        tab.draft = Some("half a thought\nand a line".to_string());
        let saved = SavedTabs::from_parts(vec![tab.clone()], 0, Vec::new(), None);
        let text = encode(&saved, &root());
        assert_eq!(decode(&text, &root()).unwrap().tabs[0].draft, tab.draft);
        assert_eq!(SavedTab::from_wire(&tab.to_wire()).unwrap().draft, tab.draft);

        tab.draft = None;
        let quiet = encode(&SavedTabs::from_parts(vec![tab], 0, Vec::new(), None), &root());
        assert!(!quiet.contains("draft"), "{quiet}");

        let odd = text.replace("\"half a thought\\nand a line\"", "12");
        assert_ne!(odd, text);
        assert_eq!(decode(&odd, &root()).unwrap().tabs[0].draft, None);
    }

    #[test]
    fn only_text_the_file_can_hold_is_a_saved_draft() {
        assert_eq!(draft_to_save("  \n "), None);
        assert_eq!(draft_to_save("text").as_deref(), Some("text"));
        assert!(draft_to_save(&"x".repeat(DRAFT_SAVE_MAX)).is_some());
        assert_eq!(draft_to_save(&"x".repeat(DRAFT_SAVE_MAX + 1)), None);
    }

    /// A failed write leaves the file behind tabs that have not changed since. The writer writes them again by
    /// itself, with no tick to ask; a newer set replaces them; and the close tries once more rather than wait.
    #[test]
    fn a_write_that_failed_is_written_again_with_nothing_changing() {
        let dir = ScratchDir::new("nv-saved-tabs", "retry");
        let state = dir.join("state");
        crate::test_scratch_dir::write_private(&state, "").unwrap();
        let first = SavedTabs::from_parts(vec![tab("s-one", None, SessionModeChoice::Auto)], 0, Vec::new(), None);
        let run = |retry: std::time::Duration, state: PathBuf, queued: SavedTabs| {
            let shared = std::sync::Arc::new(WriterShared::default());
            shared.queue.lock().unwrap().next = Some(queued);
            let thread = {
                let shared = shared.clone();
                std::thread::spawn(move || {
                    write_loop(
                        &shared,
                        TabMemory::new_as(Some(state), root(), Role::Main),
                        || {},
                        retry,
                    )
                })
            };
            (shared, thread)
        };
        let settled = |shared: &WriterShared| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let queue = shared.queue.lock().unwrap();
                if queue.next.is_none() && !queue.writing {
                    return;
                }
                drop(queue);
                assert!(std::time::Instant::now() < deadline, "the writer did not settle");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };

        let (shared, thread) = run(std::time::Duration::from_millis(20), state.clone(), first.clone());
        settled(&shared);
        assert!(!matches!(load(&state, &root()), Loaded::Saved(_)));
        std::fs::remove_file(&state).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while load(&state, &root()) != Loaded::Saved(first.clone()) {
            assert!(std::time::Instant::now() < deadline, "the set was not written again");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        shared.queue.lock().unwrap().finished = true;
        shared.ready.notify_all();
        thread.join().unwrap();

        // Far from its pause: only the close can write it.
        std::fs::remove_dir_all(&state).unwrap();
        crate::test_scratch_dir::write_private(&state, "").unwrap();
        let (shared, thread) = run(std::time::Duration::from_secs(600), state.clone(), first.clone());
        settled(&shared);
        std::fs::remove_file(&state).unwrap();
        shared.queue.lock().unwrap().finished = true;
        shared.ready.notify_all();
        thread.join().unwrap();
        assert_eq!(load(&state, &root()), Loaded::Saved(first.clone()));

        // A close on a disk still full tries once and ends: the window is waiting on this thread.
        std::fs::remove_dir_all(&state).unwrap();
        crate::test_scratch_dir::write_private(&state, "").unwrap();
        let (shared, thread) = run(std::time::Duration::from_secs(600), state.clone(), first);
        settled(&shared);
        shared.queue.lock().unwrap().finished = true;
        shared.ready.notify_all();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !thread.is_finished() {
            assert!(std::time::Instant::now() < deadline, "the writer never ended");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// A running turn is said in the file and on the link and nowhere else: a tab with none reads as it always did, and a
    /// file from before the key reads as no turn running.
    #[test]
    fn a_running_turn_survives_the_file_and_the_link_and_a_quiet_tab_says_nothing() {
        let root = Path::new("/project");
        let mut running = tab("s-1", None, SessionModeChoice::Auto);
        running.turn_running = true;
        let mut other = codex("s-2", SessionModeChoice::Auto);
        other.turn_running = true;
        let saved = SavedTabs::from_parts(vec![running.clone()], 0, vec![other.clone()], None);

        let text = encode(&saved, root);
        assert_eq!(text.matches("turn_running").count(), 2);
        assert_eq!(decode(&text, root).unwrap(), saved);
        assert_eq!(SavedTab::from_wire(&running.to_wire()).unwrap(), running);
        assert_eq!(SavedTab::from_wire(&other.to_wire()).unwrap(), other);

        let quiet = SavedTabs::from_parts(vec![tab("s-1", None, SessionModeChoice::Auto)], 0, Vec::new(), None);
        let text = encode(&quiet, root);
        assert!(!text.contains("turn_running"), "{text}");
        assert!(!decode(&text, root).unwrap().tabs[0].turn_running);
    }

    /// It stays with its tab through the group's one file, whichever window the tab was in.
    #[test]
    fn a_running_turn_stays_with_its_tab_through_the_groups_file() {
        let mut running = tab("s-2", None, SessionModeChoice::Auto);
        running.turn_running = true;
        let part = SavedTabs::from_parts(vec![running], 0, Vec::new(), None);
        let two = WindowName::Chat(2);
        let merged = merge_windows(&[(two.clone(), part)], Some(&two));

        assert!(merged.tabs[0].turn_running);
        assert!(tabs_for(&merged, &two).tabs[0].turn_running);
    }

    /// A tab saved while planning keeps where plan mode started, in the file and across the link; its `mode` is
    /// still the execution mode. Absent from both when it was not planning, so an older build reads the same file.
    #[test]
    fn plan_origin_round_trips_and_is_absent_when_none() {
        let root = Path::new("/work/project");
        let mut planning = tab("s-1", None, SessionModeChoice::Bypass);
        planning.plan_origin = Some(SessionModeChoice::Bypass);
        let plain = tab("s-2", None, SessionModeChoice::Auto);
        let saved = SavedTabs::from_parts(vec![planning.clone(), plain.clone()], 0, Vec::new(), None);
        let text = encode(&saved, root);
        assert_eq!(text.matches("plan_origin").count(), 1, "{text}");
        let read = decode(&text, root).unwrap();
        assert_eq!(read.tabs[0].plan_origin, Some(SessionModeChoice::Bypass));
        assert_eq!(read.tabs[0].mode, SessionModeChoice::Bypass);
        assert_eq!(read.tabs[1].plan_origin, None);
        assert_eq!(SavedTab::from_wire(&planning.to_wire()).unwrap(), planning);
        assert!(plain.to_wire().get("plan_origin").is_none());
    }

    fn tab(session: &str, name: Option<&str>, mode: SessionModeChoice) -> SavedTab {
        SavedTab {
            agent: AgentId::Claude,
            conversation_id: "0123456789abcdef0123456789abcdef".to_string(),
            provider_session_id: session.to_string(),
            name: name.map(str::to_string),
            mode,
            view: None,
            window: None,
            turn_running: false,
            draft: None,
            row_key: None,
            plan_origin: None,
        }
    }

    fn two() -> SavedTabs {
        SavedTabs::from_parts(
            vec![
                tab("s-one", Some("api"), SessionModeChoice::Auto),
                tab("s-two", None, SessionModeChoice::Bypass),
            ],
            1,
            Vec::new(),
            None,
        )
    }

    /// `base`, a set of Claude tabs only, with `others` after them.
    fn and_others(base: SavedTabs, others: Vec<SavedTab>, others_active: Option<usize>) -> SavedTabs {
        let claude_active = base.rank(base.claude_active, false).unwrap_or(0);
        SavedTabs::from_parts(base.tabs, claude_active, others, others_active)
    }

    #[test]
    fn a_tab_crosses_the_link_whole_and_what_is_no_tab_is_refused() {
        for agent in [AgentId::Claude, AgentId::Codex, AgentId::Acp("gemini".into())] {
            let mut tab = tab("s-1", Some("work"), SessionModeChoice::Bypass);
            tab.agent = agent;
            tab.view = Some(3);
            assert_eq!(SavedTab::from_wire(&tab.to_wire()), Ok(tab));
        }
        let mut blank = tab("  ", None, SessionModeChoice::Auto).to_wire();
        assert!(SavedTab::from_wire(&blank).is_err(), "no session");
        blank["agent"] = serde_json::json!("nobody");
        assert!(SavedTab::from_wire(&blank).is_err(), "no such agent");
        assert!(SavedTab::from_wire(&serde_json::json!("s-1")).is_err());
    }

    #[test]
    fn a_tab_in_a_chat_view_keeps_its_view_and_a_build_before_views_still_reads_the_file() {
        let mut base = two();
        base.tabs[1].view = Some(3);
        let saved = and_others(
            base,
            vec![SavedTab {
                view: Some(2),
                window: None,
                ..codex("thread-a", SessionModeChoice::Auto)
            }],
            None,
        );
        let text = encode(&saved, &root());
        assert_eq!(decode(&text, &root()), Ok(saved.clone()));
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["version"], 1, "the version stays 1");
        assert!(value["tabs"][0].get("view").is_none(), "a base-view tab writes no view");
        assert_eq!(value["tabs"][1]["view"], 3);
        assert_eq!(value["external_tabs"][0]["view"], 2);
        assert!(
            !encode(&two(), &root()).contains("view"),
            "a window with no chat view writes what it always wrote"
        );

        // The entries as a build before views has them: serde ignores the key it does not know.
        #[derive(Deserialize)]
        struct BeforeViewsTab {
            provider_session_id: String,
            mode: SessionModeChoice,
        }
        #[derive(Deserialize)]
        struct BeforeViewsExternal {
            agent: AgentId,
            provider_session_id: String,
        }
        #[derive(Deserialize)]
        struct BeforeViewsFile {
            tabs: Vec<BeforeViewsTab>,
            external_tabs: Vec<BeforeViewsExternal>,
        }
        let older: BeforeViewsFile = serde_json::from_str(&text).expect("a build before views reads the file");
        assert_eq!(older.tabs[1].provider_session_id, "s-two");
        assert_eq!(older.tabs[1].mode, SessionModeChoice::Bypass);
        assert_eq!(older.external_tabs[0].agent, AgentId::Codex);
        assert_eq!(older.external_tabs[0].provider_session_id, "thread-a");
    }

    #[test]
    fn a_view_that_no_chat_view_can_have_is_read_as_none() {
        for bad in ["0", "1", "10", "300", "-1", "2.5", "\"2\"", "null", "true"] {
            let text = format!(
                r#"{{"version":1,"project_root":"/home/user/project","tabs":[
                {{"conversation_id":"c","provider_session_id":"s","mode":"auto","view":{bad}}}],"active":0,
                "external_tabs":[{{"agent":"codex","conversation_id":"c","provider_session_id":"t","mode":"auto","view":{bad}}}]}}"#
            );
            let read = decode(&text, &root()).unwrap_or_else(|e| panic!("{bad}: {e}"));
            assert_eq!(read.tabs[0].view, None, "{bad}");
            assert_eq!(read.other_tabs().next().unwrap().view, None, "{bad}");
        }
        for good in 2..=9u8 {
            let text = format!(
                r#"{{"version":1,"project_root":"/home/user/project","tabs":[
                {{"conversation_id":"c","provider_session_id":"s","mode":"auto","view":{good}}}],"active":0}}"#
            );
            assert_eq!(decode(&text, &root()).unwrap().tabs[0].view, Some(good));
        }
    }

    #[test]
    fn it_lives_beside_the_layout_and_agent_directories() {
        assert_eq!(
            state_dir(Some(OsStr::new("/s")), Some(OsStr::new("/h"))),
            Some(PathBuf::from("/s/eitri/tabs"))
        );
        assert_eq!(
            state_dir(None, Some(OsStr::new("/h"))),
            Some(PathBuf::from("/h/.local/state/eitri/tabs"))
        );
        assert_eq!(state_dir(Some(OsStr::new("relative")), None), None);
    }

    #[test]
    fn the_file_round_trips_with_order_names_modes_and_the_active_tab() {
        let saved = two();
        let text = encode(&saved, &root());
        assert_eq!(decode(&text, &root()), Ok(saved));
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["active"], 1);
        assert_eq!(value["tabs"][0]["name"], "api");
        assert_eq!(value["tabs"][0]["mode"], "auto");
        assert_eq!(value["tabs"][1]["mode"], "bypass");
        assert!(value["tabs"][1].get("name").is_none(), "no rename, no key");
    }

    /// A tab saved in manual is written as the word `manual` and read back as manual.
    #[test]
    fn a_manual_tab_is_saved_as_manual_and_read_back() {
        let saved = SavedTabs::from_parts(
            vec![tab("m", None, SessionModeChoice::Manual)],
            0,
            vec![SavedTab {
                agent: AgentId::Codex,
                conversation_id: "c".to_string(),
                provider_session_id: "x".to_string(),
                name: None,
                mode: SessionModeChoice::Manual,
                view: None,
                window: None,
                turn_running: false,
                draft: None,
                row_key: None,
                plan_origin: None,
            }],
            None,
        );
        let text = encode(&saved, &root());
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["tabs"][0]["mode"], "manual");
        assert_eq!(value["external_tabs"][0]["mode"], "manual");
        assert_eq!(decode(&text, &root()).unwrap(), saved);
    }

    #[test]
    fn a_wrong_version_another_projects_file_or_a_broken_one_cannot_be_decoded() {
        let text = encode(&two(), &root());
        let newer = text.replace("\"version\": 1", "\"version\": 2");
        assert!(decode(&newer, &root()).unwrap_err().contains("version 2"));
        assert!(decode(&text, Path::new("/elsewhere"))
            .unwrap_err()
            .contains("written for"));
        assert!(decode("{ not json", &root()).is_err());
        assert!(decode(
            r#"{"version":1,"project_root":"/home/user/project","tabs":[]}"#,
            &root()
        )
        .is_err());
    }

    #[test]
    fn a_hand_edit_is_made_usable_rather_than_trusted() {
        let text = r#"{"version":1,"project_root":"/home/user/project","active":2,"tabs":[
            {"conversation_id":"c","provider_session_id":"a","mode":"auto"},
            {"conversation_id":"c","provider_session_id":"","mode":"auto"},
            {"conversation_id":"c","provider_session_id":"b","mode":"auto"},
            {"conversation_id":"c","provider_session_id":"a","mode":"bypass"}]}"#;
        let saved = decode(text, &root()).unwrap();
        let ids: Vec<&str> = saved.tabs.iter().map(|t| t.provider_session_id.as_str()).collect();
        assert_eq!(
            ids,
            ["a", "b"],
            "the blank is dropped and the repeat kept once, in its first place"
        );
        assert_eq!(
            saved.tabs[0].mode,
            SessionModeChoice::Auto,
            "the first listing of a repeated session wins"
        );
        assert_eq!(
            saved.active,
            Some(1),
            "the tab that was active is still the one, whatever was dropped before it"
        );

        let past_the_end = text.replace("\"active\":2", "\"active\":9");
        assert_eq!(decode(&past_the_end, &root()).unwrap().active, Some(0));
    }

    #[test]
    fn a_missing_file_is_nothing_to_offer_and_says_nothing() {
        let dir = ScratchDir::new("nv-saved-tabs", "missing");
        assert_eq!(startup(Some(&dir), &root()), (None, Vec::new()));
        assert_eq!(startup(None, &root()), (None, Vec::new()));
    }

    #[test]
    fn a_file_listing_no_tab_offers_nothing() {
        let dir = ScratchDir::new("nv-saved-tabs", "empty");
        save(&dir, &root(), &SavedTabs::default()).unwrap();
        assert_eq!(startup(Some(&dir), &root()), (None, Vec::new()));
        assert!(
            dir.join(file_name(&root())).exists(),
            "it is a normal file, left where it is"
        );
    }

    #[test]
    fn a_saved_file_is_offered_as_it_was_left() {
        let dir = ScratchDir::new("nv-saved-tabs", "offered");
        save(&dir, &root(), &two()).unwrap();
        assert_eq!(startup(Some(&dir), &root()), (Some(two()), Vec::new()));
    }

    /// An unusable file is kept for a person to read and is never replaced by the window's own later
    /// write: afterwards the project has no file and the `.unusable` beside it holds every byte.
    #[test]
    fn an_unusable_file_is_set_aside_whole_at_startup() {
        let dir = ScratchDir::new("nv-saved-tabs", "unusable");
        let path = dir.join(file_name(&root()));
        crate::test_scratch_dir::write_private(&path, "{ not json").unwrap();
        let (saved, notes) = startup(Some(&dir), &root());
        assert_eq!(saved, None);
        assert!(notes.iter().any(|n| n.contains("unusable")), "{notes:?}");
        assert!(!path.exists());
        let aside = dir.join(format!("{}.unusable", file_name(&root())));
        assert_eq!(std::fs::read_to_string(aside).unwrap(), "{ not json");

        // A newer build's file is set aside the same way.
        crate::test_scratch_dir::write_private(
            &path,
            encode(&two(), &root()).replace("\"version\": 1", "\"version\": 7"),
        )
        .unwrap();
        let (saved, notes) = startup(Some(&dir), &root());
        assert_eq!((saved, notes.len()), (None, 2));
        assert!(!path.exists());
    }

    #[test]
    fn the_file_is_private_and_leaves_no_temporary_behind() {
        use std::os::unix::fs::PermissionsExt;
        let dir = ScratchDir::new("nv-saved-tabs", "private");
        let state = dir.join("eitri").join("tabs");
        let path = save(&state, &root(), &two()).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&state), 0o700);
        let names: Vec<_> = std::fs::read_dir(&state)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "no .tmp left behind: {names:?}");
    }

    fn memory(dir: &Path) -> TabMemory {
        TabMemory::new(Some(dir.to_path_buf()), root())
    }

    fn numbered(n: usize) -> SavedTabs {
        SavedTabs::from_parts(
            (0..=n % 7)
                .map(|i| tab(&format!("s-{n}-{i}"), None, SessionModeChoice::Auto))
                .collect(),
            0,
            Vec::new(),
            None,
        )
    }

    /// The window's thread never waits for the disk: with the writer held inside a write, a hundred changes
    /// are handed over before the writer is released or its hang guard expires. When it is let go the file
    /// ends at the last of them, with the modes the synchronous write had.
    #[test]
    fn the_writer_never_blocks_the_caller_and_the_last_set_wins() {
        use std::os::unix::fs::PermissionsExt;
        let calibration = (0..3)
            .map(|_| {
                let started = std::time::Instant::now();
                for n in 0..100 {
                    std::hint::black_box(numbered(n).clone() == numbered((n + 1) % 100));
                }
                started.elapsed()
            })
            .min()
            .unwrap();
        let mut min_took = std::time::Duration::MAX;
        for attempt in 0..3 {
            let dir = ScratchDir::new("nv-saved-tabs", &format!("writer{attempt}"));
            let state = dir.join("state");
            let mut writer = TabWriter::new(Some(state.clone()), root());
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let release_rx = std::sync::Mutex::new(release_rx);
            let held = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
            let gave_up = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            {
                let held = held.clone();
                let gave_up = gave_up.clone();
                writer.before_write = Some(std::sync::Arc::new(move || {
                    if held.load(std::sync::atomic::Ordering::SeqCst) {
                        if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = release_rx
                            .lock()
                            .unwrap()
                            .recv_timeout(std::time::Duration::from_secs(60))
                        {
                            gave_up.store(true, std::sync::atomic::Ordering::SeqCst);
                            held.store(false, std::sync::atomic::Ordering::SeqCst);
                        }
                    }
                }));
            }
            let started = std::time::Instant::now();
            for n in 0..100 {
                writer.observe(Some(numbered(n)));
            }
            let took = started.elapsed();
            min_took = min_took.min(took);
            assert!(
                !gave_up.load(std::sync::atomic::Ordering::SeqCst),
                "the hand-overs had to finish while the writer was still held"
            );
            held.store(false, std::sync::atomic::Ordering::SeqCst);
            release_tx.send(()).unwrap();
            let done = writer.finish().expect("a writer ran");
            done.recv_timeout(std::time::Duration::from_secs(60))
                .expect("the last write and the end");
            assert_eq!(load(&state, &root()), Loaded::Saved(numbered(99)));
            let path = state.join(file_name(&root()));
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(&state).unwrap().permissions().mode() & 0o777, 0o700);
        }
        // The writer is held: waiting for it or writing to disk on the caller costs milliseconds per
        // hand-over against a calibration of microseconds; a healthy hand-over is a lock and a notify.
        // The generous factor absorbs scheduler noise, and the minimum drops a preempted attempt.
        assert!(
            min_took < calibration * 500,
            "hand-overs took {min_took:?}, against a caller-work calibration of {calibration:?}"
        );
    }

    /// Each change reaches the disk in order: waiting between hand-overs, every set is what the file holds.
    #[test]
    fn the_writer_writes_each_change_in_order() {
        let dir = ScratchDir::new("nv-saved-tabs", "writer-order");
        let mut writer = TabWriter::new(Some(dir.to_path_buf()), root());
        assert!(writer.finish().is_none(), "nothing written, nothing owed");
        let mut writer = TabWriter::new(Some(dir.to_path_buf()), root());
        writer.observe(Some(SavedTabs::default()));
        assert!(writer.worker.is_none(), "no change, no writer");
        for n in 0..20 {
            writer.observe(Some(numbered(n)));
            assert!(writer.wait_written(std::time::Duration::from_secs(5)));
            assert_eq!(load(&dir, &root()), Loaded::Saved(numbered(n)));
        }
        writer.observe(None);
        let done = writer.finish().unwrap();
        done.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(
            load(&dir, &root()),
            Loaded::Saved(numbered(19)),
            "a closing window writes nothing"
        );
    }

    /// A window that adopted a session and closed it again before the writer took the first set hands over
    /// tabs, then no tabs, and only the second is ever taken. The thread writes it although it never wrote
    /// anything else: the file the launch found is emptied, not left to be offered again.
    #[test]
    fn an_emptied_set_whose_predecessor_was_never_taken_is_still_written() {
        let dir = ScratchDir::new("nv-saved-tabs", "writer-emptied");
        save(&dir, &root(), &two()).unwrap();
        let shared = WriterShared::default();
        {
            let mut queue = shared.queue.lock().unwrap();
            queue.next = Some(SavedTabs::default());
            queue.finished = true;
        }
        write_loop(&shared, memory(&dir), || {}, FAILED_WRITE_RETRY);
        assert_eq!(load(&dir, &root()), Loaded::Saved(SavedTabs::default()));

        // The same through the whole writer. Whether the thread takes the first set before it is replaced or not
        // is up to the scheduler, so this half only checks the file ends empty either way; the half above pins
        // the case that went wrong.
        let dir = ScratchDir::new("nv-saved-tabs", "writer-emptied-whole");
        save(&dir, &root(), &two()).unwrap();
        let mut writer = TabWriter::new(Some(dir.to_path_buf()), root());
        let gate = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        writer.before_write = Some({
            let gate = gate.clone();
            std::sync::Arc::new(move || {
                let (open, ready) = &*gate;
                let open = open.lock().unwrap();
                let _ = ready.wait_timeout_while(open, std::time::Duration::from_secs(10), |open| !*open);
            })
        });
        writer.observe(Some(numbered(3)));
        writer.observe(Some(SavedTabs::default()));
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        let done = writer.finish().unwrap();
        done.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        assert_eq!(load(&dir, &root()), Loaded::Saved(SavedTabs::default()));
    }

    /// A writer dropped without `finish` ends its thread, after the last set it was handed, and the drop
    /// itself does not wait.
    #[test]
    fn a_dropped_writer_ends_its_thread() {
        let dir = ScratchDir::new("nv-saved-tabs", "writer-drop");
        let mut writer = TabWriter::new(Some(dir.to_path_buf()), root());
        let (entered, ready) = std::sync::mpsc::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let gate = std::sync::Mutex::new(gate);
        writer.set_before_write(std::sync::Arc::new(move || {
            entered.send(()).unwrap();
            // The sender's drop also releases the worker if the outcome check panics.
            let _ = gate.lock().unwrap().recv();
        }));
        writer.observe(Some(numbered(4)));
        ready
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("writer entered the write");
        let done = writer.worker.as_mut().unwrap().done.take().unwrap();
        let (dropped, did_drop) = std::sync::mpsc::channel();
        let dropper = std::thread::spawn(move || {
            drop(writer);
            let _ = dropped.send(());
        });
        did_drop
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("drop returns while the write is still held");
        release.send(()).unwrap();
        done.recv_timeout(std::time::Duration::from_secs(60))
            .expect("the thread ended");
        dropper.join().unwrap();
        assert_eq!(load(&dir, &root()), Loaded::Saved(numbered(4)));
    }

    /// Turning remembering off drops a set still queued and writes nothing more.
    #[test]
    fn a_disabled_writer_drops_what_is_queued() {
        let dir = ScratchDir::new("nv-saved-tabs", "writer-off");
        let mut writer = TabWriter::new(Some(dir.to_path_buf()), root());
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = std::sync::Mutex::new(release_rx);
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let entered_tx = std::sync::Mutex::new(entered_tx);
        writer.before_write = Some(std::sync::Arc::new(move || {
            let _ = entered_tx.lock().unwrap().send(());
            let _ = release_rx
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(10));
        }));
        writer.observe(Some(numbered(1)));
        // The writer must have taken set 1 before set 2 arrives, or the two are folded into one queued set and the
        // write under way is set 2.
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the writer took the first set");
        writer.observe(Some(numbered(2)));
        writer.disable();
        assert!(!writer.is_enabled());
        writer.observe(Some(numbered(3)));
        release_tx.send(()).unwrap();
        let done = writer.finish().unwrap();
        done.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        // The write already under way may finish; the queued one and anything later never do.
        match load(&dir, &root()) {
            Loaded::Missing => {}
            Loaded::Saved(saved) => assert_eq!(saved, numbered(1)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_window_that_has_done_nothing_writes_nothing_and_leaves_the_last_windows_file() {
        let dir = ScratchDir::new("nv-saved-tabs", "quiet");
        save(&dir, &root(), &two()).unwrap();
        let mut memory = memory(&dir);
        assert!(memory.observe(Some(SavedTabs::default())).is_none());
        assert!(memory.observe(Some(SavedTabs::default())).is_none());
        assert_eq!(load(&dir, &root()), Loaded::Saved(two()));
    }

    #[test]
    fn it_writes_each_change_once_and_only_a_change() {
        let dir = ScratchDir::new("nv-saved-tabs", "changes");
        let mut memory = memory(&dir);
        let first = SavedTabs::from_parts(vec![tab("s-one", None, SessionModeChoice::Auto)], 0, Vec::new(), None);
        assert!(matches!(memory.observe(Some(first.clone())), Some(Ok(_))));
        assert_eq!(load(&dir, &root()), Loaded::Saved(first.clone()));
        assert!(
            memory.observe(Some(first.clone())).is_none(),
            "the same tabs again writes nothing"
        );

        // Each of the things the window can change about a listed tab is a write.
        let mut next = first.clone();
        next.tabs[0].name = Some("renamed".to_string());
        assert!(matches!(memory.observe(Some(next.clone())), Some(Ok(_))));
        next.tabs[0].mode = SessionModeChoice::Bypass;
        assert!(matches!(memory.observe(Some(next.clone())), Some(Ok(_))));
        next.tabs.push(tab("s-two", None, SessionModeChoice::Auto));
        assert!(matches!(memory.observe(Some(next.clone())), Some(Ok(_))));
        (next.active, next.claude_active) = (Some(1), Some(1));
        assert!(matches!(memory.observe(Some(next.clone())), Some(Ok(_))));
        next.tabs.reverse();
        assert!(matches!(memory.observe(Some(next.clone())), Some(Ok(_))), "a reorder");
        assert_eq!(load(&dir, &root()), Loaded::Saved(next));
    }

    /// Closing the last listed tab on purpose is a change like any other; the window closing is not.
    #[test]
    fn closing_the_last_tab_is_recorded_but_the_window_closing_is_not() {
        let dir = ScratchDir::new("nv-saved-tabs", "closing");
        let mut memory = memory(&dir);
        assert!(matches!(memory.observe(Some(two())), Some(Ok(_))));
        assert!(memory.observe(None).is_none(), "a closing window has no tabs to show");
        assert_eq!(
            load(&dir, &root()),
            Loaded::Saved(two()),
            "the file still says what was open"
        );
        assert!(matches!(memory.observe(Some(SavedTabs::default())), Some(Ok(_))));
        assert_eq!(load(&dir, &root()), Loaded::Saved(SavedTabs::default()));
    }

    #[test]
    fn a_failed_write_is_reported_once_per_change() {
        let dir = ScratchDir::new("nv-saved-tabs", "failing");
        // A file where the directory should be: every write under it fails.
        let blocker = dir.join("blocked");
        crate::test_scratch_dir::write_private(&blocker, "").unwrap();
        let mut memory = TabMemory::new(Some(blocker.join("tabs")), root());
        assert!(matches!(memory.observe(Some(two())), Some(Err(_))));
        assert!(
            memory.observe(Some(two())).is_none(),
            "not again until something changes"
        );
    }

    #[test]
    fn a_disabled_memory_never_writes() {
        let dir = ScratchDir::new("nv-saved-tabs", "disabled");
        let mut memory = memory(&dir);
        memory.disable();
        assert!(!memory.is_enabled());
        assert!(memory.observe(Some(two())).is_none());
        assert_eq!(load(&dir, &root()), Loaded::Missing);
        assert!(TabMemory::new(None, root()).observe(Some(two())).is_none());
    }

    /// If the bad file cannot be set aside, it is left where it is: replacing it would destroy the one copy
    /// of what a person may want to read.
    #[test]
    fn a_bad_file_that_cannot_be_set_aside_is_not_overwritten() {
        let dir = ScratchDir::new("nv-saved-tabs", "aside-blocked");
        let mut memory = memory(&dir);
        let path = dir.join(file_name(&root()));
        crate::test_scratch_dir::write_private(&path, "typo").unwrap();
        // The place it would go is a directory that is not empty, so the rename fails.
        let aside = dir.join(format!("{}.unusable", file_name(&root())));
        std::fs::create_dir_all(aside.join("occupied")).unwrap();
        let outcome = memory.observe(Some(two())).expect("a write was due");
        assert!(outcome.is_err(), "reported, not swallowed");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "typo",
            "the bad file is untouched"
        );
        // It is tried again when the tabs next change, not on every tick.
        assert!(memory.observe(Some(two())).is_none());
    }

    /// A file that went bad while the window was open is kept, not overwritten.
    #[test]
    fn a_file_that_went_bad_since_launch_is_set_aside_before_the_write() {
        let dir = ScratchDir::new("nv-saved-tabs", "went-bad");
        let mut memory = memory(&dir);
        std::fs::create_dir_all(&*dir).unwrap();
        crate::test_scratch_dir::write_private(dir.join(file_name(&root())), "typo").unwrap();
        let written = memory.observe(Some(two())).unwrap().unwrap();
        let aside = written.set_aside.expect("the bad file was set aside");
        assert_eq!(std::fs::read_to_string(aside).unwrap(), "typo");
        assert_eq!(load(&dir, &root()), Loaded::Saved(two()));
    }

    fn codex(session: &str, mode: SessionModeChoice) -> SavedTab {
        SavedTab {
            agent: AgentId::Codex,
            conversation_id: "0123456789abcdef0123456789abcdef".to_string(),
            provider_session_id: session.to_string(),
            name: Some("refactor".to_string()),
            mode,
            view: None,
            window: None,
            turn_running: false,
            draft: None,
            row_key: None,
            plan_origin: None,
        }
    }

    /// Claude and Codex tabs mixed in the tab bar come back in the bar's order, from a file that records it; a file
    /// without the record, and one a later build wrote with an agent this build does not know, still load.
    #[test]
    fn the_tab_bars_order_across_agents_survives_the_file() {
        let claude = |id: &str| SavedTab {
            agent: AgentId::Claude,
            ..codex(id, SessionModeChoice::Auto)
        };
        let saved = and_others(
            SavedTabs::from_parts(vec![claude("c-1"), claude("c-2")], 0, Vec::new(), None),
            vec![
                codex("t-1", SessionModeChoice::Auto),
                codex("t-2", SessionModeChoice::Auto),
            ],
            None,
        )
        .with_bar(
            ["t-1", "c-1", "t-2", "c-2"]
                .iter()
                .map(|id| {
                    (
                        if id.starts_with('t') {
                            AgentId::Codex
                        } else {
                            AgentId::Claude
                        },
                        id.to_string(),
                    )
                })
                .collect(),
        );
        let order = |saved: &SavedTabs| -> Vec<String> {
            saved
                .in_bar_order()
                .iter()
                .map(|t| t.provider_session_id.clone())
                .collect()
        };
        assert_eq!(order(&saved), ["t-1", "c-1", "t-2", "c-2"]);
        let text = encode(&saved, &root());
        let read = decode(&text, &root()).unwrap();
        assert_eq!(order(&read), ["t-1", "c-1", "t-2", "c-2"]);
        assert_eq!(read, saved);

        // No record of the order (a file from before it, or a set with none): Claude's tabs, then the others'.
        let mut plain = saved.clone();
        plain.bar = Vec::new();
        assert_eq!(order(&plain), ["c-1", "c-2", "t-1", "t-2"]);
        assert!(!encode(&plain, &root()).contains("tab_bar"));
        // Claude only: the file is what it always was.
        assert!(!encode(&two(), &root()).contains("tab_bar"));

        // A place an unknown agent holds is nobody's here; the file loads and the rest keeps its order.
        let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
        value["tab_bar"]
            .as_array_mut()
            .unwrap()
            .insert(1, serde_json::json!({"agent": "openai", "provider_session_id": "o-1"}));
        let later = decode(&value.to_string(), &root()).unwrap();
        assert_eq!(order(&later), ["t-1", "c-1", "t-2", "c-2"]);

        // Offering another window's tabs behind this window's keeps each set's order.
        let behind = SavedTabs::from_parts(
            vec![claude("c-3")],
            0,
            vec![codex("t-3", SessionModeChoice::Auto)],
            None,
        )
        .with_bar(vec![(AgentId::Codex, "t-3".into()), (AgentId::Claude, "c-3".into())]);
        assert_eq!(
            order(&saved.clone().followed_by(behind)),
            ["t-1", "c-1", "t-2", "c-2", "t-3", "c-3"]
        );

        // Across the link between windows, whose peer may not know the key: it is carried, and a set without it reads none.
        let wire = saved.to_wire();
        assert_eq!(
            order(&SavedTabs::from_wire(&wire).unwrap()),
            ["t-1", "c-1", "t-2", "c-2"]
        );
        let mut old_peer = wire.clone();
        old_peer.as_object_mut().unwrap().remove("bar");
        assert_eq!(
            order(&SavedTabs::from_wire(&old_peer).unwrap()),
            ["c-1", "c-2", "t-1", "t-2"]
        );
        let mut odd = wire;
        odd["bar"] = serde_json::json!([{"agent": "openai", "provider_session_id": "o"}, 7, {"agent": "codex"}]);
        assert_eq!(
            order(&SavedTabs::from_wire(&odd).unwrap()),
            ["c-1", "c-2", "t-1", "t-2"]
        );
    }

    #[test]
    fn external_tabs_round_trip_and_old_files_load() {
        let saved = and_others(
            two(),
            vec![
                codex("thread-a", SessionModeChoice::Bypass),
                SavedTab {
                    agent: AgentId::Acp("gemini".into()),
                    name: None,
                    ..codex("session-b", SessionModeChoice::Auto)
                },
            ],
            Some(1),
        );
        assert_eq!(saved.active, Some(3), "the other agent's tab on screen wins");
        assert_eq!(saved.claude_active, Some(1));
        let text = encode(&saved, &root());
        assert!(text.contains("\"external_tabs\""), "{text}");
        assert!(
            text.contains("\"agent\": \"codex\"") && text.contains("\"agent\": \"acp:gemini\""),
            "{text}"
        );
        assert!(text.contains("\"version\": 1"), "the version stays 1: {text}");
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["active"], 1, "the Claude tab a Claude-only reader puts on screen");
        assert_eq!(value["active_external"], 1);
        assert_eq!(decode(&text, &root()), Ok(saved));

        // A window that ran only Claude writes exactly what it always wrote.
        let only_claude = encode(&two(), &root());
        assert!(!only_claude.contains("external"), "{only_claude}");
        // And a file written before external tabs existed loads with none.
        let old = r#"{"version":1,"project_root":"/home/user/project","tabs":[
            {"conversation_id":"c","provider_session_id":"s-one","mode":"auto"}],"active":0}"#;
        let loaded = decode(old, &root()).unwrap();
        assert_eq!(loaded.tabs.len(), 1);
        assert!(!loaded.has_other_agents());
        assert_eq!(loaded.active, Some(0));

        // An entry for Claude, an unknown agent or a session listed twice is never kept as another
        // agent's tab; an agent this build does not know is left out and the rest of the file kept.
        let twice = and_others(
            two(),
            vec![
                codex("thread-a", SessionModeChoice::Auto),
                codex("thread-a", SessionModeChoice::Bypass),
            ],
            Some(1),
        );
        let kept = decode(&encode(&twice, &root()), &root()).unwrap();
        assert_eq!(
            kept.other_tabs().cloned().collect::<Vec<_>>(),
            vec![codex("thread-a", SessionModeChoice::Auto)]
        );
        assert_eq!(kept.active, Some(2), "the active entry keeps pointing at its session");
        let claude_as_external = encode(&twice, &root()).replace("\"agent\": \"codex\"", "\"agent\": \"claude\"");
        let read = decode(&claude_as_external, &root()).unwrap();
        assert!(!read.has_other_agents());
        assert_eq!(read.tabs, two().tabs, "nor is it added to the Claude tabs");
        let unknown = encode(&twice, &root()).replacen("\"agent\": \"codex\"", "\"agent\": \"openai\"", 1);
        let read = decode(&unknown, &root()).expect("the rest of the file is still usable");
        assert_eq!(read.claude_tabs().cloned().collect::<Vec<_>>(), two().tabs);
        assert_eq!(
            read.other_tabs().cloned().collect::<Vec<_>>(),
            vec![codex("thread-a", SessionModeChoice::Bypass)]
        );
        assert_eq!(read.active, Some(2));

        // A file that holds only another agent's tabs is still something to offer at launch.
        let dir = ScratchDir::new("nv-saved-tabs", "external-only");
        let only_external =
            SavedTabs::from_parts(Vec::new(), 0, vec![codex("thread-a", SessionModeChoice::Auto)], Some(0));
        assert_eq!((only_external.active, only_external.claude_active), (Some(0), None));
        save(&dir, &root(), &only_external).unwrap();
        assert_eq!(startup(Some(&dir), &root()).0, Some(only_external));
    }

    /// Two agents may give a session the same id: each is its own tab, in the file and read back.
    #[test]
    fn the_same_session_id_under_two_agents_is_two_tabs() {
        let saved = and_others(
            two(),
            vec![SavedTab {
                agent: AgentId::Acp("gemini".into()),
                ..codex("s-one", SessionModeChoice::Auto)
            }],
            None,
        );
        let read = decode(&encode(&saved, &root()), &root()).unwrap();
        assert_eq!(read, saved);
        assert_eq!(read.tabs.len(), 3);
    }

    /// Reading a file, writing what was read and reading that again gives what the first read gave:
    /// what a file means survives a write, though entries a hand edit broke or an unknown agent's are
    /// not written back.
    #[test]
    fn saved_tabs_round_trip_by_meaning() {
        let fixtures = [
            // Claude tabs only, hand-edited: a blank entry, a repeat, the active index past them.
            r#"{"version":1,"project_root":"/home/user/project","active":3,"tabs":[
                {"conversation_id":"c","provider_session_id":"a","mode":"auto","name":"api"},
                {"conversation_id":"c","provider_session_id":"","mode":"auto"},
                {"conversation_id":"c","provider_session_id":"a","mode":"bypass"},
                {"conversation_id":"c","provider_session_id":"b","mode":"manual","view":3}]}"#,
            // Both kinds, another agent's tab on screen past one this build does not know, and a
            // Claude entry among the other agents'.
            r#"{"version":1,"project_root":"/home/user/project","active":1,"tabs":[
                {"conversation_id":"c","provider_session_id":"a","mode":"auto"},
                {"conversation_id":"c","provider_session_id":"b","mode":"auto"}],
                "external_tabs":[
                {"agent":"openai","conversation_id":"c","provider_session_id":"o","mode":"auto"},
                {"agent":"claude","conversation_id":"c","provider_session_id":"z","mode":"auto"},
                {"agent":"codex","conversation_id":"c","provider_session_id":"t","mode":"bypass","view":2},
                {"agent":"acp:gemini","conversation_id":"c","provider_session_id":"a","mode":"auto"}],
                "active_external":2}"#,
            // Other agents' tabs only, none of them on screen.
            r#"{"version":1,"project_root":"/home/user/project","active":0,"tabs":[],
                "external_tabs":[
                {"agent":"codex","conversation_id":"c","provider_session_id":"t","mode":"auto"},
                {"agent":"codex","conversation_id":"c","provider_session_id":"t","mode":"bypass"},
                {"agent":"acp:gemini","conversation_id":"c","provider_session_id":"g","mode":"manual"}]}"#,
        ];
        for fixture in fixtures {
            let first = decode(fixture, &root()).unwrap_or_else(|e| panic!("{e}: {fixture}"));
            let again = decode(&encode(&first, &root()), &root()).unwrap();
            assert_eq!(again, first, "{fixture}");
        }
        let both = decode(fixtures[1], &root()).unwrap();
        let ids: Vec<(String, &str)> = both
            .tabs
            .iter()
            .map(|t| (t.agent.record_name(), t.provider_session_id.as_str()))
            .collect();
        assert_eq!(
            ids,
            [
                ("claude".to_string(), "a"),
                ("claude".to_string(), "b"),
                ("codex".to_string(), "t"),
                ("acp:gemini".to_string(), "a"),
            ]
        );
        assert_eq!(both.active_tab().map(|t| t.provider_session_id.as_str()), Some("t"));
        assert_eq!(both.claude_active, Some(1));
        let others_only = decode(fixtures[2], &root()).unwrap();
        assert_eq!((others_only.active, others_only.claude_active), (None, None));
    }

    /// What an Eitri from before external tabs reads: its own `TabsFile`, copied here as it was.
    #[derive(Deserialize)]
    struct OlderTabsFile {
        version: u32,
        project_root: String,
        tabs: Vec<OlderTab>,
        active: usize,
    }

    #[derive(Deserialize)]
    struct OlderTab {
        provider_session_id: String,
        mode: SessionModeChoice,
    }

    #[test]
    fn an_older_reader_ignores_external_tabs() {
        let saved = and_others(two(), vec![codex("thread-a", SessionModeChoice::Bypass)], Some(0));
        let text = encode(&saved, &root());
        let older: OlderTabsFile = serde_json::from_str(&text).expect("an older build reads the file");
        assert_eq!(older.version, VERSION);
        assert_eq!(older.project_root, root().to_string_lossy());
        let read: Vec<(&str, SessionModeChoice)> = older
            .tabs
            .iter()
            .map(|t| (t.provider_session_id.as_str(), t.mode))
            .collect();
        assert_eq!(
            read,
            [("s-one", SessionModeChoice::Auto), ("s-two", SessionModeChoice::Bypass)],
            "only the Claude tabs, so no Codex thread resumes as Claude"
        );
        assert_eq!(older.active, 1, "and a Claude tab to fall back to as the active one");
    }

    fn remote_tab(sidecar: &str) -> SavedRemoteTab {
        SavedRemoteTab {
            window: None,
            helper: agent::HelperHandoff {
                daemon: "".into(),
                declared_client_tools: Default::default(),
                host: "example-host".into(),
                host_key_sha256: "SHA256:example".into(),
                sidecar_id: sidecar.into(),
                session_id: format!("session-{sidecar}"),
                last_folded_sequence: 9,
            },
            provider_session_id: Some(format!("claude-{sidecar}")),
            name: Some("api".into()),
            mode: SessionModeChoice::Auto,
        }
    }

    #[test]
    fn remote_tabs_from_two_installations_keep_distinct_identities() {
        let mut old = remote_tab("same-sidecar");
        old.helper.daemon = "old-installation".into();
        let mut new = old.clone();
        new.helper.daemon = "new-installation".into();
        let first = SavedTabs::default().with_remote(vec![old.clone()]);
        let later = SavedTabs::default().with_remote(vec![new.clone()]);
        let both = SavedTabs::default().with_remote(vec![old, new]);
        assert_eq!(decode(&encode(&both, &root()), &root()).unwrap().remote.len(), 2);
        assert_eq!(first.clone().followed_by(later.clone()).remote.len(), 2);
        assert_eq!(
            merge_windows(&[(WindowName::Chat(1), first), (WindowName::Chat(2), later)], None)
                .remote
                .len(),
            2
        );
    }

    /// Remote tabs are written in an array of their own: an older build, which reads `tabs`, never resumes one
    /// as a local Claude tab, and a file written before them reads as having none. An entry missing a remote
    /// fact is a hole, not a tab, and a tab listed twice is kept once.
    #[test]
    fn remote_tabs_have_their_own_array_an_older_build_never_reads() {
        let saved = two().with_remote(vec![remote_tab("sc-1"), remote_tab("sc-2")]);
        let text = encode(&saved, &root());
        let older: OlderTabsFile = serde_json::from_str(&text).expect("an older build reads the file");
        let read: Vec<&str> = older.tabs.iter().map(|t| t.provider_session_id.as_str()).collect();
        assert_eq!(read, ["s-one", "s-two"], "no remote tab among the Claude tabs");
        let back = decode(&text, &root()).unwrap();
        assert_eq!(back, saved);
        assert!(!back.is_empty());
        assert!(!SavedTabs::default().with_remote(vec![remote_tab("sc-1")]).is_empty());

        let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
        value["remote_tabs"][0]["helper"]
            .as_object_mut()
            .unwrap()
            .remove("sidecar_id");
        value["remote_tabs"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::to_value(remote_tab("sc-2")).unwrap());
        let holes = decode(&value.to_string(), &root()).unwrap();
        assert_eq!(holes.remote, vec![remote_tab("sc-2")]);

        // A later build's tab, with a field this build does not know, is still this build's tab to keep.
        let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
        value["remote_tabs"][0]["helper"]["added_later"] = serde_json::json!("0.4.0");
        let later = decode(&value.to_string(), &root()).unwrap();
        assert_eq!(later.remote, vec![remote_tab("sc-1"), remote_tab("sc-2")]);

        let before = encode(&two(), &root());
        assert!(
            !before.contains("remote_tabs"),
            "a file with no remote tab is what it always was"
        );
        assert!(decode(&before, &root()).unwrap().remote.is_empty());
    }

    /// A file read back and two files joined keep one entry per agent, and agree on what one agent is: two sessions
    /// of one sidecar are two entries both ways, one session listed twice is one.
    #[test]
    fn joined_and_read_files_keep_the_same_remote_tabs() {
        let mut other_session = remote_tab("sc-1");
        other_session.helper.session_id = "session-other".into();
        let joined = SavedTabs::default()
            .with_remote(vec![remote_tab("sc-1")])
            .followed_by(SavedTabs::default().with_remote(vec![remote_tab("sc-1"), other_session.clone()]));
        assert_eq!(joined.remote, vec![remote_tab("sc-1"), other_session.clone()]);
        let text = encode(
            &SavedTabs::default().with_remote(vec![remote_tab("sc-1"), other_session.clone(), remote_tab("sc-1")]),
            &root(),
        );
        assert_eq!(decode(&text, &root()).unwrap().remote, joined.remote);
    }

    fn one_tab(session: &str) -> SavedTabs {
        SavedTabs::from_parts(vec![tab(session, None, SessionModeChoice::Auto)], 0, Vec::new(), None)
    }

    /// The carry from the shared file runs once per root and role: once it is marked, a keyed file claimed or set aside
    /// later is not filled again from the shared file, and the shared file's copies are known to live elsewhere. A carry
    /// with nothing to copy is marked too.
    #[test]
    fn the_carry_from_the_shared_file_runs_once_per_root_and_role() {
        use crate::remote_project::TabOrigin;
        let state = ScratchDir::new("nv-tabs-origin", "once");
        let origin = TabOrigin::Remote {
            host: "example-host".into(),
            host_key_sha256: "SHA256:example".into(),
            root: root(),
        };
        let local = state_dir(Some(state.as_os_str()), None).unwrap();
        let remote = state_dir_for(Some(state.as_os_str()), None, &origin).unwrap();
        save_as(
            &local,
            &root(),
            Role::Second,
            &SavedTabs::default().with_remote(vec![remote_tab("sc-7")]),
        )
        .unwrap();
        assert!(!carried_elsewhere(&local, &root(), Role::Second, "SHA256:example"));
        assert_eq!(
            carry_remote_tabs(&local, &remote, &root(), Role::Second, "SHA256:example").unwrap(),
            1
        );
        assert!(carried_elsewhere(&local, &root(), Role::Second, "SHA256:example"));
        assert!(!carried_elsewhere(&local, &root(), Role::Main, "SHA256:example"));
        assert!(!carried_elsewhere(&local, &root(), Role::Second, "SHA256:other"));
        // Window 1 claimed window 2's keyed file: it left its place, and the shared file still lists the tab.
        std::fs::remove_file(remote.join(file_name_as(&root(), Role::Second))).unwrap();
        assert_eq!(
            carry_remote_tabs(&local, &remote, &root(), Role::Second, "SHA256:example").unwrap(),
            0
        );
        assert_eq!(load_as(&remote, &root(), Role::Second), Loaded::Missing);
        assert_eq!(
            carry_remote_tabs(&local, &remote, &root(), Role::Main, "SHA256:example").unwrap(),
            0
        );
        assert!(carried_elsewhere(&local, &root(), Role::Main, "SHA256:example"));
    }

    /// After a host-key change each role's file of the root moves from the old key's directory to the new key's, its
    /// remote tabs taking the new key, once; a role whose file the new key has already is left where it is.
    #[test]
    fn a_changed_host_key_moves_the_old_keys_saved_tabs() {
        use crate::remote_project::origin_key;
        let state = ScratchDir::new("nv-tabs-origin", "rekey");
        let base = state_dir(Some(state.as_os_str()), None).unwrap();
        let old = base.join(origin_key("SHA256:example"));
        let new = base.join(origin_key("SHA256:new"));
        save_as(
            &old,
            &root(),
            Role::Main,
            &one_tab("s-1").with_remote(vec![remote_tab("sc-1")]),
        )
        .unwrap();
        save_as(
            &old,
            &root(),
            Role::Second,
            &SavedTabs::default().with_remote(vec![remote_tab("sc-2")]),
        )
        .unwrap();
        save_as(
            &new,
            &root(),
            Role::Second,
            &SavedTabs::default().with_remote(vec![remote_tab("sc-3")]),
        )
        .unwrap();
        assert_eq!(
            carry_to_new_host_key(&base, &root(), "SHA256:example", "SHA256:new").unwrap(),
            1
        );
        let Loaded::Saved(main) = load_as(&new, &root(), Role::Main) else {
            panic!("window 1's file moved");
        };
        assert_eq!(main.remote[0].helper.sidecar_id, "sc-1");
        assert_eq!(main.remote[0].helper.host_key_sha256, "SHA256:new");
        assert_eq!(load_as(&old, &root(), Role::Main), Loaded::Missing);
        assert!(
            matches!(load_as(&old, &root(), Role::Second), Loaded::Saved(_)),
            "kept beside the new key's own"
        );
        assert_eq!(
            carry_to_new_host_key(&base, &root(), "SHA256:example", "SHA256:new").unwrap(),
            0
        );
    }

    /// A window of a remote project and a `--local` window of its copy, open at once on one root, each keep their own
    /// file: neither write touches the other's tabs. A remote window's first start copies the remote tabs of its
    /// server that the shared file held before files were kept by origin, once, and leaves that file whole.
    #[test]
    fn saved_tabs_are_keyed_by_origin() {
        use crate::remote_project::TabOrigin;
        let state = ScratchDir::new("nv-tabs-origin", "both");
        let remote_origin = TabOrigin::Remote {
            host: "example-host".into(),
            host_key_sha256: "SHA256:example".into(),
            root: root(),
        };
        let local = state_dir_for(Some(state.as_os_str()), None, &TabOrigin::Local).unwrap();
        let remote = state_dir_for(Some(state.as_os_str()), None, &remote_origin).unwrap();
        assert_eq!(local, state_dir(Some(state.as_os_str()), None).unwrap());
        assert_eq!(remote.parent(), Some(local.as_path()));
        assert_ne!(local, remote);

        // Written before files were kept by origin: the local window carried a remote tab forward, and another
        // server's tab was saved there too.
        let mut other_server = remote_tab("sc-other");
        other_server.helper.host_key_sha256 = "SHA256:other".into();
        let shared = one_tab("s-local").with_remote(vec![remote_tab("sc-1"), other_server.clone()]);
        save(&local, &root(), &shared).unwrap();

        assert_eq!(
            carry_remote_tabs(&local, &remote, &root(), Role::Main, "SHA256:example").unwrap(),
            1
        );
        assert_eq!(
            load(&remote, &root()),
            Loaded::Saved(SavedTabs::default().with_remote(vec![remote_tab("sc-1")]))
        );
        assert_eq!(
            load(&local, &root()),
            Loaded::Saved(shared.clone()),
            "the shared file is left whole"
        );
        assert_eq!(
            carry_remote_tabs(&local, &remote, &root(), Role::Main, "SHA256:example").unwrap(),
            0
        );

        // Both windows open at once, each writing as its tabs change.
        let mut local_writer = TabWriter::new(Some(local.clone()), root());
        let mut remote_writer = TabWriter::new(Some(state.join("unused")), root());
        remote_writer.move_to(remote.clone()).unwrap();
        local_writer.observe(Some(one_tab("s-local-2").with_remote(vec![other_server.clone()])));
        remote_writer.observe(Some(
            SavedTabs::default().with_remote(vec![remote_tab("sc-1"), remote_tab("sc-2")]),
        ));
        local_writer
            .finish()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(60))
            .unwrap();
        remote_writer
            .finish()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(60))
            .unwrap();
        assert_eq!(
            load(&local, &root()),
            Loaded::Saved(one_tab("s-local-2").with_remote(vec![other_server]))
        );
        assert_eq!(
            load(&remote, &root()),
            Loaded::Saved(SavedTabs::default().with_remote(vec![remote_tab("sc-1"), remote_tab("sc-2")]))
        );
        assert!(!state.join("unused").exists());
        assert!(remote_writer.move_to(local).is_err(), "a finished writer cannot move");
        let mut off = TabWriter::new(None, root());
        off.move_to(remote).unwrap();
        assert!(!off.is_enabled());
    }

    fn sessions_of(saved: &SavedTabs) -> Vec<&str> {
        saved.tabs.iter().map(|t| t.provider_session_id.as_str()).collect()
    }

    #[test]
    fn each_role_reads_and_writes_only_its_own_tabs_file() {
        let dir = ScratchDir::new("nv-tabs-roles", "own");
        save_as(&dir, &root(), Role::Main, &one_tab("s-main")).unwrap();
        save_as(&dir, &root(), Role::Second, &one_tab("s-second")).unwrap();
        save_as(&dir, &root(), Role::Unpaired, &one_tab("s-unpaired")).unwrap();
        assert_eq!(load(&dir, &root()), Loaded::Saved(one_tab("s-main")));
        assert_eq!(load_as(&dir, &root(), Role::Second), Loaded::Saved(one_tab("s-second")));
        assert_eq!(
            load_as(&dir, &root(), Role::Unpaired),
            Loaded::Saved(one_tab("s-unpaired"))
        );
        assert!(dir.join(file_name(&root())).exists());
        assert_eq!(file_name_as(&root(), Role::Main), file_name(&root()));
    }

    #[test]
    fn window_restore_false_brings_window_2s_tabs_to_window_1_and_marks_them_taken_only_once_written() {
        let dir = ScratchDir::new("nv-tabs-roles", "take-back");
        save_as(&dir, &root(), Role::Main, &one_tab("s-main")).unwrap();
        let second = and_others(
            one_tab("s-second"),
            vec![codex("thread-second", SessionModeChoice::Auto)],
            None,
        );
        save_as(&dir, &root(), Role::Second, &second).unwrap();
        let second_path = dir.join(file_name_as(&root(), Role::Second));
        let main_path = dir.join(file_name(&root()));

        let (offer, _notes, takeovers) = startup_with(Some(&dir), &root(), Role::Main, &[Role::Second]);
        let offer = offer.expect("an offer");
        assert_eq!(
            sessions_of(&offer),
            ["s-main", "s-second", "thread-second"],
            "window 2's Claude and other agents' tabs follow window 1's own"
        );
        assert_eq!(offer.active_tab().unwrap().provider_session_id, "s-main");
        assert_eq!(takeovers.len(), 1);
        let claimed = dir.join(format!("{}.offered", file_name_as(&root(), Role::Second)));
        assert!(!second_path.exists(), "window 2's file is claimed by the offer");
        assert_eq!(
            load_path(&claimed, &root()),
            Loaded::Saved(second.clone()),
            "the claim holds its tabs whole"
        );

        let mut writer = TabWriter::new_as(Some(dir.to_path_buf()), root(), Role::Main);
        writer.take_over_when_written(takeovers);
        // Only window 1's own tab back so far: window 2's file must stay, its tabs are in no other file.
        writer.observe(Some(one_tab("s-main-again")));
        assert!(writer.wait_written(std::time::Duration::from_secs(5)));
        assert!(claimed.exists(), "not taken while window 1's file lacks its tabs");
        // Some of them, not all.
        let some = and_others(one_tab("s-second"), Vec::new(), None);
        writer.observe(Some(some));
        assert!(writer.wait_written(std::time::Duration::from_secs(5)));
        assert!(claimed.exists(), "not taken while one of them is still missing");
        writer.observe(Some(offer.clone()));
        assert!(writer.wait_written(std::time::Duration::from_secs(5)));
        assert!(!claimed.exists(), "taken once window 1's file holds them all");
        assert!(dir
            .join(format!("{}.taken", file_name_as(&root(), Role::Second)))
            .exists());
        assert_eq!(
            load(&dir, &root()),
            Loaded::Saved(offer),
            "window 1's own file holds them"
        );
        assert!(main_path.exists());
    }

    #[test]
    fn a_lone_holder_offers_unpaired_tabs_then_marks_them_taken() {
        let dir = ScratchDir::new("nv-tabs-roles", "lone");
        save_as(&dir, &root(), Role::Unpaired, &one_tab("s-unpaired")).unwrap();
        let unpaired = dir.join(file_name_as(&root(), Role::Unpaired));
        // No file of its own yet: the unpaired window's tabs are the whole offer.
        let (offer, _, takeovers) = startup_with(Some(&dir), &root(), Role::Main, &[Role::Unpaired]);
        let offer = offer.expect("the unpaired window's tabs are offered");
        assert_eq!(sessions_of(&offer), ["s-unpaired"]);
        let mut writer = TabWriter::new_as(Some(dir.to_path_buf()), root(), Role::Main);
        writer.take_over_when_written(takeovers);
        writer.observe(Some(offer));
        assert!(writer.wait_written(std::time::Duration::from_secs(5)));
        assert!(!unpaired.exists());
        assert!(dir
            .join(format!("{}.taken", file_name_as(&root(), Role::Unpaired)))
            .exists());
        // Taken files are not offered again.
        let (again, _, takeovers) = startup_with(Some(&dir), &root(), Role::Main, &[Role::Unpaired]);
        assert_eq!(
            again.map(|s| sessions_of(&s).len()),
            Some(1),
            "window 1's own file only"
        );
        assert!(takeovers.is_empty());
    }

    #[test]
    fn a_file_the_other_window_writes_after_the_offer_is_neither_lost_nor_taken() {
        for (label, other) in [("w2", Role::Second), ("u", Role::Unpaired)] {
            let dir = ScratchDir::new("nv-tabs-roles", &format!("rewritten-{label}"));
            save_as(&dir, &root(), other, &one_tab("s-old")).unwrap();
            let other_path = dir.join(file_name_as(&root(), other));
            let taken = dir.join(format!("{}.taken", file_name_as(&root(), other)));
            let (offer, _, takeovers) = startup_with(Some(&dir), &root(), Role::Main, &[other]);
            let offer = offer.expect("the other window's tabs are offered");
            // Before the offer is taken, the other window starts a session of its own and writes its file.
            save_as(&dir, &root(), other, &one_tab("s-new")).unwrap();
            let mut writer = TabWriter::new_as(Some(dir.to_path_buf()), root(), Role::Main);
            writer.take_over_when_written(takeovers);
            writer.observe(Some(offer));
            assert!(writer.wait_written(std::time::Duration::from_secs(5)));
            assert_eq!(
                load_path(&taken, &root()),
                Loaded::Saved(one_tab("s-old")),
                "{label}: only the tabs this window offered and now holds are set aside"
            );
            assert_eq!(
                load_as(&dir, &root(), other),
                Loaded::Saved(one_tab("s-new")),
                "{label}: the other window's new session stays in its own file"
            );
            assert!(other_path.exists());
            // And it is offered at the next launch that takes the other window's tabs.
            let (again, _, takeovers) = startup_with(Some(&dir), &root(), Role::Main, &[other]);
            assert_eq!(sessions_of(&again.unwrap()), ["s-old", "s-new"], "{label}");
            assert_eq!(takeovers.len(), 1);
        }
    }

    #[test]
    fn tabs_claimed_by_a_launch_that_never_took_them_are_offered_again_with_newer_ones_after() {
        let dir = ScratchDir::new("nv-tabs-roles", "claimed-again");
        save_as(&dir, &root(), Role::Second, &one_tab("s-old")).unwrap();
        let (_, _, _) = startup_with(Some(&dir), &root(), Role::Main, &[Role::Second]);
        // That launch closed without taking the offer; a window 2 opened later wrote a file of its own.
        save_as(&dir, &root(), Role::Second, &one_tab("s-new")).unwrap();
        let (offer, _, takeovers) = startup_with(Some(&dir), &root(), Role::Main, &[Role::Second]);
        assert_eq!(sessions_of(&offer.unwrap()), ["s-old", "s-new"]);
        assert_eq!(takeovers.len(), 1);
        assert!(!dir.join(file_name_as(&root(), Role::Second)).exists());
        let claimed = dir.join(format!("{}.offered", file_name_as(&root(), Role::Second)));
        let Loaded::Saved(both) = load_path(&claimed, &root()) else {
            panic!("one claim holds both")
        };
        assert_eq!(sessions_of(&both), ["s-old", "s-new"]);
    }

    #[test]
    fn a_file_staged_by_an_interrupted_claim_is_offered_before_the_current_one() {
        let dir = ScratchDir::new("nv-tabs-roles", "staged");
        let name = file_name_as(&root(), Role::Second);
        // A launch moved window 2's file to its staged name and ended before it was claimed.
        save_as(&dir, &root(), Role::Second, &one_tab("s-staged")).unwrap();
        std::fs::rename(dir.join(&name), dir.join(format!("{name}.claiming"))).unwrap();
        save_as(&dir, &root(), Role::Second, &one_tab("s-current")).unwrap();
        let (offer, notes, takeovers) = startup_with(Some(&dir), &root(), Role::Main, &[Role::Second]);
        assert_eq!(sessions_of(&offer.unwrap()), ["s-staged", "s-current"], "{notes:?}");
        assert_eq!(takeovers.len(), 1);
        assert!(!dir.join(format!("{name}.claiming")).exists(), "nothing is left staged");
        assert!(!dir.join(&name).exists());
        let Loaded::Saved(claimed) = load_path(&dir.join(format!("{name}.offered")), &root()) else {
            panic!("one claim holds both")
        };
        assert_eq!(sessions_of(&claimed), ["s-staged", "s-current"]);
    }

    #[test]
    fn another_windows_file_with_no_tabs_is_left_where_it_was() {
        let dir = ScratchDir::new("nv-tabs-roles", "empty-other");
        let second = dir.join(file_name_as(&root(), Role::Second));
        save_as(
            &dir,
            &root(),
            Role::Second,
            &SavedTabs::from_parts(Vec::new(), 0, Vec::new(), None),
        )
        .unwrap();
        let before = std::fs::read(&second).unwrap();
        let (offer, notes, takeovers) = startup_with(Some(&dir), &root(), Role::Main, &[Role::Second]);
        assert!(offer.is_none() && takeovers.is_empty(), "{notes:?}");
        assert_eq!(
            std::fs::read(&second).unwrap(),
            before,
            "the other window's file is as it was"
        );
        let names: Vec<_> = std::fs::read_dir(&*dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "nothing staged or claimed beside it: {names:?}");
    }

    #[test]
    fn a_tab_both_files_list_is_offered_once() {
        let dir = ScratchDir::new("nv-tabs-roles", "dedupe");
        save_as(&dir, &root(), Role::Main, &two()).unwrap();
        save_as(&dir, &root(), Role::Second, &one_tab("s-two")).unwrap();
        let (offer, _, _) = startup_with(Some(&dir), &root(), Role::Main, &[Role::Second]);
        assert_eq!(sessions_of(&offer.unwrap()), ["s-one", "s-two"]);
    }

    #[test]
    fn an_unusable_file_of_another_window_is_not_offered_and_not_moved() {
        let dir = ScratchDir::new("nv-tabs-roles", "unusable");
        let second = dir.join(file_name_as(&root(), Role::Second));
        crate::test_scratch_dir::write_private(&second, "{").unwrap();
        let (offer, notes, takeovers) = startup_with(Some(&dir), &root(), Role::Main, &[Role::Second]);
        assert!(offer.is_none() && takeovers.is_empty());
        assert!(notes.iter().any(|n| n.contains("unusable")), "{notes:?}");
        assert!(second.exists(), "another window's file is left where it is");
    }

    /// Tabs of one chat window, Claude's in order, the one at `active` on screen.
    fn window_tabs(sessions: &[&str], active: usize) -> SavedTabs {
        let tabs = sessions
            .iter()
            .map(|session| tab(session, None, SessionModeChoice::Auto))
            .collect();
        SavedTabs::from_parts(tabs, active, Vec::new(), None)
    }

    fn sessions(saved: &SavedTabs) -> Vec<&str> {
        saved.tabs.iter().map(|t| t.provider_session_id.as_str()).collect()
    }

    fn places(saved: &SavedTabs) -> Vec<(&str, Option<u8>, Option<u8>)> {
        saved
            .tabs
            .iter()
            .map(|t| (t.provider_session_id.as_str(), t.view, t.window))
            .collect()
    }

    /// What the file holds for `merged`, read back.
    fn through_the_file(merged: &SavedTabs) -> SavedTabs {
        decode(&encode(merged, &root()), &root()).unwrap()
    }

    /// What `eitri` mode has of `saved` after a launch and a write of its own: it knows views, not windows.
    fn written_by_eitri_mode(saved: &SavedTabs) -> SavedTabs {
        let mut written = saved.clone();
        for tab in &mut written.tabs {
            tab.window = None;
        }
        written
    }

    #[test]
    fn a_tab_saved_in_c3_reopens_as_view_3_in_eitri_mode_and_back() {
        let merged = merge_windows(&[(WindowName::Chat(3), window_tabs(&["s-3"], 0))], None);
        let file = through_the_file(&merged);
        assert_eq!(places(&file), [("s-3", Some(3), None)]);
        let record = agent::ResumableSession {
            provider: "claude".to_string(),
            provider_session_id: "s-3".to_string(),
            created_at: "1".to_string(),
            updated_at: "2".to_string(),
            title: None,
            name: None,
        };
        let plan = crate::tab_restore::plan(&file, &[record], &[], |_, _| false);
        assert_eq!(
            plan.items[0].view.map(|view| view.number()),
            Some(3),
            "`eitri` mode puts it in `agent#3`"
        );

        let back = tabs_for(&file, &WindowName::Chat(3));
        assert_eq!(places(&back), [("s-3", None, None)]);
        assert_eq!(back.active, Some(0));
        assert!(tabs_for(&file, &WindowName::Chat(1)).is_empty());

        // And a file `eitri` mode wrote opens the same way: its view is a window.
        let viewed = SavedTabs::from_parts(
            vec![
                tab("base", None, SessionModeChoice::Auto),
                SavedTab {
                    view: Some(3),
                    ..tab("s-3", None, SessionModeChoice::Auto)
                },
            ],
            0,
            Vec::new(),
            None,
        );
        assert_eq!(windows_holding(&viewed), [WindowName::Chat(1), WindowName::Chat(3)]);
        assert_eq!(sessions(&tabs_for(&viewed, &WindowName::Chat(3))), ["s-3"]);
        assert_eq!(sessions(&tabs_for(&viewed, &WindowName::Chat(1))), ["base"]);
    }

    #[test]
    fn the_views_a_file_holds_are_listed_once_each() {
        let tabs = [(None, "a"), (Some(3), "b"), (Some(2), "c"), (Some(3), "d")]
            .map(|(view, session)| SavedTab {
                view,
                ..tab(session, None, SessionModeChoice::Auto)
            })
            .to_vec();
        assert_eq!(views_holding(&SavedTabs::from_parts(tabs, 0, Vec::new(), None)), [2, 3]);
    }

    #[test]
    fn a_c3_with_two_tabs_round_trips_through_eitri_mode_without_losing_one() {
        let parts = [(WindowName::Chat(3), window_tabs(&["other", "shown"], 1))];
        let file = through_the_file(&merge_windows(&parts, None));
        assert_eq!(
            places(&file),
            [("other", None, Some(3)), ("shown", Some(3), None)],
            "the tab on screen is the view, and the other stays in the base view with its window named"
        );
        // Read back in compositor mode as it was.
        let again = tabs_for(&file, &WindowName::Chat(3));
        assert_eq!(sessions(&again), ["other", "shown"]);
        assert_eq!(again.active_tab().unwrap().provider_session_id, "shown");
        assert_eq!(windows_holding(&file), [WindowName::Chat(3)]);

        // Through `eitri` mode: one tab in the view, one in the base view, and both are still in the file.
        let flipped = through_the_file(&written_by_eitri_mode(&file));
        assert_eq!(sessions(&tabs_for(&flipped, &WindowName::Chat(3))), ["shown"]);
        assert_eq!(
            sessions(&tabs_for(&flipped, &WindowName::Chat(1))),
            ["other"],
            "the view holds one tab, so the other opens in c1"
        );
    }

    #[test]
    fn a_window_of_other_agents_tabs_alone_keeps_one_on_screen() {
        let codex = |session: &str| SavedTab {
            agent: AgentId::Codex,
            ..tab(session, None, SessionModeChoice::Auto)
        };
        let part = SavedTabs::from_parts(Vec::new(), 0, vec![codex("x-1"), codex("x-2")], None);
        let merged = merge_windows(&[(WindowName::Chat(2), part)], None);
        assert_eq!(places(&merged), [("x-1", Some(2), None), ("x-2", None, Some(2))]);
        let back = tabs_for(&through_the_file(&merged), &WindowName::Chat(2));
        assert_eq!(sessions(&back), ["x-1", "x-2"]);
    }

    #[test]
    fn window_1s_merge_keeps_each_windows_tabs_in_order_and_one_tab_on_screen() {
        let codex = SavedTab {
            agent: AgentId::Codex,
            ..tab("x-2", None, SessionModeChoice::Auto)
        };
        let parts = [
            (
                WindowName::Chat(2),
                SavedTabs::from_parts(Vec::new(), 0, vec![codex], Some(0)),
            ),
            (WindowName::Chat(1), window_tabs(&["a", "b"], 1)),
            (WindowName::Page("markdown".into()), window_tabs(&["no"], 0)),
            (WindowName::Second, window_tabs(&["no"], 0)),
        ];
        let merged = merge_windows(&parts, Some(&WindowName::Chat(1)));
        assert_eq!(
            places(&merged),
            [("a", None, None), ("b", None, None), ("x-2", Some(2), None)],
            "c1 first, whatever order the parts came in, and only chat windows hold tabs"
        );
        assert_eq!(merged.active_tab().unwrap().provider_session_id, "b");
        let on_c2 = merge_windows(&parts, Some(&WindowName::Chat(2)));
        assert_eq!(on_c2.active_tab().unwrap().provider_session_id, "x-2");
        assert_eq!(
            sessions(&tabs_for(&merged, &WindowName::Chat(1))),
            ["a", "b"],
            "each window takes its own"
        );
        assert_eq!(tabs_for(&merged, &WindowName::Chat(1)).active, Some(1));
        assert!(tabs_for(&merged, &WindowName::Page("markdown".into())).is_empty());
    }

    #[test]
    fn a_session_two_windows_list_for_a_moment_is_one_tab() {
        let parts = [
            (WindowName::Chat(1), window_tabs(&["moving"], 0)),
            (WindowName::Chat(2), window_tabs(&["moving", "stays"], 0)),
        ];
        let merged = merge_windows(&parts, None);
        assert_eq!(sessions(&merged), ["moving", "stays"]);
        assert_eq!(through_the_file(&merged), merged);
    }

    #[test]
    fn the_saved_tabs_format_is_unchanged() {
        let codex = SavedTab {
            agent: AgentId::Codex,
            name: Some("reader".to_string()),
            ..tab("x-1", None, SessionModeChoice::Bypass)
        };
        let part = SavedTabs::from_parts(
            vec![
                tab("s-one", Some("api"), SessionModeChoice::Auto),
                tab("s-two", None, SessionModeChoice::Bypass),
            ],
            1,
            vec![codex],
            None,
        );
        let merged = merge_windows(&[(WindowName::Chat(1), part.clone())], Some(&WindowName::Chat(1)));
        assert_eq!(merged, part, "one window with no extra views is that window's own set");
        assert_eq!(
            encode(&merged, &root()),
            r#"{
  "version": 1,
  "project_root": "/home/user/project",
  "tabs": [
    {
      "conversation_id": "0123456789abcdef0123456789abcdef",
      "provider_session_id": "s-one",
      "name": "api",
      "mode": "auto"
    },
    {
      "conversation_id": "0123456789abcdef0123456789abcdef",
      "provider_session_id": "s-two",
      "mode": "bypass"
    }
  ],
  "active": 1,
  "external_tabs": [
    {
      "agent": "codex",
      "conversation_id": "0123456789abcdef0123456789abcdef",
      "provider_session_id": "x-1",
      "name": "reader",
      "mode": "bypass"
    }
  ]
}"#
        );
    }

    /// The row key rides in the file and across the link in both arrays, and a set without keys writes what it
    /// always wrote (above).
    #[test]
    fn a_row_key_crosses_the_file_and_the_link() {
        let keyed = |tab: SavedTab, key: &str| SavedTab {
            row_key: Some(key.to_string()),
            ..tab
        };
        let codex = SavedTab {
            agent: AgentId::Codex,
            ..keyed(tab("x-1", None, SessionModeChoice::Auto), "k-00000000000000ff")
        };
        let saved = SavedTabs::from_parts(
            vec![keyed(tab("s-one", None, SessionModeChoice::Auto), "k-0123456789abcdef")],
            0,
            vec![codex],
            None,
        );
        let text = encode(&saved, &root());
        assert!(text.contains(r#""row_key": "k-0123456789abcdef""#), "{text}");
        assert_eq!(decode(&text, &root()).unwrap(), saved);
        assert_eq!(SavedTabs::from_wire(&saved.to_wire()).unwrap(), saved);
        for one in &saved.tabs {
            assert_eq!(&SavedTab::from_wire(&one.to_wire()).unwrap(), one);
        }
    }

    /// A key that is no key's shape (a hand edit, another build's) is read as none, and never makes the file or the
    /// record unusable.
    #[test]
    fn a_saved_key_of_another_shape_reads_as_absent() {
        for key in [
            r#""k-0123""#,
            r#""K-0123456789abcdef""#,
            r#""k-0123456789ABCDEF""#,
            "17",
            "null",
            "{}",
        ] {
            let text = format!(
                r#"{{"version":1,"project_root":"/home/user/project","tabs":[
                {{"conversation_id":"c","provider_session_id":"s","mode":"auto","row_key":{key}}}],"active":0}}"#
            );
            let saved = decode(&text, &root()).unwrap();
            assert_eq!(saved.tabs[0].row_key, None, "{key}");
            let wire = serde_json::json!({
                "agent": "codex",
                "conversation_id": "c",
                "provider_session_id": "s",
                "mode": "auto",
                "row_key": serde_json::from_str::<serde_json::Value>(key).unwrap(),
            });
            assert_eq!(SavedTab::from_wire(&wire).unwrap().row_key, None, "{key}");
        }
    }

    /// An older build's reader takes the keys it knows and leaves the rest, so `window` costs it nothing; a value no
    /// window can have reads as absent, as a view's does, rather than making the file unusable.
    #[test]
    fn a_key_this_build_does_not_know_and_a_window_no_tab_can_have_leave_the_file_usable() {
        for window in ["1", "10", "\"3\"", "null"] {
            let text = format!(
                r#"{{"version":1,"project_root":"/home/user/project","tabs":[
                {{"conversation_id":"c","provider_session_id":"s","mode":"auto","window":{window},"later":true}}],
                "active":0,"later":{{}}}}"#
            );
            let saved = decode(&text, &root()).unwrap();
            assert_eq!(saved.tabs[0].window, None, "{window}");
        }
    }

    #[test]
    fn saved_tabs_cross_the_link_whole_and_what_is_no_set_of_tabs_is_refused() {
        let codex = SavedTab {
            agent: AgentId::Codex,
            window: Some(4),
            ..tab("x-1", None, SessionModeChoice::Auto)
        };
        let set = SavedTabs::from_parts(
            vec![tab("a", Some("api"), SessionModeChoice::Bypass)],
            0,
            vec![codex],
            Some(0),
        );
        assert_eq!(SavedTabs::from_wire(&set.to_wire()).unwrap(), set);

        assert!(SavedTabs::from_wire(&serde_json::json!([])).is_err());
        assert!(SavedTabs::from_wire(&serde_json::json!({"tabs": [{"session": "s"}]})).is_err());
        let blank = serde_json::json!({
            "tabs": [{"agent": "claude", "conversation_id": " ", "provider_session_id": "s", "mode": "auto"}]
        });
        assert!(SavedTabs::from_wire(&blank).is_err(), "a tab with no session");
        let mut late = set.to_wire();
        late["active"] = serde_json::json!(9);
        late["claude_active"] = serde_json::json!("0");
        let read = SavedTabs::from_wire(&late).unwrap();
        assert_eq!((read.active, read.claude_active), (None, None));
    }

    /// A launch that read the file compares its next set with what the file holds, not with nothing: the last tab of a
    /// group going away empties the file.
    #[test]
    fn a_writer_told_what_the_file_holds_writes_its_emptying() {
        let dir = ScratchDir::new("nv-saved-tabs", "writer-starts-from");
        let saved = window_tabs(&["s-one"], 0);
        save(&dir, &root(), &saved).unwrap();
        let mut writer = TabWriter::new(Some(dir.to_path_buf()), root());
        writer.starts_from(saved);
        writer.observe(Some(SavedTabs::default()));
        assert!(writer.wait_written(std::time::Duration::from_secs(5)));
        assert_eq!(load(&dir, &root()), Loaded::Saved(SavedTabs::default()));
    }
    #[test]
    fn remote_group_tabs_keep_their_window_through_wire_and_disk() {
        let first = SavedTabs::default().with_remote(vec![remote_tab("sc-1")]);
        let third = SavedTabs::default().with_remote(vec![remote_tab("sc-3"), remote_tab("sc-4")]);
        let third = SavedTabs::from_wire(&third.to_wire()).unwrap();
        let merged = merge_windows(
            &[
                (WindowName::Chat(3), third.clone()),
                (WindowName::Chat(1), first.clone()),
            ],
            Some(&WindowName::Chat(3)),
        );
        let saved = decode(&encode(&merged, &root()), &root()).unwrap();
        assert_eq!(windows_holding(&saved), [WindowName::Chat(1), WindowName::Chat(3)]);
        assert_eq!(tabs_for(&saved, &WindowName::Chat(1)), first);
        assert_eq!(tabs_for(&saved, &WindowName::Chat(3)), third);
        assert_eq!(SavedTabs::from_wire(&saved.to_wire()).unwrap(), saved);
    }

    #[test]
    fn a_running_writer_moves_and_flushes_rekeyed_tabs() {
        let base = ScratchDir::new("nv-tabs-move", "writer");
        let old = base.join("old");
        let new = base.join("new");
        let mut writer = TabWriter::new(Some(old.clone()), root());
        writer.observe(Some(SavedTabs::default().with_remote(vec![remote_tab("sc-1")])));
        assert!(writer.wait_written(std::time::Duration::from_secs(60)));
        let before = std::fs::read(old.join(file_name_as(&root(), Role::Main))).unwrap();
        writer.rekey_remote("SHA256:example", "SHA256:new");
        writer.move_to(new.clone()).unwrap();
        assert!(writer.wait_written(std::time::Duration::from_secs(60)));
        let Loaded::Saved(saved) = load(&new, &root()) else {
            panic!("new file missing")
        };
        assert_eq!(saved.remote[0].helper.host_key_sha256, "SHA256:new");
        writer.observe(Some(one_tab("s-after")));
        writer
            .finish()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(60))
            .unwrap();
        assert_eq!(load(&new, &root()), Loaded::Saved(one_tab("s-after")));
        assert_eq!(
            std::fs::read(old.join(file_name_as(&root(), Role::Main))).unwrap(),
            before
        );
    }
}

/// How long a change of the saved tabs holds the thread that observes it (the GTK thread, once per
/// change): `EITRI_MEASURE_DIR` names the directory to write in, so the storage is the one measured.
#[cfg(test)]
mod measure {
    use super::*;

    #[test]
    #[ignore]
    fn measure_a_saved_tabs_write() {
        let dir = PathBuf::from(std::env::var("EITRI_MEASURE_DIR").expect("EITRI_MEASURE_DIR"))
            .join(format!("saved-tabs-{}", std::process::id()));
        let root = Path::new("/p");
        let tab = |i: usize| SavedTab {
            agent: AgentId::Claude,
            conversation_id: format!("c{i}"),
            provider_session_id: format!("s{i}"),
            name: None,
            mode: SessionModeChoice::Auto,
            view: None,
            window: None,
            turn_running: false,
            draft: None,
            row_key: None,
            plan_origin: None,
        };
        let mut memory = TabMemory::new(Some(dir.clone()), root.to_path_buf());
        let mut times = Vec::new();
        for i in 0..200 {
            let now = SavedTabs::from_parts((0..4).map(|t| tab(t + i % 2)).collect(), 0, Vec::new(), None);
            let started = std::time::Instant::now();
            let written = memory.observe(Some(now));
            times.push(started.elapsed());
            assert!(matches!(written, Some(Ok(_))));
        }
        times.sort();
        eprintln!(
            "saved-tabs write in {}: median {:?}, p99 {:?}, max {:?}",
            dir.display(),
            times[100],
            times[198],
            times[199]
        );
        // The same changes through the writer: what the window's thread spends on each.
        let mut writer = TabWriter::new(Some(dir.clone()), root.to_path_buf());
        let mut times = Vec::new();
        for i in 0..200 {
            let now = SavedTabs::from_parts((0..4).map(|t| tab(t + i % 2)).collect(), 0, Vec::new(), None);
            let started = std::time::Instant::now();
            writer.observe(Some(now));
            times.push(started.elapsed());
            // Each change lands before the next, as the window's do.
            assert!(writer.wait_written(std::time::Duration::from_secs(5)));
        }
        times.sort();
        eprintln!(
            "saved-tabs hand-over to the writer in {}: median {:?}, p99 {:?}, max {:?}",
            dir.display(),
            times[100],
            times[198],
            times[199]
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
