//! Tracks paths named by turns with intersecting running intervals, without consulting the clock or disk.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

const MAX_PATHS: usize = 1024;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct TurnKey {
    pub session: String,
    pub turn_id: String,
}

#[derive(Clone, Copy)]
struct Interval {
    started_ns: u64,
    ended_ns: Option<u64>,
}

impl Interval {
    fn is_empty(self) -> bool {
        self.ended_ns.is_some_and(|end| end <= self.started_ns)
    }

    fn intersects(self, other: Self) -> bool {
        !self.is_empty()
            && !other.is_empty()
            && self.ended_ns.is_none_or(|end| other.started_ns < end)
            && other.ended_ns.is_none_or(|end| self.started_ns < end)
    }
}

struct PathEntry {
    path: PathBuf,
    keys: HashSet<TurnKey>,
    contestants: HashSet<TurnKey>,
}

impl PathEntry {
    fn refresh(&mut self, turns: &HashMap<TurnKey, Interval>) {
        self.contestants.clear();
        let mut ordered: Vec<_> = self.keys.iter().map(|key| (key, turns[key])).collect();
        ordered.sort_unstable_by_key(|(_, interval)| interval.started_ns);
        let mut farthest: Option<(&TurnKey, Interval)> = None;
        for (key, interval) in ordered {
            if interval.is_empty() {
                continue;
            }
            if let Some((previous_key, previous)) = farthest {
                if previous.intersects(interval) {
                    self.contestants.insert(previous_key.clone());
                    self.contestants.insert(key.clone());
                }
                if previous
                    .ended_ns
                    .is_some_and(|end| interval.ended_ns.is_none_or(|next| next > end))
                {
                    farthest = Some((key, interval));
                }
            } else {
                farthest = Some((key, interval));
            }
        }
    }
}

/// Remembers contests until a disjoint turn names the path or capacity evicts it.
#[derive(Default)]
pub struct LiveContest {
    // Insertion order makes eviction deterministic without a clock or an overflowing counter.
    paths: VecDeque<PathEntry>,
    turns: HashMap<TurnKey, Interval>,
}

impl LiveContest {
    pub fn new() -> LiveContest {
        Self::default()
    }

    /// Returns true only when this note changes the path from uncontested to contested.
    pub fn note_edit(&mut self, key: &TurnKey, started_ns: u64, path: &Path) -> bool {
        let interval = self.turns.get(key).copied().unwrap_or(Interval {
            started_ns,
            ended_ns: None,
        });
        let index = match self.paths.iter().position(|entry| entry.path == path) {
            Some(index) => index,
            None => {
                if self.paths.len() == MAX_PATHS {
                    let oldest = self
                        .paths
                        .iter()
                        .position(|entry| entry.keys.iter().all(|key| self.turns[key].ended_ns.is_some()))
                        .unwrap_or(0);
                    self.paths.remove(oldest);
                    self.prune_turns();
                }
                self.paths.push_back(PathEntry {
                    path: path.to_path_buf(),
                    keys: HashSet::new(),
                    contestants: HashSet::new(),
                });
                self.paths.len() - 1
            }
        };
        self.turns.insert(key.clone(), interval);
        let entry = &mut self.paths[index];
        let was_contested = !entry.contestants.is_empty();
        let releases = was_contested
            && !entry.contestants.contains(key)
            && entry
                .contestants
                .iter()
                .all(|other| !interval.intersects(self.turns[other]));
        if releases {
            entry.keys.clear();
            entry.contestants.clear();
        }
        if entry.keys.insert(key.clone()) {
            for other in &entry.keys {
                if other != key && interval.intersects(self.turns[other]) {
                    entry.contestants.insert(other.clone());
                    entry.contestants.insert(key.clone());
                }
            }
        }
        let became_contested = !was_contested && !entry.contestants.is_empty();
        if releases {
            let entry = self.paths.remove(index).unwrap();
            self.paths.push_back(entry);
            self.prune_turns();
        }
        became_contested
    }

