use agent::ssh::{ChannelLauncher, ControlMaster, ProbeReport, SshLauncher};
use sha2::{Digest, Sha256};
use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use super::manifest::{hash_file, Manifest};
use super::provenance::HelperProvenance;
use super::steps;
use crate::remote_project::{RemoteHost, RemoteProjects};

#[derive(Debug)]
pub enum InstallError {
    Io(std::io::Error),
    HelperMismatch,
    ConsentRequired,
    Reinstall(String),
    Invalid(String),
    Command(String),
    DaemonStillRuns(String),
    SidecarsStillRun(String),
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(f),
            Self::HelperMismatch => f.write_str("server helper differs from this Eitri build; reinstall Eitri"),
            Self::ConsentRequired => f.write_str("server installation needs your consent (y) first"),
            Self::Reinstall(why) => write!(
                f,
                "server files could not be verified; reinstall the server helper: {why}"
            ),
            Self::Invalid(why) | Self::Command(why) | Self::DaemonStillRuns(why) | Self::SidecarsStillRun(why) => {
                f.write_str(why)
            }
        }
    }
}

impl std::error::Error for InstallError {}
impl From<std::io::Error> for InstallError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// A read-only plan. Only a saved, consented host record enables its remote writes.
#[derive(Clone, Debug)]
pub struct InstallPlan {
    helper: PathBuf,
    sidecar: agent::UploadSidecar,
    helper_bytes: u64,
    sidecar_bytes: u64,
    provenance: HelperProvenance,
    data_dir: PathBuf,
    state_dir: PathBuf,
    version: String,
    manifest: Manifest,
    gzip: bool,
    consented: bool,
    host_key: Option<String>,
    warnings: Vec<String>,
    probe_summary: String,
    free_bytes: Option<u64>,
}

impl InstallPlan {
    pub(super) fn check_upload_space(&self) -> Result<(), InstallError> {
        let required = self
            .helper_bytes
            .saturating_add(self.sidecar_bytes)
            .saturating_add(100 * 1024 * 1024);
        if self.free_bytes.is_none_or(|free| free < required) {
            return Err(InstallError::Invalid(format!(
                "server needs {required} free bytes for the upload with 100 MiB to spare"
            )));
        }
        Ok(())
    }
    pub fn version(&self) -> &str {
        &self.version
    }
    pub fn revision(&self) -> &str {
        &self.sidecar.revision
    }
    pub fn sidecar_sha256(&self) -> &str {
        &self.manifest.lines[1].0
    }
    pub fn sidecar_path(&self) -> PathBuf {
        self.install_path().join(&self.manifest.lines[1].1)
    }
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    pub fn install_path(&self) -> PathBuf {
        self.data_dir.join(&self.version)
    }
    /// The helper's data directory on the server, as the probe found it.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }
    /// The helper's state directory on the server, as the probe found it.
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }
    pub(super) fn probe_summary(&self) -> &str {
        &self.probe_summary
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
    pub fn band_label(&self) -> &str {
        self.provenance.band_label()
    }
    pub fn installed_text(&self, host: &str) -> String {
        format!("installed Eitri server {} on {host}", self.version)
    }

    pub fn use_host_record(&mut self, record: &RemoteHost) -> Result<(), InstallError> {
        if record.version != 1 {
            return Err(InstallError::Invalid("unsupported remote host record version".into()));
        }
        self.consented = record.consented;
        self.host_key = Some(record.host_key_sha256.clone());
        Ok(())
    }

    /// The caller invokes this only after the person answers y to the displayed consent text.
    pub fn record_consent(&mut self, store: &RemoteProjects, host: &str, host_key: &str) -> Result<(), InstallError> {
        agent::ssh::validate_host(host).map_err(|error| InstallError::Invalid(error.to_string()))?;
        store.update_host(host_key, |previous| {
            let mut record = previous.unwrap_or_else(|| RemoteHost {
                version: 1,
                host_key_sha256: host_key.into(),
                data_dir: self.data_dir.clone(),
                state_dir: self.state_dir.clone(),
                helpers: Default::default(),
                sidecars: Default::default(),
                consented: false,
            });
            if record.data_dir != self.data_dir || record.state_dir != self.state_dir {
                record.data_dir = self.data_dir.clone();
                record.state_dir = self.state_dir.clone();
                record.helpers.clear();
                record.sidecars.clear();
            }
            record.consented = true;
            record
        })?;
        let record = store
            .host(host_key)?
            .ok_or_else(|| InstallError::Invalid("host record disappeared".into()))?;
        self.use_host_record(&record)
    }

    /// Merge after publication. Content-bearing installation keys keep this merge independent of upload order.
    pub fn record_installed(&self, store: &RemoteProjects) -> Result<(), InstallError> {
        if !self.consented {
            return Err(InstallError::ConsentRequired);
        }
        let key = self.host_key.as_deref().ok_or(InstallError::ConsentRequired)?;
        store.update_host(key, |previous| {
            let mut record = previous.unwrap_or_else(|| RemoteHost {
                version: 0,
                host_key_sha256: key.into(),
                data_dir: self.data_dir.clone(),
                state_dir: self.state_dir.clone(),
                helpers: Default::default(),
                sidecars: Default::default(),
                consented: false,
            });
            if !record.consented {
                record.version = 0;
                return record;
            }
            if record.data_dir != self.data_dir || record.state_dir != self.state_dir {
                record.data_dir = self.data_dir.clone();
                record.state_dir = self.state_dir.clone();
                record.helpers.clear();
                record.sidecars.clear();
            }
            record
                .helpers
                .insert(self.version.clone(), self.provenance.sha256().into());
            record.sidecars.insert(
                sidecar_record_key(&self.version, &self.sidecar.revision),
                self.manifest.lines[1].0.clone(),
            );
            record
        })?;
        Ok(())
    }
}

pub fn plan_install(probe: &ProbeReport, sidecar: &Path) -> Result<InstallPlan, InstallError> {
    let plan = plan_connection(probe, sidecar)?;
    plan.check_upload_space()?;
    Ok(plan)
}

/// Prepare the identity even when no upload would fit: an existing installation needs no upload space.
pub fn plan_connection(probe: &ProbeReport, sidecar: &Path) -> Result<InstallPlan, InstallError> {
    super::probe_step::validate(probe)?;
    let upload = agent::sidecar_for_upload().map_err(InstallError::Invalid)?;
    if upload.path != sidecar {
        return Err(InstallError::Invalid(
            "sidecar is not the artifact resolved for local tabs".into(),
        ));
    }
    let helper = sibling_helper()?;
    let provenance = super::provenance::helper_provenance(helper.parent().expect("sibling parent"))?;
    let version = helper_version(&helper)?;
    make_connection_plan(probe, &provenance, upload, helper, version)
}

/// The plan [`plan_install`] makes, for a helper and sidecar named outright rather than found beside this
/// executable: `provenance` must describe `helper`, whose own `--version` names its installation. Changes nothing.
pub fn plan_for(
    probe: &ProbeReport,
    provenance: &HelperProvenance,
    sidecar: agent::UploadSidecar,
    helper: PathBuf,
) -> Result<InstallPlan, InstallError> {
    let version = helper_version(&helper)?;
    make_plan(probe, provenance, sidecar, helper, version)
}

fn make_plan(
    probe: &ProbeReport,
    provenance: &HelperProvenance,
    sidecar: agent::UploadSidecar,
    helper: PathBuf,
    version: String,
) -> Result<InstallPlan, InstallError> {
    let plan = make_connection_plan(probe, provenance, sidecar, helper, version)?;
    plan.check_upload_space()?;
    Ok(plan)
}

