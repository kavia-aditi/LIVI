// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

//! Availability shared by Bluetooth owners and their consumers, not projection sessions.

pub mod arrival;
pub(crate) mod control;
pub mod lifecycle;
pub mod policy;

use tokio::sync::watch;

/// Wait for a selected adapter without blocking unrelated helper services.
pub async fn available<T: Clone>(adapters: &mut watch::Receiver<Option<T>>) -> Option<T> {
    loop {
        if let Some(adapter) = adapters.borrow_and_update().clone() {
            return Some(adapter);
        }
        adapters.changed().await.ok()?;
    }
}

/// A fixed adapter has no producer. Its closed channel is not a disconnection.
pub async fn changed<T>(adapters: &mut watch::Receiver<Option<T>>) {
    if adapters.changed().await.is_err() {
        std::future::pending::<()>().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connects_to_the_selected_adapter() {
        let (_tx, mut rx) = watch::channel(Some("hci2"));
        assert_eq!(available(&mut rx).await, Some("hci2"));
    }

    #[tokio::test]
    async fn unavailable_waits_without_blocking_other_work() {
        let (tx, mut rx) = watch::channel(None::<&str>);
        let waiting = tokio::spawn(async move { available(&mut rx).await });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        tx.send(Some("hci1")).unwrap();
        assert_eq!(waiting.await.unwrap(), Some("hci1"));
    }

    #[tokio::test]
    async fn disconnect_then_reconnect_can_select_a_new_adapter() {
        let (tx, mut rx) = watch::channel(Some("hci1"));
        assert_eq!(available(&mut rx).await, Some("hci1"));
        tx.send(None).unwrap();
        changed(&mut rx).await;
        assert!(rx.borrow().is_none());
        let waiting = tokio::spawn(async move { available(&mut rx).await });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        tx.send(Some("hci3")).unwrap();
        assert_eq!(waiting.await.unwrap(), Some("hci3"));
    }

    #[tokio::test]
    async fn a_fixed_adapter_does_not_spin_when_its_sender_is_dropped() {
        let (tx, mut rx) = watch::channel(Some("hci0"));
        drop(tx);
        assert_eq!(available(&mut rx).await, Some("hci0"));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), changed(&mut rx))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_closed_unavailable_source_ends_the_wait() {
        let (tx, mut rx) = watch::channel(None::<&str>);
        drop(tx);
        assert_eq!(available(&mut rx).await, None);
    }
}
