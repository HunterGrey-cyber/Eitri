use std::path::Path;

use super::install::InstallError;

/// Set by the release container after it builds the helper.
pub const RELEASE_HELPER_SHA256: Option<&str> = option_env!("EITRI_SERVER_SHA256");

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HelperProvenance {
    Release { sha256: String },
    Unsigned { sha256: String },
}

impl HelperProvenance {
    pub fn sha256(&self) -> &str {
        match self {
            Self::Release { sha256 } | Self::Unsigned { sha256 } => sha256,
        }
    }

    pub fn band_label(&self) -> &str {
        match self {
            Self::Release { .. } => "release helper",
            Self::Unsigned { .. } => "unsigned helper",
        }
    }
}

pub fn helper_provenance(exe_dir: &Path) -> Result<HelperProvenance, InstallError> {
    provenance_with_constant(exe_dir, RELEASE_HELPER_SHA256)
}

fn provenance_with_constant(exe_dir: &Path, expected: Option<&str>) -> Result<HelperProvenance, InstallError> {
    let sha256 = super::manifest::hash_file(&exe_dir.join("eitri-server"))?;
    match expected {
        Some(expected) if expected != sha256 => Err(InstallError::HelperMismatch),
        Some(_) => Ok(HelperProvenance::Release { sha256 }),
        None => Ok(HelperProvenance::Unsigned { sha256 }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_build_compares_the_helper_to_its_constant() {
        let dir = crate::test_scratch_dir::ScratchDir::new("remote", "provenance");
        let path = dir.as_ref().join("eitri-server");
        std::fs::write(&path, b"helper").unwrap();
        let hash = super::super::manifest::hash_file(&path).unwrap();
        assert!(matches!(
            provenance_with_constant(dir.as_ref(), Some(&hash)),
            Ok(HelperProvenance::Release { .. })
        ));
        std::fs::write(path, b"changed").unwrap();
        assert!(matches!(
            provenance_with_constant(dir.as_ref(), Some(&hash)),
            Err(InstallError::HelperMismatch)
        ));
    }

    #[test]
    fn no_constant_is_unsigned_and_says_so() {
        let dir = crate::test_scratch_dir::ScratchDir::new("remote", "provenance");
        std::fs::write(dir.as_ref().join("eitri-server"), b"helper").unwrap();
        let provenance = provenance_with_constant(dir.as_ref(), None).unwrap();
        assert!(matches!(provenance, HelperProvenance::Unsigned { .. }));
        assert_eq!(provenance.band_label(), "unsigned helper");
    }
}