fn make_connection_plan(
    probe: &ProbeReport,
    provenance: &HelperProvenance,
    sidecar: agent::UploadSidecar,
    helper: PathBuf,
    version: String,
) -> Result<InstallPlan, InstallError> {
    let mut warnings = super::probe_step::validate(probe)?;
    warnings.extend(sidecar.warnings.clone());
    if !safe_name(&version) || !valid_revision(&sidecar.revision) {
        return Err(InstallError::Invalid(
            "invalid helper version or sidecar revision".into(),
        ));
    }
    if !matches!(
        sidecar.origin,
        agent::UploadSidecarOrigin::Packaged | agent::UploadSidecarOrigin::PerUserBuild
    ) {
        return Err(InstallError::Invalid(
            "checkout and EITRI_SIDECAR_BINARY sidecars cannot be uploaded".into(),
        ));
    }
    if hash_file(&helper)? != provenance.sha256() {
        return Err(InstallError::HelperMismatch);
    }
    let sidecar_hash = hash_file(&sidecar.path)?;
    let manifest = Manifest {
        lines: vec![
            (provenance.sha256().into(), "bin/eitri-server".into()),
            (
                sidecar_hash.clone(),
                format!(
                    "sidecar/{}/verdandi-claude-sidecar",
                    sidecar_directory(&sidecar.revision, &sidecar_hash)
                ),
            ),
        ],
    };
    let manifest_bytes = manifest.render()?;
    let manifest_hash = format!("{:x}", Sha256::digest(&manifest_bytes));
    let version = format!("{version}-{}-{}", &provenance.sha256()[..16], &manifest_hash[..16]);
    let helper_bytes = std::fs::metadata(&helper)?.len();
    let sidecar_bytes = std::fs::metadata(&sidecar.path)?.len();
    let display_path = |path: Option<&Path>| {
        path.map_or_else(
            || "missing".into(),
            |path| crate::onboarding::display::display_name(&path.to_string_lossy()),
        )
    };
    Ok(InstallPlan {
        helper_bytes,
        sidecar_bytes,
        helper,
        sidecar,
        provenance: provenance.clone(),
        data_dir: probe.data_dir.clone(),
        state_dir: probe.state_dir.clone(),
        version,
        manifest,
        gzip: probe.gzip,
        consented: false,
        host_key: None,
        warnings,
        probe_summary: format!(
            "Server: {}, glibc {}.{}. Helper data: {}. Claude: {}. Git: {}.",
            probe.uname,
            probe.glibc.unwrap_or_default().0,
            probe.glibc.unwrap_or_default().1,
            probe.data_dir.display(),
            display_path(probe.claude.as_deref()),
            display_path(probe.git.as_deref()),
        ),
        free_bytes: probe.free_bytes,
    })
}

pub fn consent_text(plan: &InstallPlan, host: &str) -> String {
    let origin = match plan.provenance {
        HelperProvenance::Release { .. } => "this release build's server helper",
        HelperProvenance::Unsigned { .. } => "unsigned build of the server helper (built from source on this machine)",
    };
    format!("Install {origin} ({} bytes) on {host}, with sidecar build {} ({} bytes), at {}? claude must be installed and logged in on the server. [y/N]",
        plan.helper_bytes, plan.sidecar.revision, plan.sidecar_bytes, plan.install_path().display())
}

pub fn install(master: &ControlMaster, plan: &InstallPlan, progress: &dyn Fn(u64, u64)) -> Result<(), InstallError> {
    install_with(
        &SshLauncher {
            master: master.clone(),
            helper: plan.install_path().join("bin/eitri-server"),
        },
        plan,
        progress,
    )
}

pub fn install_with(
    launcher: &dyn ChannelLauncher,
    plan: &InstallPlan,
    progress: &dyn Fn(u64, u64),
) -> Result<(), InstallError> {
    if !plan.consented {
        return Err(InstallError::ConsentRequired);
    }
    plan.check_upload_space()?;
    let host_key = plan.host_key.as_deref().ok_or(InstallError::ConsentRequired)?;
    // Re-read and check before any remote write; local files may have changed since consent.
    let helper = std::fs::read(&plan.helper)?;
    let sidecar = std::fs::read(&plan.sidecar.path)?;
    use sha2::{Digest, Sha256};
    if format!("{:x}", Sha256::digest(&helper)) != plan.provenance.sha256() {
        return Err(InstallError::HelperMismatch);
    }
    if format!("{:x}", Sha256::digest(&sidecar)) != plan.manifest.lines[1].0 {
        return Err(InstallError::Invalid(
            "local sidecar changed; prepare a new install plan".into(),
        ));
    }
    let helper = payload(helper, plan.gzip)?;
    let sidecar = payload(sidecar, plan.gzip)?;
    let total = (helper.len() + sidecar.len()) as u64;
    progress(0, total);
    let mut uploaded = 0;
    let stage = format!(".install-{}", uuid::Uuid::new_v4().simple());
    let args = [checked_path(&plan.data_dir)?, &stage];
    let result = (|| {
        let child = {
            let _key = launcher.hold_host_key(host_key)?;
            let child = if plan.gzip {
                launcher.spawn_step(steps::UPLOAD, &args)?
            } else {
                launcher.spawn_step(steps::UPLOAD_PLAIN, &args)?
            };
            attached(child)?
        };
        communicate(child, &helper, &|n, _| progress(n, total))?;
        uploaded += helper.len() as u64;
        let mut input = format!("{}\n", sidecar_directory(&plan.sidecar.revision, plan.sidecar_sha256())).into_bytes();
        let header_len = input.len() as u64;
        input.extend_from_slice(&sidecar);
        let child = {
            let _key = launcher.hold_host_key(host_key)?;
            let child = if plan.gzip {
                launcher.spawn_step(steps::UPLOAD_SIDECAR, &args)?
            } else {
                launcher.spawn_step(steps::UPLOAD_SIDECAR_PLAIN, &args)?
            };
            attached(child)?
        };
        communicate(child, &input, &|n, _| {
            progress(uploaded + n.saturating_sub(header_len), total)
        })?;
        let child = {
            let _key = launcher.hold_host_key(host_key)?;
            attached(launcher.spawn_step(steps::WRITE_MANIFEST, &args)?)?
        };
        communicate(child, &plan.manifest.render()?, &|_, _| {})?;
        let input = format!(
            "{}\n{}\n",
            plan.version,
            sidecar_directory(&plan.sidecar.revision, plan.sidecar_sha256())
        );
        let child = {
            let _key = launcher.hold_host_key(host_key)?;
            attached(launcher.spawn_step(steps::PUBLISH, &args)?)?
        };
        communicate(child, input.as_bytes(), &|_, _| {})?;
        progress(total, total);
        Ok(())
    })();
    if result.is_err() {
        let cleanup = (|| {
            let _key = launcher.hold_host_key(host_key)?;
            attached(launcher.spawn_step(steps::CLEANUP, &args)?)
        })();
        if let Ok(child) = cleanup {
            let _ = communicate(child, &[], &|_, _| {});
        }
    }
    result
}

pub fn ensure_started(master: &ControlMaster, plan: &InstallPlan) -> Result<(), InstallError> {
    ensure_started_with(
        &SshLauncher {
            master: master.clone(),
            helper: plan.install_path().join("bin/eitri-server"),
        },
        plan,
    )
}

pub fn ensure_started_with(launcher: &dyn ChannelLauncher, plan: &InstallPlan) -> Result<(), InstallError> {
    if !plan.consented {
        return Err(InstallError::ConsentRequired);
    }
    let args = [checked_path(&plan.data_dir)?, &plan.version];
    let child = {
        let _key = launcher.hold_host_key(plan.host_key.as_deref().ok_or(InstallError::ConsentRequired)?)?;
        attached(launcher.spawn_step(steps::START, &args)?)?
    };
    communicate_start(child, &plan.manifest.render()?, &|_, _| {})?;
    Ok(())
}

pub(crate) fn start_recorded_with(
    launcher: &dyn ChannelLauncher,
    host: &RemoteHost,
    version: &str,
    revision: &str,
) -> Result<(), InstallError> {
    if !host.consented {
        return Err(InstallError::ConsentRequired);
    }
    let manifest = Manifest::from_record(host, version, revision)?;
    start_manifest_with(launcher, host, version, &manifest, false)
}

/// Recovery for an explicit stop checks every artifact recorded for this installation before adoption.
pub(crate) fn recover_recorded_with(
    launcher: &dyn ChannelLauncher,
    host: &RemoteHost,
    installation: &str,
) -> Result<(), InstallError> {
    if !host.consented {
        return Err(InstallError::ConsentRequired);
    }
    if !safe_name(installation) {
        return Err(InstallError::Invalid("invalid installation name".into()));
    }
    let hash = host
        .helpers
        .get(installation)
        .ok_or_else(|| InstallError::Invalid("no recorded helper hash".into()))?;
    let mut manifest = Manifest {
        lines: vec![(hash.clone(), "bin/eitri-server".into())],
    };
    let prefix = format!("{installation}/");
    for key in host.sidecars.keys() {
        if let Some(revision) = key.strip_prefix(&prefix) {
            let recorded = Manifest::from_record(host, installation, revision)?;
            manifest.lines.extend(
                recorded
                    .lines
                    .into_iter()
                    .filter(|(_, path)| path != "bin/eitri-server"),
            );
        }
    }
    start_manifest_with(launcher, host, installation, &manifest, true)
}

/// Checked candidates are sent by the laptop; the server's own manifest cannot authorize a checker.
pub(crate) fn recorded_checkers(host: &RemoteHost, preferred: &str) -> Result<Vec<u8>, InstallError> {
    let mut installations: Vec<_> = host.helpers.iter().collect();
    installations.sort_by_key(|(name, _)| name.as_str() != preferred);
    let mut input = Vec::new();
    for (name, hash) in installations {
        if !safe_name(name) {
            return Err(InstallError::Invalid("invalid installation name".into()));
        }
        Manifest {
            lines: vec![(hash.clone(), "bin/eitri-server".into())],
        }
        .render()?;
        writeln!(input, "{name} {hash}")?;
    }
    Ok(input)
}

