//! The `!` lines of a window's Claude tabs: a person's own shell commands, run by Eitri
//! ([`crate::shell_line`]) with no permission gate, as the CLI runs them.
//!
//! A line waits in the tab's queue like any message and goes when the tab is idle; while it runs nothing else
//! does. A queue item is a line only if [`TabSet::queue_shell_line`] made it, which only the composer's Enter
//! reaches. Everything else that starts with `!`, from anywhere, is text.

use super::{wall_clock_ms, BackendError, Flush, Queued, TabId, TabSet};
use crate::shell_line::{self, wire, Ending};
use std::collections::HashMap;
use std::path::PathBuf;

/// How a `!` line's words are told from the rest of the box: the page's test too.
pub fn shell_command(text: &str) -> Option<&str> {
    let command = text.strip_prefix('!')?.trim();
    (!command.is_empty()).then_some(command)
}

#[derive(Default)]
pub(super) struct Shells {
    running: HashMap<u64, shell_line::Running>,
    /// Where each tab's next line starts, when it moved: a `cd` lasts for a tab's later lines.
    cwds: HashMap<u64, PathBuf>,
}

/// A line that ended, as the window's tick sees it.
pub struct ShellEnded {
    pub tab: TabId,
    /// The turn telling the agent, when one went out now. A busy agent gets it at its turn's end.
    pub flush: Option<Flush>,
    /// A line the person stopped, or that could not start: it goes back in the box.
    pub put_back: Option<String>,
}

impl TabSet {
    /// A person's Enter on a `!` line in `id`'s composer: it joins the queue, and [`TabSet::flush_queue`]
    /// starts it when nothing runs before it. Refused in another agent's tab, where `!` means nothing.
    pub fn queue_shell_line(&mut self, id: TabId, text: &str, now_ms: u64) -> Result<(), String> {
        if shell_command(text).is_none() {
            return Err("nothing to run".to_string());
        }
        if self.get(id).is_some_and(|tab| tab.agent.is_external()) {
            return Err("this agent has no shell lines".to_string());
        }
        // What is kept under the words goes to the agent as words: a line that outlives the mark, restored from
        // a saved queue, is never run.
        self.queue_message(id, text, text.to_owned(), now_ms, false, agent::PromptOrigin::Typed)?;
        let tab = self.get_mut(id).expect("queue_message found it");
        tab.session.queue.last_mut().expect("just pushed").shell_line = true;
        Ok(())
    }

    /// Whether a `!` line of `id` is running.
    pub fn shell_line_runs(&self, id: TabId) -> bool {
        self.shells.running.contains_key(&id.0)
    }

    /// Stops `id`'s running line. False when it has none. Never waits: [`TabSet::poll_shell_lines`] reports it.
    pub fn interrupt_shell_line(&mut self, id: TabId) -> bool {
        match self.shells.running.get(&id.0) {
            Some(line) => {
                line.interrupt();
                true
            }
            None => false,
        }
    }

    /// Shows the line and starts it. The queue's own path to a line, so it runs only in a live, idle tab.
    pub(super) fn start_shell_line(&mut self, id: TabId, text: &str) -> Result<(), BackendError> {
        let refused = |message: &str| BackendError {
            message: message.to_string(),
            benign: true,
        };
        let command = shell_command(text)
            .ok_or_else(|| refused("nothing to run"))?
            .to_string();
        let root = self
            .project_root
            .clone()
            .ok_or_else(|| refused("no project to run this in"))?;
        let wake = self.ui_wake.clone();
        let cwd = self.shells.cwds.get(&id.0).cloned().unwrap_or_else(|| root.clone());
        let tab = self.get_mut(id).ok_or_else(|| refused("the tab has gone"))?;
        tab.live_mut()
            .ok_or_else(|| refused("no active session"))?
            .fold_shell_line(text);
        let request = shell_line::Request {
            command,
            cwd,
            root,
            spool_dir: None,
            timeout: shell_line::TIMEOUT,
        };
        let running = shell_line::start(request, None, move || {
            if let Some(wake) = &wake {
                wake();
            }
        });
        self.shells.running.insert(id.0, running);
        Ok(())
    }

    /// The lines that ended since the last look. Run by the window's tick. A line of a tab that has gone is
    /// dropped, which kills it.
    pub fn poll_shell_lines(&mut self) -> Vec<ShellEnded> {
        let running: Vec<u64> = self.shells.running.keys().copied().collect();
        let mut ended = Vec::new();
        for raw in running {
            let id = TabId(raw);
            if self.get(id).is_none() {
                self.shells.running.remove(&raw);
                self.shells.cwds.remove(&raw);
                continue;
            }
            let Some(line) = self.shells.running.get(&raw).and_then(shell_line::Running::finished) else {
                continue;
            };
            self.shells.running.remove(&raw);
            ended.push(self.shell_line_ended(id, line));
        }
        ended
    }

