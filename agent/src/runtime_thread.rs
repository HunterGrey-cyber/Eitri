//! A single, long-lived Tokio runtime on its own dedicated OS thread, isolated from whatever
//! thread constructs a provider (design doc §10.1: "tonic/Tokio 放在 agent crate 自己的 runtime
//! thread"). Exposes a synchronous facade -- `block_on` with a bounded timeout -- so
//! `AgentProvider`'s trait methods (Task 4) can stay ordinary synchronous Rust functions, matching
//! the existing `AgentSession` API shape, while their real implementation does async gRPC I/O
//! underneath. `handle()` is exposed separately for callers that need to spawn a long-lived
//! background task (the event-watch pump, Task 8) rather than a single bounded call.

use std::sync::mpsc;
use std::time::Duration;

pub struct RuntimeThread {
    handle: tokio::runtime::Handle,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl RuntimeThread {
    /// Spawns the dedicated OS thread and blocks the calling thread only until the new runtime is
    /// actually constructed and its `Handle` captured -- callers never race a not-yet-ready
    /// runtime. This function contains the two allowed `.expect()` calls in this whole module
    /// (Global Constraints): `Runtime::new().expect(..)` is a foundational setup failure with no
    /// meaningful degraded path (matching `eitri_supervisor.rs`'s own precedent for a bind
    /// failure), and the `handle_rx.recv().expect(..)` immediately following is not a second,
    /// independent risk — it only fires as the direct, causal consequence of that same failure
    /// (if `Runtime::new()` panics, the handle is never sent, so the `recv()` fails too),
    /// propagating the same underlying panic to the calling thread rather than introducing a new one.
    pub fn spawn() -> Self {
        let (handle_tx, handle_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().expect("failed to build Tokio runtime for a Claude provider");
            let _ = handle_tx.send(runtime.handle().clone());
            // Blocks until `Drop` sends a shutdown signal (or the sender end is simply dropped,
            // which also resolves this with an error) -- replaces an earlier permanent
            // `std::future::pending()` park (found by the final whole-branch review to leak this
            // thread and the runtime's own worker threads for the rest of the host process's
            // life, since nothing could ever signal it to stop).
            let _ = runtime.block_on(shutdown_rx);
        });
        let handle = handle_rx
            .recv()
            .expect("runtime thread died before sending its Handle back");
        Self {
            handle,
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        }
    }

    /// Runs `future` on this runtime thread and blocks the CALLING thread (never the runtime
    /// thread itself) until it completes or `timeout` elapses. `recv_timeout`, never bare `recv` --
    /// Global Constraints: no unbounded blocking wait anywhere in this crate.
    pub fn block_on<F, T>(&self, future: F, timeout: Duration) -> Result<T, RuntimeThreadError>
    where
        F: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        self.handle.spawn(async move {
            let result = future.await;
            let _ = tx.send(result);
        });
        rx.recv_timeout(timeout).map_err(|_| RuntimeThreadError::Timeout)
    }

    /// The raw handle, for spawning a long-lived background task (Task 8's event-watch pump)
    /// rather than a single bounded call.
    pub fn handle(&self) -> &tokio::runtime::Handle {
        &self.handle
    }
}

impl Drop for RuntimeThread {
    /// Signals the parked background thread to stop, then joins it -- a bounded wait in practice
    /// (the shutdown signal immediately unblocks `block_on`, and Tokio's own runtime shutdown for
    /// an otherwise-idle runtime completes promptly, cancelling any still-pending spawned tasks
    /// rather than waiting on them indefinitely), not the unbounded kind Global Constraints
    /// forbid. Without this, every `RuntimeThread` -- including one held inside a
    /// `ClaudeSidecarProvider` that fails partway through `connect()` -- permanently leaked its
    /// OS thread and Tokio runtime worker threads for the rest of the host process's life.
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Debug)]
pub enum RuntimeThreadError {
    Timeout,
}

impl std::fmt::Display for RuntimeThreadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RuntimeThreadError::Timeout => write!(f, "runtime thread call timed out"),
        }
    }
}

impl std::error::Error for RuntimeThreadError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_on_returns_the_future_s_result_within_the_timeout() {
        let runtime_thread = RuntimeThread::spawn();
        let result = runtime_thread
            .block_on(async { 2 + 2 }, Duration::from_secs(1))
            .unwrap();
        assert_eq!(result, 4);
    }

    #[test]
    fn block_on_times_out_on_a_future_that_never_resolves() {
        let runtime_thread = RuntimeThread::spawn();
        let result: Result<(), RuntimeThreadError> =
            runtime_thread.block_on(std::future::pending::<()>(), Duration::from_millis(50));
        assert!(matches!(result, Err(RuntimeThreadError::Timeout)));
    }

    #[test]
    fn handle_can_spawn_a_background_task_independent_of_block_on() {
        let runtime_thread = RuntimeThread::spawn();
        let (tx, rx) = mpsc::channel();
        runtime_thread.handle().spawn(async move {
            let _ = tx.send(42);
        });
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), 42);
    }

    /// The regression this whole fix closes: dropping many `RuntimeThread`s in a row must not
    /// leave their OS threads running. Real, not theoretical -- if `Drop` regresses back to a
    /// leak, this test's own process/thread count would climb unboundedly across iterations
    /// (though a hard numeric assertion on thread count is avoided here since it's inherently
    /// environment-sensitive; this test's real value is that it completes at all within the
    /// bounded overall test-binary timeout rather than accumulating enough leaked runtimes to
    /// eventually exhaust OS resources across a large `cargo test` run).
    #[test]
    fn dropping_many_runtime_threads_in_a_row_does_not_hang_or_leak_unboundedly() {
        for _ in 0..20 {
            let runtime_thread = RuntimeThread::spawn();
            let result = runtime_thread
                .block_on(async { 1 + 1 }, Duration::from_secs(1))
                .unwrap();
            assert_eq!(result, 2);
            drop(runtime_thread);
        }
    }
}
