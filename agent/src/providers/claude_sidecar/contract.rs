use std::collections::HashSet;

/// The version of the sidecar built from the bundled source revision.
/// Only a selected update is compared against this version to require a newer release.
pub const BUNDLED_SIDECAR_VERSION: &str = "0.3.0";

/// A quiet hint beside the compatibility diagnostic, only where the sidecar command exists.
pub const UNTESTED_CLAUDE_NOTE: &str = "If something looks wrong, a newer sidecar may fix it: eitri sidecar check";

/// A remedy on a refused start, only where the sidecar command exists.
pub const REFUSED_CLAUDE_NOTE: &str = "A newer sidecar may support this claude: eitri sidecar check";

/// The end of the sidecar's out-of-range refusal, written for the sidecar's own developers: it names a source file
/// and a test run. What a person reads in its place is [`FOR_A_PERSON`].
const FOR_ITS_DEVELOPERS: &str = " Install a claude CLI inside that range, or widen the range in ";
const FOR_A_PERSON: &str = " Install a Claude Code version inside that range.";

/// `line` of the sidecar's stderr as a person is shown it: the developer sentence at the end of the refusal of a
/// `claude` outside its range is the person's own instead. Any other line is as it was said.
pub(crate) fn said_for_a_person(line: String) -> String {
    match line.find(FOR_ITS_DEVELOPERS) {
        Some(at) if line.contains("refusing to start") => format!("{}{FOR_A_PERSON}", &line[..at]),
        _ => line,
    }
}

/// Whether a newer sidecar could be what a refusal of the host's `claude` waits for. Not when that `claude` is older
/// than the floor of the range: a newer sidecar moves the floor up, not down. A refusal this cannot read keeps the
/// note.
pub(crate) fn newer_sidecar_may_help(refusal: &str) -> bool {
    let version = refusal
        .split_once("host claude CLI version ")
        .and_then(|(_, rest)| rest.split([':', ' ']).next())
        .and_then(SidecarVersion::parse);
    let floor = refusal
        .split_once("supported range (>=")
        .and_then(|(_, rest)| rest.split([' ', ')']).next())
        .and_then(SidecarVersion::parse);
    match (version, floor) {
        (Some(version), Some(floor)) => version >= floor,
        _ => true,
    }
}

/// Capabilities required only when choosing a selected update, never during the shared
/// handshake or adoption of a running sidecar. Bundled and explicitly named sidecars keep
/// their capability-specific fallback paths.
///
/// `provider_permission_prompts` forwards the CLI's own permission questions to cards;
/// `cli_auto_mode` selects the CLI's Auto permission posture; `permission_defer` leaves
/// decisions pending for the person; `permission_denied_events` reports refusals.
/// `text_delta_message_id` keeps separate replies in separate rows, and
/// `system_prompt_preset` requests Claude Code's system prompt.
pub const REQUIRED_CAPABILITIES: [&str; 6] = [
    "provider_permission_prompts",
    "cli_auto_mode",
    "permission_defer",
    "permission_denied_events",
    "text_delta_message_id",
    "system_prompt_preset",
];

/// A sidecar release's version: `N.N.N`, or `N.N.N-rc.N`, which sorts below its release.
///
/// Parsing is the gate between text on disk and a directory name: nothing else is a version, and a
/// number has no leading zero, so one version has one spelling and one directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SidecarVersion {
    major: u64,
    minor: u64,
    patch: u64,
    stage: Stage,
}

/// The order of the variants is the order of the versions: a candidate comes before its release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Stage {
    Candidate(u64),
    Release,
}

impl SidecarVersion {
    pub fn parse(text: &str) -> Option<SidecarVersion> {
        let (release, stage) = match text.split_once("-rc.") {
            Some((release, candidate)) => (release, Stage::Candidate(version_number(candidate)?)),
            None => (text, Stage::Release),
        };
        let mut numbers = release.split('.').map(version_number);
        let version = SidecarVersion {
            major: numbers.next()??,
            minor: numbers.next()??,
            patch: numbers.next()??,
            stage,
        };
        numbers.next().is_none().then_some(version)
    }
}

