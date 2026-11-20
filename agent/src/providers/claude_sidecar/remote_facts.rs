//! A remote tab's following of the helper's path facts: one `WatchFacts` stream per open session watch, the
//! cache it fills ([`crate::gate_facts::FactCache`]), and the fresh check before an automatic answer.
//!
//! The stream is opened again every time the session's own watch opens, and the cache's facts are dropped every
//! time that watch breaks, so no fact from before a reconnect is used. A fact's stream that breaks on its own is
//! opened again on the reconnect delays; its facts are dropped meanwhile. Every fact that arrives, and every
//! fresh check's answer, wakes the tab.

use crate::gate_facts::{decode_fresh, FactCache, FreshFact, GateFactState, RootFact, GATE_FRESH_WAIT};
use crate::ingestion::UiWake;
use crate::remote_client::{FactAsker, ReconnectClock, REMOTE_RECONNECT_BACKOFF};
use claude_runtime_protocol::v1::SessionEvent;
use eitri_server_proto::gate_fact_event::Event as FactEvent;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// The longest root the laptop names a sidecar's requests by; a longer one serves no facts.
const ROOT_MAX: usize = 4096;

type Shared<T> = Arc<Mutex<T>>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn wake(slot: &Mutex<Option<UiWake>>) {
    let wake = lock(slot).clone();
    if let Some(wake) = wake {
        wake();
    }
}

/// One session's cache and the task that fills it.
struct Following {
    cache: Shared<FactCache>,
    task: Option<tokio::task::AbortHandle>,
}

pub(super) struct RemoteFacts {
    asker: FactAsker,
    /// The sidecar's recorded root, which the helper names requests by.
    root: PathBuf,
    clock: Arc<dyn ReconnectClock>,
    runtime: tokio::runtime::Handle,
    following: Mutex<Option<Following>>,
    wake: Shared<Option<UiWake>>,
}

impl RemoteFacts {
    /// Facts for the sidecar whose recorded root is `root`, or `None` when that root cannot name a request (empty,
    /// relative or past [`ROOT_MAX`]): the tab then keeps the gate without facts.
    pub(super) fn new(
        asker: FactAsker,
        root: &str,
        clock: Arc<dyn ReconnectClock>,
        runtime: tokio::runtime::Handle,
    ) -> Option<Self> {
        if root.is_empty() || root.len() > ROOT_MAX || !root.starts_with('/') {
            return None;
        }
        Some(Self {
            asker,
            root: PathBuf::from(root),
            clock,
            runtime,
            following: Mutex::new(None),
            wake: Arc::default(),
        })
    }

    /// The sidecar's recorded root, which the helper names requests by.
    pub(super) fn root(&self) -> &std::path::Path {
        &self.root
    }

    pub(super) fn set_wake(&self, wake: UiWake) {
        *lock(&self.wake) = Some(wake);
    }

    /// The session's watch has opened: its fact stream opens (again), into the session's cache.
    pub(super) fn follow(&self, session_id: &str) {
        let mut following = lock(&self.following);
        let cache = match following.as_mut() {
            Some(current) if lock(&current.cache).session_id() == session_id => {
                if let Some(task) = current.task.take() {
                    task.abort();
                }
                current.cache.clone()
            }
            _ => {
                if let Some(task) = following.as_mut().and_then(|old| old.task.take()) {
                    task.abort();
                }
                Arc::new(Mutex::new(FactCache::new(session_id, &self.root)))
            }
        };
        lock(&cache).disconnected();
        let task = self.runtime.spawn(stream_facts(
            self.asker.clone(),
            session_id.to_owned(),
            cache.clone(),
            self.wake.clone(),
            self.clock.clone(),
        ));
        *following = Some(Following {
            cache,
            task: Some(task.abort_handle()),
        });
    }

    /// A session event as it is delivered, so requests are named by their own bytes.
    pub(super) fn observe(&self, event: &SessionEvent) {
        if let Some(following) = lock(&self.following).as_ref() {
            lock(&following.cache).observe(event);
        }
    }

    /// The watch of `session_id` broke or ended: if that is the session followed, its fact stream stops and nothing
    /// it sent is used again. The end of an older session's watch leaves the current one's facts alone.
    pub(super) fn disconnected(&self, session_id: &str) {
        if let Some(following) = lock(&self.following).as_mut() {
            if lock(&following.cache).session_id() != session_id {
                return;
            }
            if let Some(task) = following.task.take() {
                task.abort();
            }
            lock(&following.cache).disconnected();
        }
        wake(&self.wake);
    }

    pub(super) fn state(&self, permission_id: &str) -> GateFactState {
        match lock(&self.following).as_ref() {
            Some(following) => lock(&following.cache).state(permission_id),
            None => GateFactState::Waiting,
        }
    }

    /// The digest of the pending request `permission_id` in the followed session, as this machine computed it.
    pub(super) fn expected_digest(&self, permission_id: &str) -> Option<[u8; 32]> {
        lock(&self.following)
            .as_ref()
            .and_then(|following| lock(&following.cache).expected_digest(permission_id))
    }

