//! What the window takes from a request the control socket accepted, as pure functions of it.

use eitri_core::panel_control::{Received, Request};

/// The chain of processes to name as the panel's partner window for `received`, nearest first, or
/// `None` when the request leaves the partner as it was. An attach brings the chain its sender
/// carried (the verified editor itself for a split, else the sender and its ancestors); a raise
/// carries none, and an empty chain on an attach is the platform not saying who sent it.
pub(crate) fn partner_chain(received: &Received) -> Option<&[u32]> {
    match received.request {
        Request::Attach { .. } => Some(&received.sender_chain),
        Request::Raise => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::panel_control::CloseWith;
    use std::path::PathBuf;

    fn attach(close_with: Option<CloseWith>, chain: &[u32]) -> Received {
        Received {
            request: Request::Attach {
                addr: PathBuf::from("/run/user/1/eitri/n"),
                close_with,
            },
            sender_chain: chain.to_vec(),
        }
    }

    #[test]
    fn a_split_names_its_editor_alone() {
        let received = attach(Some(CloseWith { pid: 77, start: 9 }), &[77]);
        assert_eq!(partner_chain(&received), Some(&[77u32][..]));
    }

    #[test]
    fn a_plain_attach_names_the_senders_chain_and_a_raise_names_nothing() {
        assert_eq!(partner_chain(&attach(None, &[40, 30, 20])), Some(&[40u32, 30, 20][..]));
        // No peer pid (macOS): an empty chain, which clears the partner rather than leaving a stale one.
        assert_eq!(partner_chain(&attach(None, &[])), Some(&[][..]));
        let raise = Received {
            request: Request::Raise,
            sender_chain: vec![],
        };
        assert_eq!(partner_chain(&raise), None);
    }
}