pub(crate) fn live_recorded_runs_with(
    launcher: &dyn ChannelLauncher,
    host: &RemoteHost,
    installation: &str,
) -> Result<(Vec<u8>, Vec<u8>), InstallError> {
    if !host.consented {
        return Err(InstallError::ConsentRequired);
    }
    if !safe_name(installation) {
        return Err(InstallError::Invalid("invalid installation name".into()));
    }
    let hash = host
        .helpers
        .get(installation)
        .ok_or_else(|| InstallError::Invalid("no recorded helper hash".into()))?;
    let manifest = Manifest {
        lines: vec![(hash.clone(), "bin/eitri-server".into())],
    }
    .render()?;
    let args = [
        checked_path(&host.data_dir)?,
        installation,
        checked_path(&host.state_dir)?,
    ];
    let child = {
        let _key = launcher.hold_host_key(&host.host_key_sha256)?;
        attached(launcher.spawn_step(steps::CHECK_SIDECARS, &args)?)?
    };
    communicate_start_output(child, &manifest, &|_, _| {})
}

fn start_manifest_with(
    launcher: &dyn ChannelLauncher,
    host: &RemoteHost,
    version: &str,
    manifest: &Manifest,
    recovery: bool,
) -> Result<(), InstallError> {
    let run_key = super::cli::installation_run_key(version);
    let mut args = vec![checked_path(&host.data_dir)?, version];
    if recovery {
        args.push(checked_path(&host.state_dir)?);
        args.push(&run_key);
    }
    let child = {
        let _key = launcher.hold_host_key(&host.host_key_sha256)?;
        attached(launcher.spawn_step(steps::START, &args)?)?
    };
    communicate_start(child, &manifest.render()?, &|_, _| {})?;
    Ok(())
}

fn payload(bytes: Vec<u8>, gzip: bool) -> Result<Vec<u8>, InstallError> {
    if !gzip {
        return Ok(bytes);
    }
    let child = Command::new("gzip")
        .arg("-c")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    communicate_uncapped(child, &bytes, &|_, _| {}, usize::MAX, None).map(|(out, _)| out)
}

/// How long a lifecycle step's channel may take to print its attachment marker. A floor sized for a slow link, not a
/// figure measured on this one: the marker comes back after a channel open and an exec through the master and the
/// server starting a shell, a few round trips that a link of a few hundred milliseconds stretches past five seconds
/// when the master's connection is busy with an upload or the server is short of memory. Only a channel that never
/// answers is made to wait this long.
pub(crate) const ATTACHMENT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

/// Consume only the attachment marker, with the caller's key guard still held. No payload may be sent before it.
pub(crate) fn attached(child: Child) -> Result<Child, InstallError> {
    attached_within(child, ATTACHMENT_DEADLINE)
}

fn attached_within(mut child: Child, wait: std::time::Duration) -> Result<Child, InstallError> {
    use std::os::fd::AsRawFd;
    #[cfg(test)]
    before_attachment::run();
    let result = (|| {
        let stdout = child
            .stdout
            .as_mut()
            .ok_or_else(|| InstallError::Invalid("lifecycle launcher did not pipe stdout".into()))?;
        let deadline = std::time::Instant::now() + wait;
        for expected in agent::ssh::channel::LIFECYCLE_ATTACHED {
            loop {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    return Err(InstallError::Command("lifecycle attachment timed out".into()));
                }
                let mut fd = libc::pollfd {
                    fd: stdout.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: stdout owns this descriptor throughout the poll and its one-byte read.
                let ready =
                    unsafe { libc::poll(&mut fd, 1, remaining.as_millis().max(1).min(i32::MAX as u128) as i32) };
                if ready < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error.into());
                }
                if ready == 0 {
                    continue;
                }
                let mut byte = [0];
                match stdout.read(&mut byte) {
                    Ok(1) if byte[0] == *expected => break,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error.into()),
                    _ => {
                        return Err(InstallError::Command(
                            "invalid or missing lifecycle attachment marker".into(),
                        ))
                    }
                }
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    Ok(child)
}

#[cfg(test)]
pub(crate) mod before_attachment {
    use std::cell::RefCell;

    thread_local! {
        pub(crate) static HOOK: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
    }

    pub(super) fn run() {
        if let Some(hook) = HOOK.with(|cell| cell.borrow_mut().take()) {
            hook();
        }
    }
}

pub(crate) fn communicate(child: Child, input: &[u8], progress: &dyn Fn(u64, u64)) -> Result<Vec<u8>, InstallError> {
    communicate_uncapped(child, input, progress, 64 * 1024, None).map(|(out, _)| out)
}

pub(super) fn communicate_start(
    child: Child,
    input: &[u8],
    progress: &dyn Fn(u64, u64),
) -> Result<Vec<u8>, InstallError> {
    communicate_start_output(child, input, progress).map(|(out, _)| out)
}

fn communicate_start_output(
    child: Child,
    input: &[u8],
    progress: &dyn Fn(u64, u64),
) -> Result<(Vec<u8>, Vec<u8>), InstallError> {
    if input.len() > 4096 {
        return Err(InstallError::Invalid("startup manifest exceeds one pipe write".into()));
    }
    communicate_uncapped(
        child,
        input,
        progress,
        64 * 1024,
        Some(std::time::Duration::from_secs(25)),
    )
}

fn communicate_uncapped(
    mut child: Child,
    input: &[u8],
    progress: &dyn Fn(u64, u64),
    cap: usize,
    timeout: Option<std::time::Duration>,
) -> Result<(Vec<u8>, Vec<u8>), InstallError> {
    let deadline = timeout.map(|wait| std::time::Instant::now() + wait);
    let mut stdin = child.stdin.take().expect("launcher pipes stdin");
    let mut stdout = child.stdout.take().expect("launcher pipes stdout");
    let mut stderr = child.stderr.take().expect("launcher pipes stderr");
    std::thread::scope(|scope| {
        let out = scope.spawn(move || {
            let mut bytes = Vec::new();
            stdout
                .by_ref()
                .take(cap.saturating_add(1) as u64)
                .read_to_end(&mut bytes)?;
            Ok::<_, std::io::Error>(bytes)
        });
        let err = scope.spawn(move || {
            let mut tail = Vec::new();
            let mut buf = [0; 8192];
            loop {
                let n = stderr.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                tail.extend_from_slice(&buf[..n]);
                if tail.len() > 64 * 1024 {
                    tail.drain(..tail.len() - 64 * 1024);
                }
            }
            Ok::<_, std::io::Error>(tail)
        });
        let written = (|| {
            let mut done = 0;
            for chunk in input.chunks(64 * 1024) {
                stdin.write_all(chunk)?;
                done += chunk.len() as u64;
                progress(done, input.len() as u64);
            }
            Ok::<_, std::io::Error>(())
        })();
        drop(stdin);
        // START can refuse before reading its manifest. Keep its exit status and diagnostic rather than
        // replacing them with SIGKILL; its deadline still bounds a child that stopped reading and stays alive.
        if written.is_err() && deadline.is_none() {
            let _ = child.kill();
        }
        #[cfg(test)]
        after_start_input::run();
        let mut timed_out = false;
        let status = if let Some(deadline) = deadline {
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => break Ok(status),
                    Ok(None) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(20))
                    }
                    Ok(None) => {
                        timed_out = true;
                        let _ = child.kill();
                        break child.wait();
                    }
                    Err(error) => break Err(error),
                }
            }
        } else {
            child.wait()
        };
        let out = out.join().expect("output reader panicked")?;
        let err = err.join().expect("error reader panicked")?;
        let status = status?;
        if timed_out {
            return Err(InstallError::Command("checked helper startup timed out".into()));
        }
        if status.code() == Some(66) {
            return Err(InstallError::Reinstall(
                agent::remote_client::MISSING_INSTALLATION.into(),
            ));
        }
        if status.code() == Some(67) {
            return Err(InstallError::Reinstall(
                String::from_utf8_lossy(&err).trim().to_string(),
            ));
        }
        if status.code() == Some(75) {
            return Err(InstallError::DaemonStillRuns(
                String::from_utf8_lossy(&err).trim().to_string(),
            ));
        }
        if status.code() == Some(76) {
            return Err(InstallError::SidecarsStillRun(
                String::from_utf8_lossy(&err).trim().to_string(),
            ));
        }
        if !status.success() {
            return Err(InstallError::Command(String::from_utf8_lossy(&err).trim().to_string()));
        }
        written?;
        if out.len() > cap {
            return Err(InstallError::Invalid("command output exceeds 64 KiB".into()));
        }
        Ok((out, err))
    })
}

