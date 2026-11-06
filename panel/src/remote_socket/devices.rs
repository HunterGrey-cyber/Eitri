//! `serve`'s errands about devices: an enrollment, a grant, the listen address, a self-revocation, a push endpoint,
//! the report of who is connected. This build parses each and carries none out: each is answered `refused` with
//! `not yet`. A credential an errand carries (an enrollment code, a token) is dropped on the reader's thread and never
//! reaches the tick.

use super::*;

/// An errand as the reader hands it to the tick.
pub(super) enum DevicesInbound {
    /// Parsed, and not carried out by this build.
    NotYet,
}

impl DevicesInbound {
    /// Reads an errand on the reader's thread; `None` for any other line.
    pub(super) fn read(line: RvInbound, _shared: &Shared) -> Option<DevicesInbound> {
        match line {
            RvInbound::Enroll { .. }
            | RvInbound::StartEnroll { .. }
            | RvInbound::GrantRequest { .. }
            | RvInbound::ListenRequest { .. }
            | RvInbound::RevokeSelf { .. }
            | RvInbound::SetPushEndpoint { .. }
            | RvInbound::DevicesConnected { .. } => Some(DevicesInbound::NotYet),
            _ => None,
        }
    }
}

impl RemoteSocket {
    /// Carries out an errand on the tick.
    pub(super) fn take_devices(&self, _handle: &AgentPanelHandle, conn: u64, line: DevicesInbound) {
        match line {
            DevicesInbound::NotYet => self.answer(conn, refused_line(None, NOT_YET)),
        }
    }
}
