//! A scheduler's book of the sources it has armed, one per token of the window contract: the bookkeeping that is the
//! same whatever toolkit's sources it holds (GLib's in the GTK shell).
//!
//! The book only remembers; its user does the removing, in this order: cancel a token's source (`take`, then remove
//! it) before arming the token again, and take a one-shot source out when it fires, before its event is posted, so a
//! sink that arms the same token again from inside its handler arms a fresh source and a later cancel never removes
//! a source the toolkit has already destroyed (which GLib reports as a critical warning). `arm` gives back a source
//! it replaced, which is then still armed in the toolkit and the caller's to remove.

use std::collections::HashMap;
use std::hash::Hash;

/// The armed sources `S` by their token `T`, at most one each.
pub struct SourceBook<T, S> {
    armed: HashMap<T, S>,
}

impl<T: Eq + Hash + Copy, S> SourceBook<T, S> {
    pub fn new() -> Self {
        SourceBook { armed: HashMap::new() }
    }

    /// Notes `source` as `token`'s. The source it replaces, if any, is given back for the caller to remove; a
    /// caller that cancels first gets none.
    pub fn arm(&mut self, token: T, source: S) -> Option<S> {
        self.armed.insert(token, source)
    }

    /// Forgets `token`'s source and gives it back: on a cancel the caller removes it, on a fire the toolkit
    /// destroys it.
    pub fn take(&mut self, token: T) -> Option<S> {
        self.armed.remove(&token)
    }

    /// Every source still armed, forgotten, for the caller to remove.
    pub fn drain(&mut self) -> impl Iterator<Item = S> + '_ {
        self.armed.drain().map(|(_, source)| source)
    }
}

#[cfg(test)]
mod tests {
    use super::SourceBook;

    #[test]
    fn a_token_holds_one_source_and_a_taken_one_is_gone() {
        let mut book = SourceBook::new();
        assert_eq!(book.arm(1, "first"), None);
        assert_eq!(
            book.arm(1, "second"),
            Some("first"),
            "the replaced source is the caller's to remove"
        );
        assert_eq!(book.arm(2, "other"), None);
        assert_eq!(book.take(1), Some("second"));
        assert_eq!(book.take(1), None, "a fired or cancelled source is never removed twice");
        let mut left: Vec<_> = book.drain().collect();
        left.sort_unstable();
        assert_eq!(left, ["other"]);
        assert_eq!(book.take(2), None, "drained sources are forgotten");
    }
}
