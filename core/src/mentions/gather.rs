//! Asking the editor for what a message names, without waiting for it.
//!
//! [`Gather::start`] sends one chunk per source; [`Gather::poll`] is called from the panel's pass, tries each answer, and
//! once every one is in hands them to a worker that reads the CLI's settings, judges every file and writes the sections.
//! Nothing blocks the GTK thread: a chunk's answer is a `Pending` that is only ever tried, the disk is looked at on the
//! worker, and a reply that does not come inside the editor's budget is a section that says it did not.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Instant;

use agent::read_deny::ReadRules;

use super::blocks::{self, Scope};
use super::Kind;
use crate::editor_rpc::EditorRpc;
use crate::editor_tools::stamp::{Sequencer, Stamp, EDITOR_BUDGET, STAMP_MARGIN};
use crate::editor_tools::tools::diagnostics::{self, DiagnosticsAnswer, DiagnosticsArgs, Severity};
use crate::editor_tools::tools::mention_reads::{self, BuffersAnswer, QuickfixAnswer};
use crate::editor_tools::words;
use crate::nvim_rpc::{Pending, RpcError};

/// The most diagnostics the editor is asked for: the host tells fewer, and the ones past the bound are counted.
pub const DIAGNOSTICS_ASKED_MAX: u32 = 2000;

/// What one source's read has come to.
enum Read {
    Buffers(Pending, Option<Result<BuffersAnswer, String>>),
    Quickfix(Pending, Option<Result<QuickfixAnswer, String>>),
    Diagnostics(Pending, Option<Result<DiagnosticsAnswer, String>>),
    /// Could not be sent: no editor.
    Failed(Kind, String),
}

/// A source's answer, owned, for the worker.
enum Answer {
    Buffers(Result<BuffersAnswer, String>),
    Quickfix(Result<QuickfixAnswer, String>),
    Diagnostics(Result<DiagnosticsAnswer, String>),
    Failed(String),
}

impl Read {
    fn kind(&self) -> Kind {
        match self {
            Read::Buffers(..) => Kind::Buffers,
            Read::Quickfix(..) => Kind::Quickfix,
            Read::Diagnostics(..) => Kind::Diagnostics,
            Read::Failed(kind, _) => *kind,
        }
    }

    fn arrived(&self) -> bool {
        match self {
            Read::Buffers(_, answer) => answer.is_some(),
            Read::Quickfix(_, answer) => answer.is_some(),
            Read::Diagnostics(_, answer) => answer.is_some(),
            Read::Failed(..) => true,
        }
    }

    /// Takes the reply if it has come, or gives up on it at `give_up`.
    fn try_take(&mut self, give_up: bool) {
        fn sentence(error: RpcError) -> String {
            match error {
                RpcError::Unavailable(why) => why,
                other => format!("the editor could not be asked ({other})"),
            }
        }
        fn settle<T>(
            pending: &Pending,
            slot: &mut Option<Result<T, String>>,
            parse: impl Fn(&rmpv::Value) -> Result<T, String>,
            give_up: bool,
        ) {
            if slot.is_some() {
                return;
            }
            match pending.try_take() {
                Some(Ok(value)) => *slot = Some(parse(&value)),
                Some(Err(error)) => *slot = Some(Err(sentence(error))),
                None if give_up => *slot = Some(Err(words::NOT_READ.to_owned())),
                None => {}
            }
        }
        match self {
            Read::Buffers(pending, slot) => settle(pending, slot, mention_reads::parse_buffers, give_up),
            Read::Quickfix(pending, slot) => settle(pending, slot, mention_reads::parse_quickfix, give_up),
            Read::Diagnostics(pending, slot) => settle(pending, slot, diagnostics::parse, give_up),
            Read::Failed(..) => {}
        }
    }

    fn into_answer(self) -> (Kind, Answer) {
        let kind = self.kind();
        let answer = match self {
            Read::Buffers(_, answer) => Answer::Buffers(answer.unwrap_or_else(|| Err(words::NOT_READ.to_owned()))),
            Read::Quickfix(_, answer) => Answer::Quickfix(answer.unwrap_or_else(|| Err(words::NOT_READ.to_owned()))),
            Read::Diagnostics(_, answer) => {
                Answer::Diagnostics(answer.unwrap_or_else(|| Err(words::NOT_READ.to_owned())))
            }
            Read::Failed(_, why) => Answer::Failed(why),
        };
        (kind, answer)
    }
}

/// The sections a message's names came to, in the order the person wrote them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gathered {
    pub sections: Vec<(Kind, String)>,
}

impl Gathered {
    /// The turn's text with the sections after it.
    pub fn appended_to(&self, text: &str) -> String {
        let mut out = text.to_owned();
        for (_, section) in &self.sections {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(section);
        }
        out
    }

    /// Every name answering that its section could not be made.
    fn unavailable(kinds: &[Kind], why: &str) -> Gathered {
        Gathered {
            sections: kinds
                .iter()
                .map(|&kind| (kind, blocks::unavailable(kind, why)))
                .collect(),
        }
    }
}

