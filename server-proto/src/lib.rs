pub mod args_hex;
pub mod probe;

pub const PROTOCOL_MAJOR: u32 = 1;
pub const PROTOCOL_MINOR: u32 = 0;
pub const CAP_SIDECARS: &str = "sidecars";
pub const CAP_JOURNAL: &str = "journal";
pub const CAP_PENDING: &str = "pending";
/// The helper runs trust discovery on the server.
pub const CAP_TRUST_SCAN: &str = "trust-scan";
/// Connect accepts an installation name as well as a listed run key.
pub const CAP_INSTALLATION_ARGS: &str = "installation-args";
pub const SIDECAR_METADATA: &str = "eitri-sidecar";
pub const JOURNAL_HEAD_METADATA: &str = "eitri-journal-head";
/// Variables a client may pass to StartSidecar. The account travels in its own field;
/// the server supplies the environment, so no client environment variables are allowed.
pub const STARTSIDECAR_ENV_ALLOWED: &[&str] = &[];

tonic::include_proto!("eitri.server.v1");
