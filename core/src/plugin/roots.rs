//! Where a page may point the host: the plugin's declared roots, held canonical from load, and the checks
//! a page-supplied path passes before anything opens it. A path is refused before the disk is touched when
//! it is relative or holds a control character; otherwise it is canonicalised, so `..` and every symlink
//! are resolved, and the result must hold no control character either and lie inside a root.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Roots(BTreeMap<String, PathBuf>);

impl Roots {
    /// `map`'s paths must already be canonical.
    pub fn new(map: BTreeMap<String, PathBuf>) -> Roots {
        Roots(map)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &PathBuf)> {
        self.0.iter()
    }

    /// Whether a canonical path is a root or below one, compared by component: `/work2` is not in `/work`.
    pub fn contains(&self, canonical: &Path) -> bool {
        self.0.values().any(|root| canonical.starts_with(root))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathRefusal {
    NotAbsolute,
    ControlCharacter,
    Missing,
    OutsideRoots,
    NotRegularFile,
    NotDirectory,
}

impl PathRefusal {
    /// What the page is told. Control characters are shown escaped.
    pub fn message(&self, raw: &str) -> String {
        let shown = raw.escape_debug();
        match self {
            PathRefusal::NotAbsolute => format!("\"{shown}\" is not an absolute path"),
            PathRefusal::ControlCharacter => format!("\"{shown}\" holds a control character"),
            PathRefusal::Missing => format!("\"{shown}\" does not exist"),
            PathRefusal::OutsideRoots => format!("\"{shown}\" is outside every declared root"),
            PathRefusal::NotRegularFile => format!("\"{shown}\" is not a regular file"),
            PathRefusal::NotDirectory => format!("\"{shown}\" is not a directory"),
        }
    }
}

/// U+0000-U+001F and U+007F: the characters that let a name end one editor command and start another.
pub fn has_control(text: &str) -> bool {
    text.chars().any(|c| c <= '\u{1f}' || c == '\u{7f}')
}

fn canonical_inside(roots: &Roots, raw: &str) -> Result<(PathBuf, std::fs::Metadata), PathRefusal> {
    if has_control(raw) {
        return Err(PathRefusal::ControlCharacter);
    }
    if !Path::new(raw).is_absolute() {
        return Err(PathRefusal::NotAbsolute);
    }
    let canonical = std::fs::canonicalize(raw).map_err(|_| PathRefusal::Missing)?;
    // A link inside a root can lead to a file whose own name holds a control character.
    if has_control(&canonical.to_string_lossy()) {
        return Err(PathRefusal::ControlCharacter);
    }
    if !roots.contains(&canonical) {
        return Err(PathRefusal::OutsideRoots);
    }
    let meta = std::fs::metadata(&canonical).map_err(|_| PathRefusal::Missing)?;
    Ok((canonical, meta))
}

/// For `editor.open`: a regular file inside a root, canonical.
pub fn file_inside(roots: &Roots, raw: &str) -> Result<PathBuf, PathRefusal> {
    let (path, meta) = canonical_inside(roots, raw)?;
    if meta.is_file() {
        Ok(path)
    } else {
        Err(PathRefusal::NotRegularFile)
    }
}

/// For `editor.assist` and a task's working directory: a directory inside a root, canonical.
pub fn dir_inside(roots: &Roots, raw: &str) -> Result<PathBuf, PathRefusal> {
    let (path, meta) = canonical_inside(roots, raw)?;
    if meta.is_dir() {
        Ok(path)
    } else {
        Err(PathRefusal::NotDirectory)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::test_support::Scratch;

    fn roots_at(dir: &Path) -> Roots {
        Roots::new(BTreeMap::from([(
            "work".to_string(),
            std::fs::canonicalize(dir).unwrap(),
        )]))
    }

    #[test]
    fn a_file_inside_is_accepted_and_everything_else_refused_by_kind() {
        let s = Scratch::new("roots");
        let work = s.0.join("work");
        std::fs::create_dir_all(work.join("ex/src")).unwrap();
        std::fs::write(work.join("ex/src/lib.rs"), "").unwrap();
        std::fs::write(s.0.join("secret.txt"), "").unwrap();
        let roots = roots_at(&work);
        let ok = work.join("ex/src/lib.rs");
        assert_eq!(file_inside(&roots, ok.to_str().unwrap()), Ok(ok.clone()));
        let up = format!("{}/ex/../../secret.txt", work.display());
        assert_eq!(file_inside(&roots, &up), Err(PathRefusal::OutsideRoots));
        assert_eq!(file_inside(&roots, "ex/src/lib.rs"), Err(PathRefusal::NotAbsolute));
        assert_eq!(
            file_inside(&roots, &format!("{}\n", ok.display())),
            Err(PathRefusal::ControlCharacter)
        );
        assert_eq!(
            file_inside(&roots, &format!("{}\u{7f}", ok.display())),
            Err(PathRefusal::ControlCharacter)
        );
        assert_eq!(
            file_inside(&roots, work.join("ex").to_str().unwrap()),
            Err(PathRefusal::NotRegularFile)
        );
        assert_eq!(
            file_inside(&roots, work.join("nope").to_str().unwrap()),
            Err(PathRefusal::Missing)
        );
        assert_eq!(dir_inside(&roots, ok.to_str().unwrap()), Err(PathRefusal::NotDirectory));
        assert_eq!(
            dir_inside(&roots, work.join("ex").to_str().unwrap()),
            Ok(work.join("ex"))
        );
        let fifo = work.join("fifo");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: a valid NUL-terminated path; mkfifo touches nothing else.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert_eq!(
            file_inside(&roots, fifo.to_str().unwrap()),
            Err(PathRefusal::NotRegularFile)
        );
    }

    #[test]
    fn a_symlinked_root_matches_and_a_link_out_of_it_does_not() {
        let s = Scratch::new("symroot");
        let real = s.0.join("real-work");
        std::fs::create_dir_all(real.join("ex")).unwrap();
        std::fs::write(real.join("ex/main.rs"), "").unwrap();
        std::fs::write(s.0.join("outside.rs"), "").unwrap();
        std::os::unix::fs::symlink(&real, s.0.join("work")).unwrap();
        std::os::unix::fs::symlink(s.0.join("outside.rs"), real.join("ex/out.rs")).unwrap();
        // The root as the user wrote it, through the link; Roots holds it canonical.
        let roots = roots_at(&s.0.join("work"));
        let through_link = s.0.join("work/ex/main.rs");
        assert_eq!(
            file_inside(&roots, through_link.to_str().unwrap()),
            Ok(real.join("ex/main.rs"))
        );
        let out = s.0.join("work/ex/out.rs");
        assert_eq!(
            file_inside(&roots, out.to_str().unwrap()),
            Err(PathRefusal::OutsideRoots)
        );
    }

    #[test]
    fn a_sibling_with_a_common_prefix_is_not_inside() {
        let s = Scratch::new("prefix");
        std::fs::create_dir_all(s.0.join("work")).unwrap();
        std::fs::create_dir_all(s.0.join("work2")).unwrap();
        std::fs::write(s.0.join("work2/a.rs"), "").unwrap();
        let roots = roots_at(&s.0.join("work"));
        assert_eq!(
            file_inside(&roots, s.0.join("work2/a.rs").to_str().unwrap()),
            Err(PathRefusal::OutsideRoots)
        );
    }

    #[test]
    fn a_link_to_a_name_with_a_control_character_is_refused() {
        let s = Scratch::new("ctlink");
        let work = s.0.join("work");
        std::fs::create_dir_all(work.join("odd\ndir")).unwrap();
        std::fs::write(work.join("odd\nfile.rs"), "").unwrap();
        std::os::unix::fs::symlink(work.join("odd\nfile.rs"), work.join("safe.rs")).unwrap();
        std::os::unix::fs::symlink(work.join("odd\ndir"), work.join("safe-dir")).unwrap();
        let roots = roots_at(&work);
        assert_eq!(
            file_inside(&roots, work.join("safe.rs").to_str().unwrap()),
            Err(PathRefusal::ControlCharacter)
        );
        assert_eq!(
            dir_inside(&roots, work.join("safe-dir").to_str().unwrap()),
            Err(PathRefusal::ControlCharacter)
        );
    }
}
