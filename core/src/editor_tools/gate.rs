//! What a call needs from the person, the same for every backend: no client's own approval flow for these
//! tools is relied on, so this table is the only one.

use agent::setting_sources::ProjectTrust;
use agent::PermissionMode;

/// What a call turned out to need. A tool reports it at the step where it knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// Inside the project, in a buffer already loaded.
    Nothing,
    /// A file that must be loaded first: a load can start language servers that run the repository's code.
    LoadFile,
    /// A path outside the project.
    OutsideProject,
    /// A change the tool would apply to the person's files.
    Applier,
    /// Code run in the person's editor.
    Exec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Card,
}

/// Bypass allows all but [`Need::Exec`], which is a card in every mode. A load is the one row trust moves:
/// an Auto tab loads without a card only after a yes at the trust question, whether the answer otherwise was a no
/// or nobody was asked.
pub fn verdict(need: Need, mode: PermissionMode, trust: ProjectTrust) -> Verdict {
    match (need, mode, trust) {
        (Need::Nothing, _, _) => Verdict::Allow,
        (Need::Exec, _, _) => Verdict::Card,
        (_, PermissionMode::Bypass, _) => Verdict::Allow,
        (Need::LoadFile, PermissionMode::Auto, ProjectTrust::Trusted) => Verdict::Allow,
        (Need::LoadFile | Need::OutsideProject | Need::Applier, _, _) => Verdict::Card,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ProjectTrust::{NothingFound, Trusted, Untrusted};
    use Verdict::{Allow, Card};

    const NEEDS: [Need; 5] = [
        Need::Nothing,
        Need::LoadFile,
        Need::OutsideProject,
        Need::Applier,
        Need::Exec,
    ];
    const MODES: [PermissionMode; 3] = [PermissionMode::Manual, PermissionMode::Auto, PermissionMode::Bypass];

    #[test]
    fn every_row_of_the_table() {
        // Columns: Manual, Auto, Bypass; each cell is (`y` session, `n` session).
        let table = [
            (Need::Nothing, [(Allow, Allow), (Allow, Allow), (Allow, Allow)]),
            (Need::LoadFile, [(Card, Card), (Allow, Card), (Allow, Allow)]),
            (Need::OutsideProject, [(Card, Card), (Card, Card), (Allow, Allow)]),
            (Need::Applier, [(Card, Card), (Card, Card), (Allow, Allow)]),
            (Need::Exec, [(Card, Card), (Card, Card), (Card, Card)]),
        ];
        assert_eq!(table.len(), NEEDS.len());
        for (need, cells) in table {
            for (mode, (trusted, untrusted)) in MODES.into_iter().zip(cells) {
                assert_eq!(verdict(need, mode, Trusted), trusted, "{need:?} {mode:?} y");
                assert_eq!(
                    verdict(need, mode, NothingFound),
                    untrusted,
                    "{need:?} {mode:?} nobody asked"
                );
                assert_eq!(verdict(need, mode, Untrusted), untrusted, "{need:?} {mode:?} n");
            }
        }
    }

    #[test]
    fn manual_never_allows_what_auto_cards() {
        for need in NEEDS {
            for trust in [Trusted, Untrusted, NothingFound] {
                if verdict(need, PermissionMode::Auto, trust) == Card {
                    assert_eq!(verdict(need, PermissionMode::Manual, trust), Card, "{need:?} {trust:?}");
                }
            }
        }
    }

    #[test]
    fn exec_is_a_card_in_every_mode() {
        for mode in MODES {
            for trust in [Trusted, Untrusted, NothingFound] {
                assert_eq!(verdict(Need::Exec, mode, trust), Card, "{mode:?} {trust:?}");
            }
        }
    }
}
