use std::fmt;
use std::time::Duration;

use crate::nvim_rpc::NvimLink;

/// A connection to the terminal's nvim, separate from the waiting room.
pub struct OriginLink {
    link: NvimLink,
}

/// A connection to the waiting room, separate from the terminal's nvim.
pub struct RoomLink {
    _link: NvimLink,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    NotYet,
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotYet => f.write_str("not yet"),
        }
    }
}

impl std::error::Error for LinkError {}

impl OriginLink {
    pub fn connect(_addr: &str, _bound: Duration) -> Result<OriginLink, LinkError> {
        Err(LinkError::NotYet)
    }

    pub fn chan(&self) -> u64 {
        self.link.channel_id()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoomEvent {
    Ui(bool),
    TakeBack,
    OriginUnreachable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LetGo {
    Returned,
    NoUi,
}
