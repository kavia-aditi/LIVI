// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

//! Retry supervision only. Consumers own bootstrap and independently dispatched sessions.

use std::fmt::Display;
use std::time::Duration;
use tokio::sync::watch;

/// Withdraw availability before releasing registrations, including partial startup failure.
/// The cycle owns its scoped tasks; this supervisor does not adopt projection sessions.
pub async fn supervise<E: Display>(
    mut cycle: impl AsyncFnMut() -> Result<(), E>,
    mut withdraw: impl FnMut(),
    mut release: impl AsyncFnMut(),
    mut stop: watch::Receiver<bool>,
) {
    loop {
        let ended = cycle().await;
        withdraw();
        release().await;
        match ended {
            Ok(()) => break,
            Err(e) => eprintln!(
                "[helperd] Bluetooth unavailable ({e}); independent services carry on, retrying"
            ),
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            _ = stop.changed() => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[tokio::test(start_paused = true)]
    async fn partial_failure_withdraws_then_releases_before_retry() {
        let trace = Arc::new(Mutex::new(Vec::new()));
        let (_tx, stop) = watch::channel(false);
        let mut count = 0;
        supervise(
            async || {
                count += 1;
                trace.lock().unwrap().push("cycle");
                if count == 1 { Err("partial startup") } else { Ok(()) }
            },
            || trace.lock().unwrap().push("withdraw"),
            async || trace.lock().unwrap().push("release"),
            stop,
        ).await;
        assert_eq!(*trace.lock().unwrap(), [
            "cycle", "withdraw", "release", "cycle", "withdraw", "release",
        ]);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_interrupts_retry_without_starting_another_cycle() {
        let (tx, stop) = watch::channel(false);
        let mut count = 0;
        supervise(
            async || {
                count += 1;
                Err("lost adapter")
            },
            || { tx.send_replace(true); },
            async || {},
            stop,
        ).await;
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn owner_tasks_end_but_dispatched_sessions_are_not_adopted() {
        let (_tx, stop) = watch::channel(false);
        let (started, mut ready) = tokio::sync::mpsc::unbounded_channel();
        let session = tokio::spawn(std::future::pending::<()>());
        let mut owned = None;
        supervise(
            async || {
                let mut tasks = tokio::task::JoinSet::new();
                let tx = started.clone();
                owned = Some(tasks.spawn(async move {
                    tx.send(()).unwrap();
                    std::future::pending::<()>().await;
                }));
                ready.recv().await.unwrap();
                Ok::<(), &str>(())
            },
            || {},
            async || {},
            stop,
        ).await;
        tokio::task::yield_now().await;
        assert!(owned.unwrap().is_finished());
        assert!(!session.is_finished());
        session.abort();
        let _ = session.await;
    }
}