fn sibling_helper() -> Result<PathBuf, InstallError> {
    Ok(std::env::current_exe()?
        .parent()
        .ok_or_else(|| InstallError::Invalid("Eitri executable has no directory".into()))?
        .join("eitri-server"))
}

fn helper_version(helper: &Path) -> Result<String, InstallError> {
    let output = Command::new(helper).arg("--version").output()?;
    let text =
        std::str::from_utf8(&output.stdout).map_err(|_| InstallError::Invalid("helper version is not UTF-8".into()))?;
    let fields: Vec<_> = text.split_whitespace().collect();
    let ["eitri-server", version, revision] = fields.as_slice() else {
        return Err(InstallError::Invalid("invalid helper version output".into()));
    };
    if *revision == "unknown" {
        return Err(InstallError::Invalid("this helper was built without a Git identity; rebuild from a Git checkout or set EITRI_BUILD_COMMIT to the source commit".into()));
    }
    if !output.status.success() || !safe_name(version) || !valid_revision(revision) {
        return Err(InstallError::Invalid("invalid helper version identity".into()));
    }
    Ok(format!("{version}-{revision}"))
}

pub(crate) fn sidecar_directory(revision: &str, hash: &str) -> String {
    format!("{revision}-{}", &hash[..16])
}

pub(crate) fn sidecar_record_key(installation: &str, revision: &str) -> String {
    format!("{installation}/{revision}")
}