/// `u64::from_str` alone would take a `+`.
fn version_number(text: &str) -> Option<u64> {
    let digits = !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    let one_spelling = text == "0" || !text.starts_with('0');
    (digits && one_spelling).then(|| text.parse().ok()).flatten()
}

/// The first of [`REQUIRED_CAPABILITIES`] that `advertised` does not name.
pub fn missing_required<'a>(advertised: impl IntoIterator<Item = &'a str>) -> Option<&'static str> {
    let advertised: HashSet<&str> = advertised.into_iter().collect();
    REQUIRED_CAPABILITIES
        .into_iter()
        .find(|capability| !advertised.contains(capability))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(text: &str) -> SidecarVersion {
        SidecarVersion::parse(text).unwrap_or_else(|| panic!("{text} parses"))
    }

    #[test]
    fn a_version_is_three_numbers_with_an_optional_release_candidate() {
        for text in ["0.0.0", "0.3.2", "12.34.56", "0.3.2-rc.1", "0.3.2-rc.0"] {
            assert!(SidecarVersion::parse(text).is_some(), "{text}");
        }
    }

    #[test]
    fn nothing_else_parses_as_a_version() {
        for text in [
            "",
            "0",
            "0.3",
            "0.3.2.1",
            "v0.3.2",
            "0.3.2 ",
            " 0.3.2",
            "0.3.2\n",
            "+0.3.2",
            "0.+3.2",
            "-0.3.2",
            "0.3.x",
            "0..2",
            "0.3.2-rc",
            "0.3.2-rc.",
            "0.3.2-rc.x",
            "0.3.2-rc.+1",
            "0.3.2-rc.1-rc.2",
            "0.3.2-beta.1",
            "0.3.2+build",
            "../x",
            "0.3/2.1",
            "٣.٣.٣",
            "99999999999999999999999.0.0",
        ] {
            assert_eq!(SidecarVersion::parse(text), None, "{text:?}");
        }
    }

    /// A directory is named by the version's text, so one version has one spelling.
    #[test]
    fn a_number_has_no_leading_zero() {
        for text in ["00.3.2", "0.03.2", "0.3.02", "0.3.2-rc.01"] {
            assert_eq!(SidecarVersion::parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn versions_order_by_number_not_by_text() {
        assert!(version("0.10.0") > version("0.9.9"));
        assert!(version("1.0.0") > version("0.99.99"));
        assert!(version("0.3.10") > version("0.3.9"));
        assert_eq!(version("0.3.2"), version("0.3.2"));
    }

    #[test]
    fn a_release_candidate_sorts_below_its_release_and_above_the_last_one() {
        assert!(version("0.3.2-rc.1") < version("0.3.2"));
        assert!(version("0.3.2-rc.999") < version("0.3.2"));
        assert!(version("0.3.2-rc.1") > version("0.3.1"));
        assert!(version("0.3.2-rc.2") > version("0.3.2-rc.1"));
        assert!(version("0.3.2-rc.10") > version("0.3.2-rc.9"));
    }

    #[test]
    fn the_bundled_version_parses() {
        version(BUNDLED_SIDECAR_VERSION);
    }

    #[test]
    fn nothing_is_missing_from_a_list_with_all_of_them() {
        let mut advertised: Vec<&str> = REQUIRED_CAPABILITIES.to_vec();
        advertised.push("something_else");
        assert_eq!(missing_required(advertised), None);
    }

    #[test]
    fn each_required_capability_is_named_when_it_is_the_one_absent() {
        for absent in REQUIRED_CAPABILITIES {
            let advertised = REQUIRED_CAPABILITIES.iter().copied().filter(|name| *name != absent);
            assert_eq!(missing_required(advertised), Some(absent));
        }
    }

    #[test]
    fn a_capability_is_matched_whole() {
        let advertised = REQUIRED_CAPABILITIES.map(|name| &name[..name.len() - 1]);
        assert_eq!(missing_required(advertised), Some(REQUIRED_CAPABILITIES[0]));
    }
}
