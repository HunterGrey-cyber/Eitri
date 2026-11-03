//! Pure path expansion and lexical tidying against explicit roots.

use std::path::{Path, PathBuf};

/// Expand against explicit roots without consulting the process's current directory.
pub fn expand(text: &str, base: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let path = if text == "~" {
        home?.to_path_buf()
    } else if let Some(rest) = text.strip_prefix("~/") {
        // Repeated separators after the tilde must not replace the home root.
        home?.join(rest.trim_start_matches('/'))
    } else if text.starts_with('~') {
        return None;
    } else {
        base.join(text)
    };
    path.is_absolute().then_some(path)
}

/// Drop `.` components and a trailing separator; `..` is kept because a link before it makes it mean something else.
pub fn tidy(path: &Path) -> PathBuf {
    path.components().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expands_against_home_and_tilde_user_does_not() {
        let base = Path::new("/project");
        let home = Some(Path::new("/home/person"));
        assert_eq!(expand("~", base, home), Some(PathBuf::from("/home/person")));
        assert_eq!(expand("~/", base, home), Some(PathBuf::from("/home/person/")));
        assert_eq!(
            expand("~/src/app", base, home),
            Some(PathBuf::from("/home/person/src/app"))
        );
        assert_eq!(expand("~person/src", base, home), None);
    }

    #[test]
    fn relative_forms_are_against_base_never_the_cwd() {
        let base = Path::new("/elsewhere/project");
        assert_eq!(expand("./src", base, None), Some(base.join("./src")));
        assert_eq!(expand("../sibling", base, None), Some(base.join("../sibling")));
        assert_eq!(expand("/absolute", base, None), Some(PathBuf::from("/absolute")));
    }

    #[test]
    fn no_home_leaves_tilde_unexpanded() {
        assert_eq!(expand("~", Path::new("/base"), None), None);
        assert_eq!(expand("~/src", Path::new("/base"), None), None);
    }

    #[test]
    fn tidying_drops_dots_and_separators_but_keeps_parent_components() {
        assert_eq!(tidy(Path::new("/base/./child//")), PathBuf::from("/base/child"));
        assert_eq!(
            tidy(Path::new("/base/link/../child")),
            PathBuf::from("/base/link/../child")
        );
    }
}