pub(crate) fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
}
pub(crate) fn valid_revision(revision: &str) -> bool {
    matches!(revision.len(), 7 | 40)
        && revision
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub(crate) fn checked_path(path: &Path) -> Result<&str, InstallError> {
    path.to_str()
        .filter(|path| path.starts_with('/') && path.len() > 1 && path.split('/').skip(1).all(safe_name))
        .ok_or_else(|| {
            InstallError::Invalid("server directory must be an absolute normal path using /[A-Za-z0-9._/+-]+".into())
        })
}

/// A plan for a fixture helper and sidecar under `root`, and the launcher that runs its steps there: what other
/// modules' tests install and start.
#[cfg(test)]
pub(crate) fn test_plan(root: &Path, flavor: &str) -> (InstallPlan, agent::ssh::LocalExec) {
    tests::fixture(root, flavor, false)
}

/// The consented host record of `plan` installed, as `record_installed` leaves it.
#[cfg(test)]
pub(crate) fn test_host_record(plan: &InstallPlan) -> RemoteHost {
    tests::record(plan)
}

#[cfg(test)]
pub(crate) fn test_plan_without_upload_space(root: &Path) -> InstallPlan {
    let mut plan = tests::fixture(root, "open", false).0;
    plan.free_bytes = Some(0);
    plan
}

#[cfg(test)]
mod after_start_input {
    use std::cell::RefCell;

    thread_local! {
        pub(super) static HOOK: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
    }

    pub(super) fn run() {
        if let Some(hook) = HOOK.with(|cell| cell.borrow_mut().take()) {
            hook();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_scratch_dir::ScratchDir;
    use agent::ssh::LocalExec;
    use std::os::unix::fs::PermissionsExt;

    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn stop_recovery_checks_recorded_sidecar_hashes_before_adopting() {
        let root = crate::test_scratch_dir::ScratchDir::new("remote", "recover-hashes");
        let (mut plan, launcher) = super::test_plan(&root, "recover");
        let host = super::test_host_record(&plan);
        plan.use_host_record(&host).unwrap();
        super::install_with(&launcher, &plan, &|_, _| {}).unwrap();
        super::recover_recorded_with(&launcher, &host, plan.version()).unwrap();
        let started = plan.install_path().join("started");
        assert!(started.exists());
        std::fs::remove_file(&started).unwrap();
        std::fs::write(plan.install_path().join(&plan.manifest.lines[1].1), b"changed").unwrap();
        assert!(super::recover_recorded_with(&launcher, &host, plan.version()).is_err());
        assert!(!started.exists());
    }

    #[test]
    fn a_start_that_closes_stdin_keeps_its_reinstall_status() {
        for (code, expected) in [
            (66, agent::remote_client::MISSING_INSTALLATION),
            (67, "manifest refused"),
        ] {
            let mut child = Command::new("sh")
                .args([
                    "-c",
                    "exec 0<&-; kill -STOP $$; printf 'manifest refused' >&2; exit \"$1\"",
                    "sh",
                    &code.to_string(),
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let pid = child.id() as libc::pid_t;
            let mut status = 0;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            loop {
                // SAFETY: this child belongs to the test and status lives through the call.
                let found = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED | libc::WNOHANG) };
                if found == pid {
                    break;
                }
                if found < 0 || std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("the child did not stop after closing stdin");
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            assert!(libc::WIFSTOPPED(status));
            after_start_input::HOOK.with(|cell| {
                *cell.borrow_mut() = Some(Box::new(move || {
                    // SAFETY: only the test's stopped child is resumed, after the manifest write has failed.
                    unsafe { libc::kill(pid, libc::SIGCONT) };
                }));
            });
            let error = communicate_start(child, b"manifest\n", &|_, _| {}).unwrap_err();
            assert!(
                matches!(&error, InstallError::Reinstall(why) if why == expected),
                "{error:?}"
            );
        }
    }

    #[test]
    fn a_helper_without_git_identity_explains_how_to_build_an_installable_one() {
        let root = ScratchDir::new("remote", "unknown-build");
        let helper = root.join("eitri-server");
        std::fs::write(&helper, "#!/bin/sh\nprintf 'eitri-server 0.3.0 unknown\\n'\n").unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let why = helper_version(&helper).unwrap_err().to_string();
        assert!(
            why.contains("without a Git identity") && why.contains("EITRI_BUILD_COMMIT"),
            "{why}"
        );
    }

    pub(super) fn fixture(root: &Path, flavor: &str, gzip: bool) -> (InstallPlan, LocalExec) {
        let local = root.join(format!("local-{flavor}"));
        std::fs::create_dir_all(&local).unwrap();
        let helper = local.join("eitri-server");
        let mut report = super::super::probe_step::fixture(root);
        report.root = agent::ssh::RootState::Ok;
        let report = report.render();
        std::fs::write(
            &helper,
            format!("#!/bin/sh\nif test \"$1\" = probe; then cat <<'REPORT'\n{report}REPORT\nexit 0\nfi\ntest \"$1\" = ensure || exit 1\ntest ! -e ensure-fails || {{ echo ensure-refused >&2; exit 1; }}\nprintf {flavor} > started\n"),
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let sidecar = local.join("verdandi-claude-sidecar");
        std::fs::write(&sidecar, format!("sidecar {flavor}")).unwrap();
        let provenance = HelperProvenance::Unsigned {
            sha256: hash_file(&helper).unwrap(),
        };
        let mut probe = super::super::probe_step::fixture(root);
        probe.gzip = gzip;
        let upload = agent::UploadSidecar {
            path: sidecar,
            revision: "abcdef0".into(),
            origin: agent::UploadSidecarOrigin::PerUserBuild,
            warnings: Vec::new(),
        };
        let plan = make_plan(&probe, &provenance, upload, helper, "0.3.0-123abcd".into()).unwrap();
        (
            plan,
            LocalExec {
                helper: root.join("fixture-helper"),
            },
        )
    }

    #[test]
    fn probe_summary_reveals_hidden_characters_in_server_tool_paths() {
        let root = ScratchDir::new("remote", "probe-tool-paths");
        let (plan, _) = fixture(&root, "probe", false);
        let mut probe = super::super::probe_step::fixture(&root);
        probe.claude = Some("/bin/claude\x1b[2J".into());
        probe.git = Some("/bin/git\u{202e}".into());
        let plan = make_plan(&probe, &plan.provenance, plan.sidecar, plan.helper, "0.3.0".into()).unwrap();
        assert!(plan
            .probe_summary()
            .contains(r"Claude: /bin/claude\u{1b}[2J. Git: /bin/git\u{202e}."));
        assert!(plan
            .probe_summary()
            .chars()
            .all(|ch| !ch.is_control() && ch != '\u{202e}'));
    }

    pub(super) fn record(plan: &InstallPlan) -> RemoteHost {
        RemoteHost {
            version: 1,
            host_key_sha256: "SHA256:example".into(),
            data_dir: plan.data_dir.clone(),
            state_dir: plan.state_dir.clone(),
            helpers: [(plan.version.clone(), plan.provenance.sha256().into())].into(),
            sidecars: [(
                sidecar_record_key(&plan.version, &plan.sidecar.revision),
                plan.manifest.lines[1].0.clone(),
            )]
            .into(),
            consented: true,
        }
    }

    fn authorize(plan: &mut InstallPlan) {
        plan.use_host_record(&record(plan)).unwrap();
    }

    struct GuardedSteps {
        local: LocalExec,
        held: std::sync::Arc<std::sync::atomic::AtomicBool>,
        starts: std::sync::atomic::AtomicUsize,
        waiting: PathBuf,
        release: PathBuf,
    }

    struct KeyGuard(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for KeyGuard {
        fn drop(&mut self) {
            self.0.store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl ChannelLauncher for GuardedSteps {
        fn spawn(&self, _: &[std::ffi::OsString]) -> std::io::Result<Child> {
            unreachable!()
        }

        fn hold_host_key(&self, expected: &str) -> std::io::Result<Box<dyn Send>> {
            assert_eq!(expected, "SHA256:example");
            if self.held.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return Err(std::io::Error::from(std::io::ErrorKind::WouldBlock));
            }
            Ok(Box::new(KeyGuard(self.held.clone())))
        }

        fn spawn_step(&self, literal: &'static str, args: &[&str]) -> std::io::Result<Child> {
            assert!(
                self.held.load(std::sync::atomic::Ordering::SeqCst),
                "the key check and spawn must exclude a restart"
            );
            if literal == steps::START {
                let first = self.starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
                return Command::new("sh")
                    .args([
                        "-c",
                        if first {
                            r#"printf "eitri-lifecycle-attached-v1\n"; cat >/dev/null; touch "$1"; while ! test -e "$2"; do sleep 0.01; done"#
                        } else {
                            r#"printf "eitri-lifecycle-attached-v1\n"; cat >/dev/null"#
                        },
                        "sh",
                    ])
                    .arg(&self.waiting)
                    .arg(&self.release)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn();
            }
            match literal {
                steps::UPLOAD => self.local.spawn_step(steps::UPLOAD, args),
                steps::UPLOAD_PLAIN => self.local.spawn_step(steps::UPLOAD_PLAIN, args),
                steps::UPLOAD_SIDECAR => self.local.spawn_step(steps::UPLOAD_SIDECAR, args),
                steps::UPLOAD_SIDECAR_PLAIN => self.local.spawn_step(steps::UPLOAD_SIDECAR_PLAIN, args),
                steps::WRITE_MANIFEST => self.local.spawn_step(steps::WRITE_MANIFEST, args),
                steps::PUBLISH => self.local.spawn_step(steps::PUBLISH, args),
                steps::CLEANUP => self.local.spawn_step(steps::CLEANUP, args),
                _ => unreachable!(),
            }
        }
    }

    fn guarded(root: &Path, local: LocalExec) -> GuardedSteps {
        GuardedSteps {
            local,
            held: Default::default(),
            starts: Default::default(),
            waiting: root.join("waiting"),
            release: root.join("release"),
        }
    }

    #[test]
    fn concurrent_starts_do_not_hold_the_restart_lock_while_waiting_for_start() {
        let root = ScratchDir::new("remote", "concurrent-starts");
        let (mut plan, local) = fixture(&root, "one", false);
        authorize(&mut plan);
        let launcher = guarded(&root, local);
        std::thread::scope(|threads| {
            let first = threads.spawn(|| ensure_started_with(&launcher, &plan));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            while !launcher.waiting.exists() {
                assert!(
                    !first.is_finished(),
                    "the first start ended before consuming its manifest"
                );
                assert!(
                    std::time::Instant::now() < deadline,
                    "the first start never received its manifest"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let second = start_recorded_with(&launcher, &record(&plan), &plan.version, &plan.sidecar.revision);
            std::fs::write(&launcher.release, b"go").unwrap();
            first.join().unwrap().unwrap();
            second.unwrap();
            assert_eq!(launcher.starts.load(std::sync::atomic::Ordering::SeqCst), 2);
        });
    }

    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn uploads_release_the_restart_lock_between_spawn_and_progress() {
        let root = ScratchDir::new("remote", "upload-restart-lock");
        let (mut plan, local) = fixture(&root, "one", false);
        authorize(&mut plan);
        let launcher = guarded(&root, local);
        install_with(&launcher, &plan, &|_, _| {
            assert!(
                !launcher.held.load(std::sync::atomic::Ordering::SeqCst),
                "upload progress must not hold up another window's start"
            );
        })
        .unwrap();
    }

    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_upload_keeps_the_restart_lock_until_its_delayed_attachment() {
        let root = ScratchDir::new("remote", "upload-attach");
        let (mut plan, local) = fixture(&root, "one", false);
        authorize(&mut plan);
        let launcher = super::super::tests::DelayedAttachment {
            local,
            target: steps::UPLOAD_PLAIN,
            root: root.to_path_buf(),
            held: Default::default(),
            refused: Default::default(),
        };
        install_with(&launcher, &plan, &|done, _| {
            assert!(!launcher.held.load(std::sync::atomic::Ordering::SeqCst));
            if done > 0 {
                assert!(
                    launcher.refused.load(std::sync::atomic::Ordering::SeqCst),
                    "replacement must be refused until the child attaches"
                );
            }
        })
        .unwrap();
        assert!(launcher.refused.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn an_absent_or_wrong_attachment_marker_kills_and_reaps_without_sending_input() {
        for output in ["", "wrong\\n", "eitri-lifecycle-attached-v1"] {
            let child = Command::new("sh")
                .args(["-c", "printf '%b' \"$1\"; exec 1>&-; exec cat", "sh", output])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let pid = child.id() as libc::pid_t;
            let result = attached(child);
            assert!(matches!(result, Err(InstallError::Command(_))));
            let mut status = 0;
            assert_eq!(unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) }, -1);
            assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ECHILD));
        }
    }

    #[test]
    fn a_marker_slower_than_five_seconds_still_attaches() {
        let child = Command::new("sh")
            .args(["-c", "sleep 6; printf 'eitri-lifecycle-attached-v1\\n'; exec cat"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut child = attached(child).expect("a slow link's marker attaches");
        drop(child.stdin.take());
        assert!(child.wait().unwrap().success());
    }

    #[test]
    fn a_silent_attachment_is_bounded_and_reaped() {
        let child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id() as libc::pid_t;
        let result = attached_within(child, std::time::Duration::ZERO);
        assert!(matches!(result, Err(InstallError::Command(why)) if why.contains("timed out")));
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) }, -1);
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ECHILD));
    }

    #[test]
    fn an_upload_without_its_marker_receives_no_payload_or_progress() {
        struct Unmarked {
            local: LocalExec,
            received: PathBuf,
        }
        impl ChannelLauncher for Unmarked {
            fn spawn(&self, _: &[std::ffi::OsString]) -> std::io::Result<Child> {
                unreachable!()
            }
            fn spawn_step(&self, literal: &'static str, args: &[&str]) -> std::io::Result<Child> {
                if literal == steps::CLEANUP {
                    return self.local.spawn_step(steps::CLEANUP, args);
                }
                assert_eq!(literal, steps::UPLOAD_PLAIN, "no subsequent installation step may run");
                Command::new("sh")
                    .args(["-c", r#"exec 3>"$1"; exec 1>&-; exec cat >&3"#, "sh"])
                    .arg(&self.received)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
            }
        }
        let root = ScratchDir::new("remote", "upload-no-marker");
        let (mut plan, local) = fixture(&root, "one", false);
        authorize(&mut plan);
        let launcher = Unmarked {
            local,
            received: root.join("received"),
        };
        let result = install_with(&launcher, &plan, &|done, _| assert_eq!(done, 0));
        assert!(matches!(result, Err(InstallError::Command(why)) if why.contains("attachment marker")));
        assert_eq!(std::fs::read(&launcher.received).unwrap(), b"");
        assert!(!plan.install_path().exists());
    }

    #[test]
    fn different_bytes_of_one_revision_have_distinct_installation_paths() {
        let root = ScratchDir::new("remote", "content-identity");
        let (one, _) = fixture(&root, "one", false);
        let (two, _) = fixture(&root, "two", false);
        assert_ne!(one.install_path(), two.install_path());
        assert_ne!(one.manifest.lines[1].1, two.manifest.lines[1].1);
    }

    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn changed_server_directories_reuse_consent_and_replace_installation_records() {
        let root = ScratchDir::new("remote", "relocate");
        let (mut old, _) = fixture(&root, "old", false);
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        old.record_consent(&store, "example-host", "SHA256:example").unwrap();
        let (mut new, launcher) = fixture(&root, "new", false);
        new.data_dir = root.join("new-data");
        new.state_dir = root.join("new-state");
        new.use_host_record(&store.host("SHA256:example").unwrap().unwrap())
            .unwrap();
        assert!(new.consented);
        install_with(&launcher, &new, &|_, _| {}).unwrap();
        new.record_installed(&store).unwrap();
        let saved = store.host("SHA256:example").unwrap().unwrap();
        assert_eq!(saved.data_dir, new.data_dir);
        assert_eq!(saved.state_dir, new.state_dir);
        assert_eq!(saved.helpers.len(), 1);
        assert_eq!(saved.sidecars.len(), 1);
        assert!(saved.helpers.contains_key(&new.version));
    }

    #[test]
    fn nothing_is_written_before_consent() {
        let root = ScratchDir::new("remote", "consent");
        let (plan, launcher) = fixture(&root, "one", true);
        assert!(!plan.data_dir.exists());
        assert!(matches!(
            install_with(&launcher, &plan, &|_, _| {}),
            Err(InstallError::ConsentRequired)
        ));
        assert!(matches!(
            ensure_started_with(&launcher, &plan),
            Err(InstallError::ConsentRequired)
        ));
        assert!(!plan.data_dir.exists());
        let consent = consent_text(&plan, "example-host");
        for text in [
            "unsigned build of the server helper (built from source on this machine)",
            "abcdef0",
            "bytes",
            "claude must be installed and logged in",
            "example-host",
        ] {
            assert!(consent.contains(text), "{consent}");
        }
    }

    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn concurrent_installs_leave_one_complete_tree() {
        let root = ScratchDir::new("remote", "concurrent");
        let (mut one, launcher) = fixture(&root, "one", true);
        let (mut two, _) = fixture(&root, "two", false);
        authorize(&mut one);
        authorize(&mut two);
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            for plan in [&one, &two] {
                let launcher = &launcher;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    install_with(launcher, plan, &|_, _| {}).unwrap();
                });
            }
        });
        for plan in [&one, &two] {
            let dir = plan.install_path();
            assert_eq!(
                hash_file(&dir.join("bin/eitri-server")).unwrap(),
                plan.provenance.sha256()
            );
            assert_eq!(
                std::fs::read(dir.join("MANIFEST")).unwrap(),
                plan.manifest.render().unwrap()
            );
            assert_eq!(hash_file(&plan.sidecar_path()).unwrap(), plan.manifest.lines[1].0);
            ensure_started_with(&launcher, plan).unwrap();
            for path in [
                dir.clone(),
                dir.join("bin"),
                dir.join("bin/eitri-server"),
                plan.sidecar_path().parent().unwrap().to_owned(),
                plan.sidecar_path(),
                dir.join("MANIFEST"),
            ] {
                assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
            }
        }
        assert!(std::fs::read_dir(&one.data_dir).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_str()
            .unwrap()
            .starts_with(".install-")));
    }

    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn failed_upload_leaves_no_temporary_file() {
        let root = ScratchDir::new("remote", "failure");
        let (plan, launcher) = fixture(&root, "one", true);
        let data = checked_path(&plan.data_dir).unwrap();
        let child = launcher.spawn_step(steps::UPLOAD, &[data, ".install-failure"]).unwrap();
        assert!(communicate(child, b"invalid gzip", &|_, _| {}).is_err());
        assert_eq!(
            std::fs::read_dir(plan.data_dir.join(".install-failure/bin"))
                .unwrap()
                .count(),
            0
        );
        let child = launcher
            .spawn_step(steps::UPLOAD_SIDECAR, &[data, ".install-failure"])
            .unwrap();
        assert!(communicate(child, b"abcdef0\ninvalid gzip", &|_, _| {}).is_err());
        assert_eq!(
            std::fs::read_dir(plan.data_dir.join(".install-failure/sidecar/abcdef0"))
                .unwrap()
                .count(),
            0
        );
    }

    #[cfg(target_os = "linux")]
    fn changed_file_refuses(relative: &str) {
        let root = ScratchDir::new("remote", "changed");
        let (mut plan, launcher) = fixture(&root, "one", true);
        authorize(&mut plan);
        let progress = std::cell::RefCell::new(Vec::new());
        install_with(&launcher, &plan, &|done, total| {
            progress.borrow_mut().push((done, total))
        })
        .unwrap();
        let progress = progress.into_inner();
        assert_eq!(progress[0].0, 0);
        assert_eq!(progress.last().unwrap().0, progress.last().unwrap().1);
        let relative = if relative == "sidecar" {
            &plan.manifest.lines[1].1
        } else {
            relative
        };
        let path = plan.install_path().join(relative);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[0] ^= 1;
        std::fs::write(path, bytes).unwrap();
        let error = ensure_started_with(&launcher, &plan).unwrap_err();
        assert!(matches!(error, InstallError::Reinstall(_)));
        assert!(error.to_string().contains("reinstall"));
        assert!(!plan.install_path().join("started").exists());
        install_with(&launcher, &plan, &|_, _| {}).unwrap();
        ensure_started_with(&launcher, &plan).unwrap();
        assert_eq!(
            std::fs::read_to_string(plan.install_path().join("started")).unwrap(),
            "one"
        );
    }

    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn changed_helper_refuses_to_start() {
        changed_file_refuses("bin/eitri-server");
    }
    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn changed_sidecar_refuses_to_start() {
        changed_file_refuses("sidecar");
    }

    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn older_version_start_checks_its_own_recorded_hash() {
        let root = ScratchDir::new("remote", "older");
        let (mut old, launcher) = fixture(&root, "old", false);
        let (mut new, _) = fixture(&root, "new", false);
        new.version = format!(
            "0.4.0-765abcd-{}-{}",
            &new.provenance.sha256()[..16],
            &new.sidecar_sha256()[..16]
        );
        authorize(&mut old);
        authorize(&mut new);
        install_with(&launcher, &old, &|_, _| {}).unwrap();
        install_with(&launcher, &new, &|_, _| {}).unwrap();
        let mut host = record(&old);
        host.helpers.extend(record(&new).helpers);
        host.sidecars.extend(record(&new).sidecars);
        // A server-side manifest cannot replace the hashes held by the laptop.
        std::fs::write(old.install_path().join("MANIFEST"), new.manifest.render().unwrap()).unwrap();
        start_recorded_with(&launcher, &host, &old.version, &old.sidecar.revision).unwrap();
        assert_eq!(
            std::fs::read_to_string(old.install_path().join("started")).unwrap(),
            "old"
        );
        host.helpers.insert(old.version.clone(), new.provenance.sha256().into());
        assert!(start_recorded_with(&launcher, &host, &old.version, &old.sidecar.revision).is_err());
    }

    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn consent_and_hashes_merge_without_dropping_older_installs() {
        let root = ScratchDir::new("remote", "records");
        let (mut plan, launcher) = fixture(&root, "one", false);
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        plan.record_consent(&store, "example-host", "SHA256:example").unwrap();
        assert!(!plan.data_dir.exists());
        let mut previous = store.host("SHA256:example").unwrap().unwrap();
        previous.helpers.insert("0.2.0-123abcd".into(), "a".repeat(64));
        store.save_host(&previous).unwrap();
        install_with(&launcher, &plan, &|_, _| {}).unwrap();
        plan.record_installed(&store).unwrap();
        let saved = store.host("SHA256:example").unwrap().unwrap();
        assert!(saved.consented);
        assert_eq!(saved.helpers.len(), 2);
        assert_eq!(
            saved
                .sidecars
                .get(&sidecar_record_key(&plan.version, "abcdef0"))
                .unwrap(),
            &plan.manifest.lines[1].0
        );
        let old_manifest = plan.manifest.clone();
        let mut update = plan.clone();
        update.sidecar.revision = format!("fedcba9{}", "0".repeat(33));
        update.sidecar.path = root.join("new-sidecar");
        std::fs::write(&update.sidecar.path, b"updated sidecar").unwrap();
        update = make_plan(
            &super::super::probe_step::fixture(&root),
            &plan.provenance,
            update.sidecar,
            plan.helper.clone(),
            "0.3.0-123abcd".into(),
        )
        .unwrap();
        update.use_host_record(&saved).unwrap();
        install_with(&launcher, &update, &|_, _| {}).unwrap();
        update.record_installed(&store).unwrap();
        let saved = store.host("SHA256:example").unwrap().unwrap();
        assert_eq!(saved.sidecars.len(), 2);
        assert_eq!(
            hash_file(&plan.install_path().join(&old_manifest.lines[1].1)).unwrap(),
            old_manifest.lines[1].0
        );
        assert_eq!(
            std::fs::read(update.install_path().join("MANIFEST")).unwrap(),
            update.manifest.render().unwrap()
        );
        // Running tabs retain their revision even when the installed manifest names the update.
        start_recorded_with(&launcher, &saved, &plan.version, &plan.sidecar.revision).unwrap();
    }

    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn sidecar_only_rebuilds_and_full_revision_prefix_collisions_remain_separate() {
        let root = ScratchDir::new("remote", "sidecar-identities");
        let (mut one, launcher) = fixture(&root, "one", false);
        let (two, _) = fixture(&root, "two", false);
        let probe = super::super::probe_step::fixture(&root);
        let mut rebuilt = make_plan(
            &probe,
            &one.provenance,
            two.sidecar,
            one.helper.clone(),
            "0.3.0-123abcd".into(),
        )
        .unwrap();
        let mut renamed_sidecar = one.sidecar.clone();
        renamed_sidecar.revision = format!("abcdef0{}", "1".repeat(33));
        let mut renamed = make_plan(
            &probe,
            &one.provenance,
            renamed_sidecar,
            one.helper.clone(),
            "0.3.0-123abcd".into(),
        )
        .unwrap();
        assert_ne!(one.install_path(), rebuilt.install_path());
        assert_ne!(one.install_path(), renamed.install_path());
        assert_ne!(one.sidecar_path(), renamed.sidecar_path());
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        one.record_consent(&store, "example-host", "SHA256:example").unwrap();
        for plan in [&mut rebuilt, &mut renamed] {
            plan.use_host_record(&store.host("SHA256:example").unwrap().unwrap())
                .unwrap();
        }
        for plan in [&one, &rebuilt, &renamed] {
            install_with(&launcher, plan, &|_, _| {}).unwrap();
        }
        for plan in [&renamed, &rebuilt, &one] {
            plan.record_installed(&store).unwrap();
        }
        let host = store.host("SHA256:example").unwrap().unwrap();
        for plan in [&one, &rebuilt, &renamed] {
            start_recorded_with(&launcher, &host, &plan.version, &plan.sidecar.revision).unwrap();
        }
    }

    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn reverse_recording_order_preserves_both_builds_of_one_revision() {
        let root = ScratchDir::new("remote", "record-order");
        let (mut one, launcher) = fixture(&root, "one", false);
        let (mut two, _) = fixture(&root, "two", false);
        let store = RemoteProjects::new(Some(root.as_os_str()), None);
        one.record_consent(&store, "example-host", "SHA256:example").unwrap();
        two.use_host_record(&store.host("SHA256:example").unwrap().unwrap())
            .unwrap();
        install_with(&launcher, &one, &|_, _| {}).unwrap();
        install_with(&launcher, &two, &|_, _| {}).unwrap();
        two.record_installed(&store).unwrap();
        one.record_installed(&store).unwrap();
        let saved = store.host("SHA256:example").unwrap().unwrap();
        assert_eq!(saved.helpers.len(), 2);
        assert_eq!(saved.sidecars.len(), 2);
        for plan in [&one, &two] {
            start_recorded_with(&launcher, &saved, &plan.version, &plan.sidecar.revision).unwrap();
            assert_eq!(
                std::fs::read(plan.install_path().join("MANIFEST")).unwrap(),
                Manifest::from_record(&saved, &plan.version, &plan.sidecar.revision)
                    .unwrap()
                    .render()
                    .unwrap()
            );
        }
    }

    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn publication_sweeps_only_stale_unlocked_staging_directories() {
        use std::os::fd::AsRawFd;
        let root = ScratchDir::new("remote", "sweep");
        let (mut plan, launcher) = fixture(&root, "one", false);
        authorize(&mut plan);
        std::fs::create_dir_all(&plan.data_dir).unwrap();
        let stale = plan.data_dir.join(".install-abandoned");
        let active = plan.data_dir.join(".install-active");
        let recent = plan.data_dir.join(".install-recent");
        let fresh = plan.data_dir.join(".install-fresh");
        let retained = plan.data_dir.join("older-version");
        for path in [&stale, &active, &recent, &fresh, &retained] {
            std::fs::create_dir(path).unwrap();
        }
        let times = std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH);
        for path in [&stale, &active, &retained] {
            std::fs::File::open(path).unwrap().set_times(times).unwrap();
        }
        let recent_time = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 60 * 60);
        std::fs::File::open(&recent)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(recent_time))
            .unwrap();
        let held = std::fs::File::open(&active).unwrap();
        assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_SH) }, 0);
        install_with(&launcher, &plan, &|_, _| {}).unwrap();
        assert!(!stale.exists());
        assert!(active.is_dir() && recent.is_dir() && fresh.is_dir() && retained.is_dir());
        drop(held);
        install_with(&launcher, &plan, &|_, _| {}).unwrap();
        assert!(!active.exists());
    }

    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn upload_and_manifest_steps_lock_their_staging_tree_while_writing() {
        use std::os::fd::AsRawFd;
        for (index, subdir, header, gzip) in [
            (0, "bin", "", true),
            (1, "bin", "", false),
            (2, "sidecar/abcdef0", "abcdef0\n", true),
            (3, "sidecar/abcdef0", "abcdef0\n", false),
            (4, "", "", false),
        ] {
            let root = ScratchDir::new("remote", "stage-lock");
            let (mut plan, launcher) = fixture(&root, "one", false);
            authorize(&mut plan);
            let stage = plan.data_dir.join(".install-writing");
            std::fs::create_dir_all(&stage).unwrap();
            let args = [checked_path(&plan.data_dir).unwrap(), ".install-writing"];
            let mut child = match index {
                0 => launcher.spawn_step(steps::UPLOAD, &args),
                1 => launcher.spawn_step(steps::UPLOAD_PLAIN, &args),
                2 => launcher.spawn_step(steps::UPLOAD_SIDECAR, &args),
                3 => launcher.spawn_step(steps::UPLOAD_SIDECAR_PLAIN, &args),
                _ => launcher.spawn_step(steps::WRITE_MANIFEST, &args),
            }
            .unwrap();
            child.stdin.as_mut().unwrap().write_all(header.as_bytes()).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                if std::fs::read_dir(stage.join(subdir)).is_ok_and(|mut entries| entries.next().is_some()) {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "writer did not create its temporary file"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let probe = std::fs::File::open(&stage).unwrap();
            assert_eq!(
                unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                -1
            );
            assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::EWOULDBLOCK));
            probe
                .set_times(std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))
                .unwrap();
            install_with(&launcher, &plan, &|_, _| {}).unwrap();
            assert!(stage.is_dir());
            communicate(child, &payload(b"body".to_vec(), gzip).unwrap(), &|_, _| {}).unwrap();
            assert_eq!(
                unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0
            );
        }
    }

    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn sidecar_progress_excludes_short_and_full_revision_headers() {
        for gzip in [false, true] {
            for revision in ["abcdef0".to_owned(), "abcdef0".to_owned() + &"1".repeat(33)] {
                let root = ScratchDir::new("remote", "progress");
                let (mut plan, launcher) = fixture(&root, "one", gzip);
                plan.sidecar.revision = revision;
                plan = make_plan(
                    &super::super::probe_step::fixture(&root),
                    &plan.provenance,
                    plan.sidecar,
                    plan.helper,
                    "0.3.0-123abcd".into(),
                )
                .unwrap();
                plan.gzip = gzip;
                authorize(&mut plan);
                let progress = std::cell::RefCell::new(Vec::new());
                install_with(&launcher, &plan, &|done, total| {
                    assert!(done <= total, "progress exceeds payload size: {done}/{total}");
                    progress.borrow_mut().push((done, total));
                })
                .unwrap();
                let progress = progress.into_inner();
                assert!(progress.windows(2).all(|pair| pair[0].0 <= pair[1].0));
                let (done, total) = *progress.last().unwrap();
                assert_eq!(done, total);
            }
        }
    }

    #[test]
    fn planning_refuses_insufficient_or_unknown_space_before_upload() {
        let root = ScratchDir::new("remote", "space");
        let (plan, _) = fixture(&root, "one", false);
        let required = plan.helper_bytes + plan.sidecar_bytes + 100 * 1024 * 1024;
        let mut probe = super::super::probe_step::fixture(&root);
        for free in [None, Some(required - 1)] {
            probe.free_bytes = free;
            let error = make_plan(
                &probe,
                &plan.provenance,
                plan.sidecar.clone(),
                plan.helper.clone(),
                "0.3.0-123abcd".into(),
            )
            .unwrap_err();
            assert!(error.to_string().contains("100 MiB"));
            assert!(!plan.data_dir.exists());
        }
        probe.free_bytes = Some(required);
        assert!(make_plan(
            &probe,
            &plan.provenance,
            plan.sidecar.clone(),
            plan.helper.clone(),
            "0.3.0-123abcd".into()
        )
        .is_ok());
    }

    // Masking preserves byte offsets, so nesting and line wrapping cannot hide an argument.
    fn step_calls_are_fixed(path: &str, text: &str) -> Result<usize, String> {
        let code = crate::source_scan::code_only(text);
        let squeezed: String = code.chars().filter(|c| !c.is_whitespace()).collect();
        let mut count = 0;
        for name in ["step_argv", "spawn_step"] {
            for end in crate::source_scan::word_ends(&code, name) {
                let before = code[..end - name.len()].trim_end();
                let tail = code[end..].trim_start();
                if before.split_whitespace().last() == Some("fn") {
                    continue;
                }
                if !tail.starts_with('(') {
                    let plain_use = crate::source_scan::word_ends(before, "use").last().is_some_and(|&end| {
                        before[end..]
                            .chars()
                            .all(|c| c.is_alphanumeric() || c.is_whitespace() || "_:,{}".contains(c))
                            && !before[end..].split_whitespace().any(|word| word == "as")
                            && matches!(tail.chars().next(), Some(',' | '}' | ';'))
                    });
                    if plain_use {
                        continue;
                    }
                    return Err(format!("{path}: unchecked {name} reference"));
                }
                let method = before.ends_with('.');
                // Unqualified spawn_step functions in CLI fixtures describe model script steps.
                if path == "agent/tests/codex_real_scripted.rs"
                    && name == "spawn_step"
                    && !method
                    && !before.ends_with("::")
                {
                    continue;
                }
                let open = end + code[end..].find('(').unwrap();
                let index = usize::from(name == "step_argv" || !method);
                let mut depth = 0;
                let mut from = open + 1;
                let mut args = Vec::new();
                for (offset, ch) in code[from..].char_indices() {
                    let at = open + 1 + offset;
                    match ch {
                        '(' | '[' | '{' => depth += 1,
                        ')' if depth == 0 => {
                            args.push(from..at);
                            break;
                        }
                        ')' | ']' | '}' => depth -= 1,
                        ',' if depth == 0 => {
                            args.push(from..at);
                            from = at + 1;
                        }
                        _ => {}
                    }
                }
                let range = args.get(index).ok_or_else(|| format!("{path}: missing step literal"))?;
                let argument: String = code[range.clone()].chars().filter(|c| !c.is_whitespace()).collect();
                let raw = text[range.clone()].trim();
                let inline = argument.is_empty() && (raw.starts_with('"') || raw.starts_with("r#"));
                let declared = !argument.is_empty()
                    && argument
                        .bytes()
                        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
                    && squeezed.contains(&format!("const{argument}:&str=;"));
                let step_constant = argument.rsplit_once("steps::").is_some_and(|(prefix, name)| {
                    matches!(prefix, "" | "super::" | "crate::remote_host::")
                        && !name.is_empty()
                        && name.bytes().all(|b| b.is_ascii_uppercase() || b == b'_')
                });
                let forwarded = text[open + 1..args.last().unwrap().end]
                    .split_whitespace()
                    .collect::<String>();
                let ssh_adapter = path == "agent/src/ssh/launcher.rs"
                    && name == "step_argv"
                    && argument == "literal"
                    && forwarded == "&self.master,literal,args";
                let installation_adapter = path == "core/src/remote_project/connection.rs"
                    && name == "spawn_step"
                    && before
                        .chars()
                        .filter(|c| !c.is_whitespace())
                        .collect::<String>()
                        .ends_with("self.base.")
                    && forwarded == "literal,args";
                if !(inline || declared || step_constant || ssh_adapter || installation_adapter) {
                    return Err(format!("{path}: dynamic {name} literal: {raw}"));
                }
                count += 1;
            }
        }
        Ok(count)
    }

    #[test]
    fn step_scan_accepts_only_the_installation_launchers_unchanged_forwarding() {
        let path = "core/src/remote_project/connection.rs";
        let forwarding = "fn spawn_step(&self, literal: &'static str, args: &[&str]) {\n self . base . spawn_step ( literal, args )\n}";
        assert_eq!(step_calls_are_fixed(path, forwarding).unwrap(), 1);
        for changed in [
            forwarding.replace("literal, args", "dynamic, args"),
            forwarding.replace("literal, args", "literal, other_args"),
            forwarding.replace("self . base", "self . other"),
        ] {
            assert!(step_calls_are_fixed(path, &changed).is_err(), "{changed}");
        }
        assert!(step_calls_are_fixed("other/src/lib.rs", forwarding).is_err());
    }

    #[test]
    fn step_scan_covers_ufcs_and_wrapping_and_rejects_dynamic_arguments() {
        for text in [
            "fn f(){agent::ssh::step_argv(master, dynamic(), &[]);}",
            "fn f(){ChannelLauncher::spawn_step(launcher, dynamic, &[]);}",
            "fn f(){<L as ChannelLauncher> :: spawn_step (launcher, dynamic, &[]);}",
            "fn f(){launcher . spawn_step (dynamic, &[]);}",
            "use agent::ssh::step_argv as argv; fn f(){argv(master, dynamic, &[]);}",
            "use agent::ssh::{step_argv as argv};",
            "fn f(){let f = ChannelLauncher::spawn_step; f(l, dynamic, &[]);}",
            "fn f(){let f = agent::ssh::step_argv;}",
            "fn f(){let f = launcher.spawn_step;}",
        ] {
            assert!(step_calls_are_fixed("shell/src/fixture.rs", text).is_err(), "{text}");
        }
        assert_eq!(step_calls_are_fixed("other/src/lib.rs",
            "pub use agent::ssh::step_argv; use agent::ssh::{step_argv, ChannelLauncher}; use agent::ssh::{ChannelLauncher, step_argv}; fn step_argv() {} fn spawn_step() {}").unwrap(), 0);
        assert_eq!(step_calls_are_fixed("other/src/lib.rs",
            "fn f(){ChannelLauncher :: spawn_step (launcher, steps::START, &[]); agent::ssh::step_argv(master, \"true\", &[]);}").unwrap(), 2);
    }

    #[test]
    fn steps_are_fixed_literals() {
        let workspace = crate::source_scan::read_required("Cargo.toml");
        let compact: String = workspace.text.chars().filter(|c| !c.is_whitespace()).collect();
        let members = compact.split_once("members=[").unwrap().1.split_once(']').unwrap().0;
        let dirs: Vec<_> = members.split('"').skip(1).step_by(2).collect();
        let sources = crate::source_scan::rust_sources(&dirs);
        let count: usize = sources
            .iter()
            .map(|source| step_calls_are_fixed(&source.path, &source.text).unwrap())
            .sum();
        assert!(count >= 15);
        let steps = crate::source_scan::read_required("core/src/remote_host/steps.rs");
        let declarations: String = crate::source_scan::code_only(&steps.text)
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        for declaration in declarations.split(';').filter(|item| !item.is_empty()) {
            assert!(
                declaration.starts_with("pubconst") && declaration.ends_with(":&str="),
                "bootstrap steps must be string literals: {declaration}"
            );
        }
        for literal in steps.text.split("r#\"").skip(1) {
            let literal = literal.split_once("\"#").unwrap().0;
            assert!(
                !literal.contains(['\n', '\r', '\'']),
                "step must fit one quoted login-shell line"
            );
        }
    }
    // Runs the Linux server scripts here as the server; macOS lacks flock(1) and GNU stat.
    #[cfg(target_os = "linux")]
    #[test]
    fn starting_existing_files_needs_no_upload_space_and_only_missing_files_reinstall() {
        let root = ScratchDir::new("remote", "start-no-space");
        let (mut plan, launcher) = fixture(&root, "one", false);
        authorize(&mut plan);
        install_with(&launcher, &plan, &|_, _| {}).unwrap();
        let mut probe = super::super::probe_step::fixture(&root);
        probe.free_bytes = Some(0);
        let mut squeezed = make_connection_plan(
            &probe,
            &plan.provenance,
            plan.sidecar.clone(),
            plan.helper.clone(),
            "0.3.0-123abcd".into(),
        )
        .unwrap();
        authorize(&mut squeezed);
        ensure_started_with(&launcher, &squeezed).unwrap();
        assert!(install_with(&launcher, &squeezed, &|_, _| {})
            .unwrap_err()
            .to_string()
            .contains("100 MiB"));
        std::fs::write(plan.install_path().join("ensure-fails"), "").unwrap();
        let error = ensure_started_with(&launcher, &plan).unwrap_err();
        assert!(matches!(error, InstallError::Command(_)), "{error}");
        std::fs::remove_file(plan.install_path().join("bin/eitri-server")).unwrap();
        assert!(matches!(
            ensure_started_with(&launcher, &plan),
            Err(InstallError::Reinstall(_))
        ));
    }
}
