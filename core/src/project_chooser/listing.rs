//! Folder facts shared by the chooser and its reader. Paths stay separate from display text.

use std::cmp::Ordering;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

pub const READ_DEBOUNCE: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    Dir { mount_point: bool },
    Link { target: String, kind: LinkKind },
    File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    Unknown,
    Dir,
    File,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub raw: OsString,
    pub name: String,
    pub kind: EntryKind,
    /// Resolved physical path, used for mount queries and recent-project matching.
    pub physical: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listing {
    Read {
        /// The resolved physical folder; the request and its envelope retain the walked path.
        dir: PathBuf,
        entries: Vec<Entry>,
        capped: Option<Cap>,
        exact: Option<Entry>,
    },
    Denied {
        exact: Option<Entry>,
    },
    Missing,
    NotAFolder,
    Loop,
    Stuck,
    TooManyStuck,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cap {
    Folders,
    Scanned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Project,
    Action,
    Here,
    Folder,
    File,
    Link,
    Mount,
}

/// Compare names without case, treating each run of ASCII digits as a number without an integer size limit.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let a = a.to_lowercase();
    let b = b.to_lowercase();
    let mut a = a.chars().peekable();
    let mut b = b.chars().peekable();
    loop {
        match (a.peek(), b.peek()) {
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let mut x = String::new();
                let mut y = String::new();
                while let Some(c) = a.next_if(|c| c.is_ascii_digit()) {
                    x.push(c);
                }
                while let Some(c) = b.next_if(|c| c.is_ascii_digit()) {
                    y.push(c);
                }
                let x = x.trim_start_matches('0');
                let y = y.trim_start_matches('0');
                let order = x.len().cmp(&y.len()).then_with(|| x.cmp(y));
                if order != Ordering::Equal {
                    return order;
                }
            }
            _ => match (a.next(), b.next()) {
                (Some(x), Some(y)) if x == y => {}
                (Some(x), Some(y)) => return x.cmp(&y),
                (None, None) => return Ordering::Equal,
                (None, Some(_)) => return Ordering::Less,
                (Some(_), None) => return Ordering::Greater,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_cmp_puts_v2_before_v10_and_ignores_case() {
        assert_eq!(natural_cmp("v2", "v10"), std::cmp::Ordering::Less);
        assert_eq!(natural_cmp("V2", "v2"), std::cmp::Ordering::Equal);
        assert_eq!(natural_cmp("Ä2", "ä10"), std::cmp::Ordering::Less);
        assert_eq!(natural_cmp("v2a", "v2b"), std::cmp::Ordering::Less);
        assert_eq!(natural_cmp("v2.9", "v2.10"), std::cmp::Ordering::Less);
        assert_eq!(
            natural_cmp("v12345678901234567890", "v9999999999999999999"),
            std::cmp::Ordering::Greater
        );
    }
}