    pub fn note_end(&mut self, key: &TurnKey, ended_ns: u64) {
        let Some(interval) = self.turns.get_mut(key) else {
            return;
        };
        interval.ended_ns = Some(ended_ns);
        for entry in &mut self.paths {
            if entry.keys.contains(key) {
                // A delayed end can correct an overlap inferred while its interval was open.
                entry.refresh(&self.turns);
            }
        }
    }

    pub fn is_contested(&self, path: &Path) -> bool {
        self.paths
            .iter()
            .any(|entry| entry.path == path && !entry.contestants.is_empty())
    }

    pub fn contested_by(&self, key: &TurnKey, path: &Path) -> bool {
        self.paths
            .iter()
            .any(|entry| entry.path == path && entry.contestants.contains(key))
    }

    fn prune_turns(&mut self) {
        let retained: HashSet<_> = self.paths.iter().flat_map(|entry| &entry.keys).collect();
        self.turns.retain(|key, _| retained.contains(key));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(session: &str, turn_id: &str) -> TurnKey {
        TurnKey {
            session: session.into(),
            turn_id: turn_id.into(),
        }
    }

    #[test]
    fn live_contest_tracks_intervals_membership_release_and_capacity() {
        enum Step {
            Edit(usize, u64, &'static str, bool),
            End(usize, u64),
            Check(&'static str, &'static [usize]),
            Fill(bool),
            Len(usize),
        }
        use Step::*;

        let cases: &[(&str, &[Step])] = &[
            (
                "overlapping turns",
                &[
                    Edit(0, 10, "shared", false),
                    Edit(1, 20, "shared", true),
                    Check("shared", &[0, 1]),
                    Edit(0, 10, "shared", false),
                    Edit(1, 20, "shared", false),
                    End(0, 30),
                    Check("shared", &[0, 1]),
                    End(1, 40),
                    Check("shared", &[0, 1]),
                ],
            ),
            (
                "overlapping turns arriving in reverse order",
                &[
                    Edit(1, 20, "shared", false),
                    Edit(0, 10, "shared", true),
                    Check("shared", &[0, 1]),
                ],
            ),
            (
                "different paths",
                &[
                    Edit(0, 10, "first", false),
                    Edit(1, 20, "second", false),
                    Check("first", &[]),
                    Check("second", &[]),
                    Check("missing", &[]),
                ],
            ),
            (
                "turn starting after the earlier turn ended",
                &[
                    Edit(0, 10, "shared", false),
                    End(0, 20),
                    Edit(1, 30, "shared", false),
                    Check("shared", &[]),
                ],
            ),
            (
                "touching intervals do not intersect",
                &[
                    Edit(0, 10, "shared", false),
                    End(0, 20),
                    Edit(1, 20, "shared", false),
                    Check("shared", &[]),
                ],
            ),
            (
                "an edit arriving after the earlier turn ended still overlaps",
                &[
                    Edit(0, 10, "shared", false),
                    End(0, 30),
                    Edit(1, 20, "shared", true),
                    Check("shared", &[0, 1]),
                ],
            ),
            (
                "a third turn after both contestants ended releases the path",
                &[
                    Edit(0, 10, "shared", false),
                    Edit(1, 20, "shared", true),
                    End(0, 30),
                    End(1, 40),
                    Edit(2, 40, "shared", false),
                    Check("shared", &[]),
                ],
            ),
            (
                "a later independent turn releases the contest",
                &[
                    Edit(0, 10, "shared", false),
                    Edit(1, 20, "shared", true),
                    End(0, 30),
                    Edit(2, 40, "shared", false),
                    Check("shared", &[0, 1, 2]),
                    End(1, 50),
                    End(2, 60),
                    Edit(0, 10, "shared", false),
                    Check("shared", &[0, 1, 2]),
                    Edit(3, 60, "shared", false),
                    Check("shared", &[]),
                    Edit(4, 70, "shared", true),
                    Check("shared", &[3, 4]),
                ],
            ),
            (
                "an end applies to every path the turn named",
                &[
                    Edit(0, 10, "first", false),
                    Edit(0, 10, "second", false),
                    End(0, 20),
                    Edit(1, 20, "first", false),
                    Edit(1, 20, "second", false),
                    Check("first", &[]),
                    Check("second", &[]),
                ],
            ),
            (
                "a known end is kept when another path is named later",
                &[
                    Edit(1, 30, "shared", false),
                    Edit(0, 10, "first", false),
                    End(0, 20),
                    Edit(0, 10, "shared", false),
                    Check("shared", &[]),
                ],
            ),
            (
                "a delayed end corrects an inferred overlap",
                &[
                    Edit(0, 10, "shared", false),
                    Edit(1, 30, "shared", true),
                    End(0, 20),
                    Check("shared", &[]),
                ],
            ),
            (
                "session and turn both identify a contestant",
                &[
                    Edit(0, 10, "shared", false),
                    Edit(3, 20, "shared", true),
                    Edit(1, 30, "shared", false),
                    Check("shared", &[0, 1, 3]),
                ],
            ),
            (
                "the oldest fully ended path is evicted before a running path",
                &[
                    Edit(0, 10, "running", false),
                    Edit(1, 20, "running", true),
                    Edit(2, 30, "old-ended", false),
                    Edit(3, 40, "old-ended", true),
                    Edit(2, 30, "new-ended", false),
                    Edit(3, 40, "new-ended", true),
                    End(2, 50),
                    End(3, 60),
                    Fill(false),
                    Check("old-ended", &[2, 3]),
                    Edit(4, 70, "overflow", false),
                    Len(1024),
                    Check("running", &[0, 1]),
                    Check("old-ended", &[]),
                    Check("new-ended", &[2, 3]),
                ],
            ),
            (
                "a released path has the age of its new turn",
                &[
                    Edit(0, 10, "released", false),
                    Edit(1, 20, "released", true),
                    Edit(0, 10, "old-ended", false),
                    Edit(1, 20, "old-ended", true),
                    End(0, 30),
                    End(1, 40),
                    Edit(2, 40, "released", false),
                    Edit(3, 50, "released", true),
                    End(2, 60),
                    End(3, 70),
                    Fill(false),
                    Edit(4, 70, "overflow", false),
                    Len(1024),
                    Check("old-ended", &[]),
                    Check("released", &[2, 3]),
                ],
            ),
            (
                "the path count stays bounded when all turns are running",
                &[
                    Edit(0, 10, "old-running", false),
                    Edit(1, 20, "old-running", true),
                    Fill(true),
                    Edit(4, 70, "overflow", false),
                    Len(1024),
                    Check("old-running", &[]),
                ],
            ),
        ];

        let keys = [
            key("one", "a"),
            key("two", "b"),
            key("three", "c"),
            key("one", "b"),
            key("two", "a"),
        ];
        for (name, steps) in cases {
            let mut contest = LiveContest::new();
            for (index, step) in steps.iter().enumerate() {
                match step {
                    Edit(who, started, path, became) => assert_eq!(
                        contest.note_edit(&keys[*who], *started, Path::new(path)),
                        *became,
                        "{name}, step {index}: only a new contest is reported"
                    ),
                    End(who, ended) => contest.note_end(&keys[*who], *ended),
                    Check(path, members) => {
                        let path = Path::new(path);
                        assert_eq!(contest.is_contested(path), !members.is_empty(), "{name}, step {index}");
                        for (who, key) in keys.iter().enumerate() {
                            assert_eq!(
                                contest.contested_by(key, path),
                                members.contains(&who),
                                "{name}, step {index}, key {who}"
                            );
                        }
                    }
                    Fill(running) => {
                        for number in contest.paths.len()..1024 {
                            let path = format!("filler-{number}");
                            assert!(!contest.note_edit(&keys[4], 70, Path::new(&path)), "{name}");
                        }
                        if !running {
                            contest.note_end(&keys[4], 80);
                        }
                    }
                    Len(expected) => assert_eq!(contest.paths.len(), *expected, "{name}, step {index}"),
                }
            }
        }
    }
}