    pub(super) fn root_fact(&self) -> Option<RootFact> {
        lock(&self.following)
            .as_ref()
            .and_then(|following| lock(&following.cache).root_fact())
    }

    /// The fresh check of the pending request `permission_id`, named by `digest`, on the connection's runtime
    /// within [`GATE_FRESH_WAIT`]. `done` is called exactly once, then the tab is woken: with `Unavailable` at once,
    /// on the caller's thread, when no session is followed, and otherwise on the runtime, or with `Unavailable` when
    /// the check's task is dropped before it answers (the runtime shutting down). No lock is held while it runs.
    pub(super) fn check(&self, permission_id: &str, digest: [u8; 32], done: Box<dyn FnOnce(FreshFact) + Send>) {
        let session_id = lock(&self.following)
            .as_ref()
            .map(|following| lock(&following.cache).session_id().to_owned());
        let mut once = Once {
            done: Some(done),
            wake: self.wake.clone(),
        };
        let Some(session_id) = session_id else {
            once.call(FreshFact::Unavailable);
            return;
        };
        let (asker, permission_id) = (self.asker.clone(), permission_id.to_owned());
        self.runtime.spawn(async move {
            let answer = asker.check(&session_id, &permission_id, digest, GATE_FRESH_WAIT).await;
            once.call(match answer {
                Ok(answer) => decode_fresh(&answer, &digest),
                Err(_) => FreshFact::Unavailable,
            });
        });
    }
}

/// Held by a session's watch for as long as it runs: however the watch ends (a loss, a gap, the session closing, the
/// provider dropping it), the session's facts stop being followed.
pub(super) struct WatchingFacts {
    pub(super) facts: Arc<RemoteFacts>,
    pub(super) session_id: String,
}

impl Drop for WatchingFacts {
    fn drop(&mut self) {
        self.facts.disconnected(&self.session_id);
    }
}

/// A fresh check's `done`, called at most once and, if its task is dropped first, called with `Unavailable` then; the
/// tab is woken after it.
struct Once {
    done: Option<Box<dyn FnOnce(FreshFact) + Send>>,
    wake: Shared<Option<UiWake>>,
}

impl Once {
    fn call(&mut self, fresh: FreshFact) {
        if let Some(done) = self.done.take() {
            done(fresh);
            wake(&self.wake);
        }
    }
}

impl Drop for Once {
    fn drop(&mut self) {
        self.call(FreshFact::Unavailable);
    }
}

impl Drop for RemoteFacts {
    fn drop(&mut self) {
        if let Some(task) = lock(&self.following)
            .as_mut()
            .and_then(|following| following.task.take())
        {
            task.abort();
        }
    }
}

/// Reads `session_id`'s fact stream into `cache` until the helper ends it (the session closed or is no longer
/// followed); a stream that breaks drops what it sent and is opened again on the reconnect delays.
async fn stream_facts(
    asker: FactAsker,
    session_id: String,
    cache: Shared<FactCache>,
    wake_slot: Shared<Option<UiWake>>,
    clock: Arc<dyn ReconnectClock>,
) {
    let mut failures = 0usize;
    loop {
        if let Ok(mut stream) = asker.watch(&session_id).await {
            loop {
                match stream.message().await {
                    Ok(Some(event)) => {
                        failures = 0;
                        match event.event {
                            Some(FactEvent::Root(root)) => lock(&cache).receive_root(&root),
                            Some(FactEvent::Request(fact)) => lock(&cache).receive(&fact),
                            None => continue,
                        }
                        wake(&wake_slot);
                    }
                    Ok(None) => {
                        lock(&cache).disconnected();
                        wake(&wake_slot);
                        return;
                    }
                    Err(_) => break,
                }
            }
        }
        lock(&cache).disconnected();
        wake(&wake_slot);
        let wait = REMOTE_RECONNECT_BACKOFF[failures.min(REMOTE_RECONNECT_BACKOFF.len() - 1)];
        failures += 1;
        clock.sleep(wait).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn a_fresh_checks_done_runs_once_even_when_its_task_is_dropped() {
        let wakes = Arc::new(AtomicUsize::new(0));
        let counted = wakes.clone();
        let slot: Shared<Option<UiWake>> = Arc::new(Mutex::new(Some(Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        }))));
        let (told, answers) = std::sync::mpsc::channel();
        let first = told.clone();
        let mut once = Once {
            done: Some(Box::new(move |fresh| first.send(fresh).unwrap())),
            wake: slot.clone(),
        };
        once.call(FreshFact::Refused("no longer pending".into()));
        once.call(FreshFact::Unavailable);
        drop(once);
        assert_eq!(answers.try_recv(), Ok(FreshFact::Refused("no longer pending".into())));
        assert!(answers.try_recv().is_err());
        assert_eq!(wakes.load(Ordering::SeqCst), 1);
        // A task dropped before it answered still answers, once.
        drop(Once {
            done: Some(Box::new(move |fresh| told.send(fresh).unwrap())),
            wake: slot,
        });
        assert_eq!(answers.try_recv(), Ok(FreshFact::Unavailable));
        assert_eq!(wakes.load(Ordering::SeqCst), 2);
    }
}
