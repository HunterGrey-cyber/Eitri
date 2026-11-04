use std::path::PathBuf;

use super::room::OriginLink;
use crate::companion::InstallError;

pub struct TakeoverParts {
    pub sockets: crate::companion::Sockets,
    pub commands: CommandsPart,
    pub tmux_swap: Option<TmuxSwap>,
}

pub struct CommandsPart {
    pub socket: PathBuf,
    pub words: Vec<String>,
    pub group: String,
}

pub struct TmuxSwap {
    pub tmux: String,
    pub tmux_pane: String,
    pub pane_switch_socket: PathBuf,
    pub shim_dir: PathBuf,
}

pub struct GlueHandle {
    pub link_chan: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlueNotice {
    UiEntered,
    Replaced,
}

pub fn takeover_install(_link: &OriginLink, _parts: &TakeoverParts) -> Result<GlueHandle, InstallError> {
    Err(InstallError::Malformed("not yet".into()))
}