    fn shell_line_ended(&mut self, id: TabId, line: shell_line::Finished) -> ShellEnded {
        match (&line.cwd, line.left_project) {
            (Some(cwd), _) => {
                self.shells.cwds.insert(id.0, cwd.clone());
            }
            (None, true) => {
                self.shells.cwds.remove(&id.0);
            }
            (None, false) => {}
        }
        let tab = self.get_mut(id).expect("the caller looked");
        if let Some(backend) = tab.live_mut() {
            backend.fold_shell_output(wire::row_text(&line));
        }
        let typed = format!("!{}", line.command);
        // A stopped line tells the model nothing and starts no turn, as in the terminal.
        if matches!(line.ending, Ending::Interrupted | Ending::NotStarted) {
            let flush = self.flush_queue(id);
            return ShellEnded {
                tab: id,
                flush,
                put_back: Some(typed),
            };
        }
        let root = self.project_root.clone();
        let wire_text = wire::wire_text(&line, root.as_deref().filter(|_| line.left_project));
        let mut report = Queued::held(&typed, wire_text, wall_clock_ms());
        report.steerable = false;
        report.shell_report = true;
        let tab = self.get_mut(id).expect("the caller looked");
        tab.session.queue.insert(0, report);
        let flush = self.flush_queue(id);
        ShellEnded {
            tab: id,
            flush,
            put_back: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::shell_command;
    use super::ShellEnded;
    use crate::test_providers::RecordingProvider;
    use std::time::{Duration, Instant};

    struct Fixture {
        dir: PathBuf,
        set: TabSet,
        tab: TabId,
        provider: Arc<RecordingProvider>,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            agent::state_dirs::redirect_state_to_a_test_root();
            let dir = agent::state_dirs::test_workspace_dir(label);
            let provider = Arc::new(RecordingProvider::steering());
            let conversation = agent::AgentConversation::create(
                provider.clone(),
                &dir,
                agent::setting_sources::ProjectTrust::Untrusted,
            )
            .unwrap();
            let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
            set.pump(&dir, true);
            let tab = set.active();
            set.get_mut(tab).unwrap().backend = TabBackend::Live(AgentBackend::Conversation {
                agent: AgentId::Claude,
                conversation: Box::new(conversation),
            });
            Self {
                dir,
                set,
                tab,
                provider,
            }
        }

        fn line(&mut self, text: &str) -> Option<Flush> {
            self.set.queue_shell_line(self.tab, text, 1).unwrap();
            self.set.flush_queue(self.tab)
        }

        fn ended(&mut self) -> ShellEnded {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                self.set.pump(&self.dir, true);
                if let Some(ended) = self.set.poll_shell_lines().pop() {
                    return ended;
                }
                assert!(Instant::now() < deadline, "the line did not end");
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        fn rows(&mut self) -> (Vec<String>, Vec<String>) {
            let backend = self.set.get(self.tab).unwrap().live().unwrap();
            let projection = backend.projection();
            (
                projection.user_prompts.iter().map(|p| p.text.clone()).collect(),
                projection.notices.iter().filter_map(|n| n.content.clone()).collect(),
            )
        }

        fn wait(&mut self, mut done: impl FnMut(&Tab) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                self.set.pump(&self.dir, true);
                if done(self.set.get(self.tab).unwrap()) {
                    return;
                }
                assert!(Instant::now() < deadline, "the tab did not reach the expected state");
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        fn start_turn(&mut self) {
            self.provider.queue(AgentDomainEvent::TurnStarted {
                turn_id: "turn-1".into(),
            });
            self.wait(Tab::turn_running);
        }

        fn end_turn(&mut self) {
            self.provider.queue(AgentDomainEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                outcome: agent::TurnOutcome::Completed,
                result_text: String::new(),
                stop_reason: None,
                usage: None,
                detail: Default::default(),
            });
            self.wait(|tab| !tab.turn_running());
        }
    }

    #[test]
    fn a_person_s_line_runs_shows_its_output_and_tells_the_agent_in_the_cli_s_words() {
        let mut f = Fixture::new("shell-line-runs");
        let flush = f.line("!echo MARK").expect("an idle tab starts it");
        assert!(flush.outcome.is_ok());
        assert!(f.set.shell_line_runs(f.tab));
        assert!(f.provider.turns().is_empty(), "no turn until it has ended");
        let ended = f.ended();
        assert!(ended.flush.expect("the report goes out").outcome.is_ok());
        assert_eq!(ended.put_back, None);
        assert!(!f.set.shell_line_runs(f.tab));
        assert_eq!(
            f.provider.turns(),
            ["<bash-input>echo MARK</bash-input>\n<bash-stdout>MARK</bash-stdout><bash-stderr></bash-stderr>"]
        );
        let stamp = &f.provider.turn_stamps()[0];
        assert!(
            !stamp.typed_by_person,
            "the CLI's own record of a line has no human origin"
        );
        assert_eq!(f.rows(), (vec!["!echo MARK".to_string()], vec!["MARK".to_string()]));
    }

    /// The one rule of this module: only `queue_shell_line` runs anything.
    #[test]
    fn words_that_start_with_a_bang_are_words_on_every_other_path() {
        let mut f = Fixture::new("shell-line-words");
        let marker = f.dir.join("ran");
        let line = format!("!touch {}", marker.display());
        for origin in [PromptOrigin::Typed, PromptOrigin::Composed] {
            f.set
                .queue_message(f.tab, &line, line.clone(), 1, true, origin)
                .unwrap();
            assert!(f.set.flush_queue(f.tab).expect("it goes out").outcome.is_ok());
            assert!(!f.set.shell_line_runs(f.tab));
            f.start_turn();
            f.end_turn();
        }
        f.start_turn();
        f.set.send_review(f.tab, &line, 1).unwrap();
        f.end_turn();
        assert!(f.set.flush_queue(f.tab).expect("it goes out").outcome.is_ok());
        assert!(!f.set.shell_line_runs(f.tab));
        assert_eq!(f.provider.turns(), [line.clone(), line.clone(), line]);
        assert!(!marker.exists());
    }

    #[test]
    fn a_restored_queue_holds_words_never_a_line_to_run() {
        let mut f = Fixture::new("shell-line-restored");
        f.start_turn();
        f.set.queue_shell_line(f.tab, "!touch ran", 1).unwrap();
        let saved = serde_json::to_string(&f.set.get(f.tab).unwrap().session.queue).unwrap();
        let restored: Vec<Queued> = serde_json::from_str(&saved).unwrap();
        assert!(!restored[0].shell_line);
        assert_eq!(restored[0].wire, "!touch ran");
    }

    #[test]
    fn during_a_turn_a_line_waits_goes_into_no_turn_and_runs_alone_at_its_end() {
        let mut f = Fixture::new("shell-line-during");
        f.start_turn();
        f.set.queue_shell_line(f.tab, "!echo MARK", 1).unwrap();
        assert!(f.set.flush_queue(f.tab).is_none());
        f.set.steer_held(f.tab, 3, Some("turn-1"));
        assert!(f.provider.steer_stamps().is_empty(), "a line never goes into a turn");
        f.set
            .queue_message(f.tab, "after", "after".into(), 2, true, PromptOrigin::Typed)
            .unwrap();
        assert!(!f.set.shell_line_runs(f.tab));
        f.end_turn();
        assert!(f.set.flush_queue(f.tab).expect("it starts").outcome.is_ok());
        assert!(f.set.shell_line_runs(f.tab));
        assert!(f.set.flush_queue(f.tab).is_none(), "nothing else goes while it runs");
        let ended = f.ended();
        assert!(ended.flush.expect("its report goes out").outcome.is_ok());
        assert_eq!(
            f.provider.turns().len(),
            1,
            "the report goes alone: {:?}",
            f.provider.turns()
        );
        f.start_turn();
        f.end_turn();
        assert!(f.set.flush_queue(f.tab).expect("what waited behind it").outcome.is_ok());
        assert_eq!(f.provider.turns()[1], "after");
    }

    #[test]
    fn a_report_waits_out_a_turn_the_cli_started_while_the_line_ran() {
        let mut f = Fixture::new("shell-line-cli-turn");
        f.line("!echo MARK; sleep 0.2").unwrap();
        f.start_turn();
        let ended = f.ended();
        assert!(ended.flush.is_none());
        assert!(f.provider.turns().is_empty());
        f.end_turn();
        assert!(f.set.flush_queue(f.tab).expect("it goes now").outcome.is_ok());
        assert_eq!(f.rows().0, ["!echo MARK; sleep 0.2"], "its row was shown once");
    }

    #[test]
    fn stopping_a_line_starts_no_turn_and_puts_it_back() {
        let mut f = Fixture::new("shell-line-stop");
        let began = f.dir.join("began");
        let line = format!("!echo PART; touch {}; sleep 20", began.display());
        f.line(&line).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !began.exists() {
            assert!(Instant::now() < deadline, "the line never began");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(f.set.interrupt_shell_line(f.tab));
        let ended = f.ended();
        assert_eq!(ended.put_back.as_deref(), Some(line.as_str()));
        assert!(f.provider.turns().is_empty());
        assert_eq!(f.provider.interrupts(), 0, "the agent's turn is not what stopped");
        assert_eq!(f.rows().1, ["PART\nInterrupted"]);
        assert!(!f.set.interrupt_shell_line(f.tab), "nothing runs any more");
    }

    #[test]
    fn a_cd_lasts_for_the_tab_s_next_line_and_leaving_the_project_goes_back() {
        let mut f = Fixture::new("shell-line-cd");
        let root = std::fs::canonicalize(&f.dir).unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        f.line("!cd sub").unwrap();
        f.ended();
        assert_eq!(f.set.shells.cwds[&f.tab.0], root.join("sub"));
        f.start_turn();
        f.end_turn();
        f.line("!pwd").unwrap();
        f.ended();
        assert!(
            f.provider.turns()[1].contains(&format!("<bash-stdout>{}/sub<", root.display())),
            "{:?}",
            f.provider.turns()
        );
        f.start_turn();
        f.end_turn();
        f.line("!cd /").unwrap();
        f.ended();
        assert!(!f.set.shells.cwds.contains_key(&f.tab.0));
        assert!(f.provider.turns()[2].contains(&format!("\nShell cwd was reset to {}", f.dir.display())));
    }

    #[test]
    fn a_tab_that_goes_kills_its_line() {
        let mut f = Fixture::new("shell-line-close");
        let marker = f.dir.join("late");
        f.line(&format!("!sleep 1; touch {}", marker.display())).unwrap();
        let tab = f.tab;
        let removed = f.set.remove(tab).expect("the tab goes");
        drop(removed);
        f.set.poll_shell_lines();
        assert!(!f.set.shell_line_runs(tab));
        std::thread::sleep(Duration::from_millis(1300));
        assert!(!marker.exists());
    }

    #[test]
    fn a_line_is_the_words_after_a_first_bang() {
        assert_eq!(shell_command("!ls -l"), Some("ls -l"));
        assert_eq!(shell_command("! ls "), Some("ls"));
        assert_eq!(shell_command("!"), None);
        assert_eq!(shell_command("!  \n"), None);
        assert_eq!(shell_command("ls !"), None);
    }

    #[test]
    fn a_first_turn_s_mark_does_not_outlive_being_saved() {
        let turn = FirstTurn {
            wire: "!touch ran".into(),
            typed: "!touch ran".into(),
            typed_by_person: false,
            shell_line: true,
        };
        let saved = serde_json::to_string(&turn).unwrap();
        let restored: FirstTurn = serde_json::from_str(&saved).unwrap();
        assert!(!restored.shell_line);
    }

    #[test]
    fn a_first_turn_with_the_same_words_and_no_mark_is_words() {
        let mut f = Fixture::new("shell-line-first-words");
        let marker = f.dir.join("ran");
        let line = format!("!touch {}", marker.display());
        let turn = FirstTurn {
            wire: line.clone(),
            typed: line.clone(),
            typed_by_person: true,
            shell_line: false,
        };
        f.set.send_first_turn(f.tab, "first", turn).expect("it goes");
        assert!(!f.set.shell_line_runs(f.tab));
        assert_eq!(f.provider.turns(), [line]);
    }

    #[test]
    fn a_first_turn_with_the_mark_runs_the_line() {
        let mut f = Fixture::new("shell-line-first");
        let turn = FirstTurn {
            wire: "!true".into(),
            typed: "!true".into(),
            typed_by_person: false,
            shell_line: true,
        };
        f.set.send_first_turn(f.tab, "first", turn).expect("it starts");
        assert!(f.set.shell_line_runs(f.tab));
        assert!(f.provider.turns().is_empty());
    }

    /// A plain send from a page that has not heard the line started: the host holds it all the same.
    #[test]
    fn a_message_sent_while_a_line_runs_waits_for_it() {
        let mut f = Fixture::new("shell-line-holds-a-send");
        f.line("!sleep 0.3").unwrap();
        let turn = FirstTurn {
            wire: "words".into(),
            typed: "words".into(),
            typed_by_person: true,
            shell_line: false,
        };
        assert!(matches!(
            f.set.send_prompt(f.tab, "p", ViewId::BASE, turn),
            Some(Sent::Prompt { outcome: Ok(()), .. })
        ));
        assert!(f.provider.turns().is_empty());
        assert_eq!(f.set.get(f.tab).unwrap().session.queue.len(), 1);
        assert!(f.ended().flush.expect("the report").outcome.is_ok());
        assert!(f.provider.turns()[0].starts_with("<bash-input>"));
    }

    /// The CLI may start a turn of its own before it reads a report, and the report then waits as a queue item.
    #[test]
    fn a_report_put_back_by_the_cli_stays_a_report() {
        let mut f = Fixture::new("shell-line-requeued");
        f.line("!echo MARK").unwrap();
        f.ended();
        let backend = f.set.get(f.tab).unwrap();
        let opening = backend.session.opening_prompt.as_ref().expect("the report went out");
        assert!(opening.message.shell_report);
        assert!(!opening.message.steerable);
    }
}
