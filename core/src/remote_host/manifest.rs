use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::Path;

use super::install::InstallError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub lines: Vec<(String, String)>,
}

impl Manifest {
    pub fn render(&self) -> Result<Vec<u8>, InstallError> {
        let mut lines = self.lines.clone();
        lines.sort_by(|a, b| a.1.cmp(&b.1));
        let mut out = Vec::new();
        let mut previous = None;
        for (hash, path) in &lines {
            if !valid_hash(hash)
                || path.starts_with('/')
                || path.split('/').any(|part| !super::install::safe_name(part))
                || previous == Some(path)
            {
                return Err(InstallError::Invalid("invalid manifest entry".into()));
            }
            writeln!(out, "{hash}  {path}")?;
            previous = Some(path);
        }
        if out.is_empty() {
            return Err(InstallError::Invalid("empty manifest".into()));
        }
        Ok(out)
    }

    /// Older helpers are checked against the laptop's saved hashes, never the server's manifest.
    pub fn from_record(
        host: &crate::remote_project::RemoteHost,
        version: &str,
        revision: &str,
    ) -> Result<Self, InstallError> {
        if !super::install::safe_name(version) || !super::install::valid_revision(revision) {
            return Err(InstallError::Invalid("invalid installed version or revision".into()));
        }
        let helper = host
            .helpers
            .get(version)
            .ok_or_else(|| InstallError::Invalid("no recorded helper hash".into()))?;
        let sidecar = host
            .sidecars
            .get(&super::install::sidecar_record_key(version, revision))
            .ok_or_else(|| InstallError::Invalid("no recorded sidecar hash".into()))?;
        if !valid_hash(helper) || !valid_hash(sidecar) {
            return Err(InstallError::Invalid("invalid recorded artifact hash".into()));
        }
        let manifest = Self {
            lines: vec![
                (helper.clone(), "bin/eitri-server".into()),
                (
                    sidecar.clone(),
                    format!(
                        "sidecar/{}/verdandi-claude-sidecar",
                        super::install::sidecar_directory(revision, sidecar)
                    ),
                ),
            ],
        };
        manifest.render()?;
        Ok(manifest)
    }
}

pub(crate) fn valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(crate) fn hash_file(path: &Path) -> Result<String, InstallError> {
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut bytes = [0; 64 * 1024];
    loop {
        let n = file.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        hash.update(&bytes[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