/// The sections for `answers`. Looks at the disk: the settings, and every name a buffer or an entry gives. A worker's
/// work.
fn build(answers: Vec<(Kind, Answer)>, root: &Path, home: Option<&Path>, config_dir: Option<&Path>) -> Gathered {
    let rules = ReadRules::load(root, config_dir, home);
    let scope = Scope::new(root, home, &rules);
    let sections = answers
        .into_iter()
        .map(|(kind, answer)| {
            let section = match answer {
                Answer::Failed(why)
                | Answer::Buffers(Err(why))
                | Answer::Quickfix(Err(why))
                | Answer::Diagnostics(Err(why)) => blocks::unavailable(kind, &why),
                Answer::Buffers(Ok(answer)) => blocks::buffers(&answer, &scope),
                Answer::Quickfix(Ok(answer)) => blocks::quickfix(&answer, &scope),
                Answer::Diagnostics(Ok(answer)) => blocks::diagnostics(&answer, &scope),
            };
            (kind, section)
        })
        .collect();
    Gathered { sections }
}

/// One message's open reads.
pub struct Gather {
    kinds: Vec<Kind>,
    reads: Vec<Read>,
    root: PathBuf,
    home: Option<PathBuf>,
    config_dir: Option<PathBuf>,
    give_up_at: Instant,
    /// The worker that is writing the sections, once every answer is in, and when it is given up on.
    building: Option<(Receiver<Gathered>, Instant)>,
}

impl Gather {
    /// Sends the reads for `kinds`. With no editor to ask (`rpc` is `None`) every source answers that it could not be
    /// read, and the message goes out saying so.
    pub fn start(
        rpc: Option<&dyn EditorRpc>,
        sequencer: &mut Sequencer,
        kinds: &[Kind],
        root: &Path,
        home: Option<&Path>,
        config_dir: Option<PathBuf>,
        now: Instant,
    ) -> Gather {
        let reads = kinds
            .iter()
            .map(|&kind| match rpc {
                None => Read::Failed(kind, words::NO_EDITOR.to_owned()),
                Some(rpc) => {
                    let stamp: Stamp = sequencer.stamp(EDITOR_BUDGET - STAMP_MARGIN);
                    match kind {
                        Kind::Buffers => Read::Buffers(mention_reads::send_buffers(rpc, &stamp, root), None),
                        Kind::Quickfix => Read::Quickfix(mention_reads::send_quickfix(rpc, &stamp), None),
                        Kind::Diagnostics => Read::Diagnostics(
                            diagnostics::send(
                                rpc,
                                &stamp,
                                &DiagnosticsArgs {
                                    root: root.to_path_buf(),
                                    path: None,
                                    min_severity: Severity::Hint,
                                    utf16: false,
                                    limit: DIAGNOSTICS_ASKED_MAX,
                                },
                            ),
                            None,
                        ),
                    }
                }
            })
            .collect();
        Gather {
            kinds: kinds.to_vec(),
            reads,
            root: root.to_path_buf(),
            home: home.map(Path::to_path_buf),
            config_dir,
            give_up_at: now + EDITOR_BUDGET,
            building: None,
        }
    }

    /// The sections once every read has come (or the editor's budget has run out) and the worker has written them,
    /// `None` while something is still out. Call it again after `None`; it never blocks.
    pub fn poll(&mut self, now: Instant) -> Option<Gathered> {
        if self.kinds.is_empty() {
            return Some(Gathered { sections: Vec::new() });
        }
        if let Some((sections, give_up)) = &self.building {
            return match sections.try_recv() {
                Ok(done) => Some(done),
                Err(TryRecvError::Empty) if now < *give_up => None,
                Err(TryRecvError::Empty) => {
                    Some(Gathered::unavailable(&self.kinds, "the settings were not read in time"))
                }
                Err(TryRecvError::Disconnected) => {
                    Some(Gathered::unavailable(&self.kinds, "the sections could not be made"))
                }
            };
        }
        let give_up = now >= self.give_up_at;
        for read in &mut self.reads {
            read.try_take(give_up);
        }
        if !self.reads.iter().all(Read::arrived) {
            return None;
        }
        let answers: Vec<(Kind, Answer)> = std::mem::take(&mut self.reads)
            .into_iter()
            .map(Read::into_answer)
            .collect();
        let (root, home, config_dir) = (self.root.clone(), self.home.clone(), self.config_dir.clone());
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("mention-sections".into())
            .spawn(move || {
                let _ = tx.send(build(answers, &root, home.as_deref(), config_dir.as_deref()));
            });
        if spawned.is_err() {
            return Some(Gathered::unavailable(&self.kinds, "a worker could not be started"));
        }
        self.building = Some((rx, now + EDITOR_BUDGET));
        None
    }
}

#[cfg(test)]
mod tests;
