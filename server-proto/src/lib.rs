pub mod args_hex;
pub mod probe;

pub const PROTOCOL_MAJOR: u32 = 1;
pub const PROTOCOL_MINOR: u32 = 2;
pub const CAP_SIDECARS: &str = "sidecars";
pub const CAP_JOURNAL: &str = "journal";
pub const CAP_PENDING: &str = "pending";
/// The helper runs trust discovery on the server.
pub const CAP_TRUST_SCAN: &str = "trust-scan";
/// Connect accepts an installation name as well as a listed run key.
pub const CAP_INSTALLATION_ARGS: &str = "installation-args";
pub const CAP_SYNC_IGNORES: &str = "sync-ignores";
pub const CAP_GIT_CONFIG: &str = "git-config";
pub const CAP_SYNC_FILES: &str = "sync-files";
/// Server work budget for a tree listing or streamed file read. Clients allow additional time for transport.
pub const SYNC_STREAM_DEADLINE: std::time::Duration = std::time::Duration::from_secs(300);
pub const CAP_SYNC_MODES: &str = "sync-modes";
pub const CAP_EXEC: &str = "exec";
pub const SIDECAR_METADATA: &str = "eitri-sidecar";
pub const JOURNAL_HEAD_METADATA: &str = "eitri-journal-head";
/// Variables a client may pass to StartSidecar. The account travels in its own field;
/// the server supplies the environment, so no client environment variables are allowed.
pub const STARTSIDECAR_ENV_ALLOWED: &[&str] = &[];

tonic::include_proto!("eitri.server.v1");

// Terminal service capability and message bounds.
pub const CAP_TERMINAL: &str = "terminal";
pub const TERM_INPUT_MAX: usize = 64 * 1024;
pub const TERM_OUTPUT_MAX: usize = 64 * 1024;
pub const TERM_SNAPSHOT_PART_MAX: usize = 1024 * 1024;
pub const TERM_SNAPSHOT_MAX: usize = 32 * 1024 * 1024;
pub const TERM_COLUMNS_MAX: u32 = 1000;
pub const TERM_ROWS_MAX: u32 = 500;
pub const TERM_ID_MAX: usize = 64;
pub const TERM_LIST_MAX: usize = 64;
pub const TERM_PATH_MAX: usize = 4096;
pub const TERM_NAME_MAX: usize = 64;

/// A terminal host ended without a durable program exit record.
pub const TERM_GONE_NOTE: &str = "the terminal host ended without recording an exit; programs in the terminal that \
                                ignored the hangup may still run";

/// Accepts only normal, relative UTF-8 paths outside git metadata, before any file is touched.
pub fn check_sync_rel(rel: &str) -> Result<(), String> {
    if rel.chars().any(char::is_control)
        || rel.split('/').any(|part| matches!(part, "" | "." | ".." | ".git"))
        || std::path::Path::new(rel).is_absolute()
    {
        return Err(format!(
            "{rel:?}: expected a normal relative path with no .git component or control characters"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn rels_with_dotdot_absolute_or_a_dot_git_component_are_refused() {
        for rel in [
            "", "/a", "../a", "a/../b", ".", "a/./b", "a//b", "a/", ".git", "a/.git/b", "a\0b", "a\nb", "a\u{7f}",
            "a\u{85}",
        ] {
            let error = super::check_sync_rel(rel).unwrap_err();
            assert!(error.contains(&format!("{rel:?}")), "{error}");
        }
        for rel in ["a", "a/b", ".github/workflows/check.yml", "文件/a", "a..b", "git"] {
            super::check_sync_rel(rel).unwrap();
        }
    }
}
