// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

//! Internal control seams. Wire parsing/serialization stays with each socket.

use std::future::Future;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Command {
    ListPaired,
    Connect,
    DisconnectProfile,
    ConnectFull,
    Disconnect,
    Remove,
}

impl Command {
    pub(crate) fn parse(verb: &str) -> Option<Self> {
        Some(match verb {
            "list_paired" => Self::ListPaired,
            "connect" => Self::Connect,
            "disconnect-profile" => Self::DisconnectProfile,
            "connect-full" => Self::ConnectFull,
            "disconnect" => Self::Disconnect,
            "remove" => Self::Remove,
            _ => return None,
        })
    }

    pub(crate) fn verb(self) -> &'static str {
        match self {
            Self::ListPaired => "list_paired",
            Self::Connect => "connect",
            Self::DisconnectProfile => "disconnect-profile",
            Self::ConnectFull => "connect-full",
            Self::Disconnect => "disconnect",
            Self::Remove => "remove",
        }
    }
}

/// The response remains owned by the socket adapter, not by radio policy.
pub(crate) trait Control {
    fn execute(&self, command: Command, argument: &str) -> impl Future<Output = String> + Send;
}

pub(crate) trait Disconnect {
    fn disconnect(&self, mac: &str) -> impl Future<Output = Result<(), String>> + Send;
}

type Fallback = Arc<dyn Fn(String) -> Result<(), String> + Send + Sync>;

/// A selected backend is authoritative, even when it fails. Only absence permits fallback.
pub(crate) async fn disconnect<B: Disconnect>(
    selected: Option<&B>,
    fallback: Option<Fallback>,
    mac: &str,
) -> Result<(), String> {
    if mac.is_empty() {
        return Err("disconnect requires a MAC".into());
    }
    if let Some(backend) = selected {
        return backend.disconnect(mac).await;
    }
    match fallback {
        Some(drop) => tokio::task::spawn_blocking({
            let mac = mac.to_string();
            move || drop(mac)
        })
        .await
        .map_err(|e| e.to_string())?,
        None => Err("nothing here can drop a bluetooth link".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake;
    impl Disconnect for Fake {
        async fn disconnect(&self, _: &str) -> Result<(), String> {
            Err("backend failed".into())
        }
    }

    #[tokio::test]
    async fn selected_backend_failure_never_uses_fallback() {
        let fallback: Fallback = Arc::new(|_| panic!("must not fall back"));
        assert_eq!(
            disconnect(Some(&Fake), Some(fallback), "AA").await,
            Err("backend failed".into())
        );
    }

    #[tokio::test]
    async fn fallback_only_runs_when_absent_and_a_mac_is_supplied() {
        let fallback: Fallback = Arc::new(|mac| {
            assert_eq!(mac, "aa:BB");
            Ok(())
        });
        assert_eq!(disconnect::<Fake>(None, Some(fallback.clone()), "aa:BB").await, Ok(()));
        assert_eq!(
            disconnect::<Fake>(None, Some(fallback), "").await,
            Err("disconnect requires a MAC".into())
        );
        assert_eq!(
            disconnect::<Fake>(None, None, "AA").await,
            Err("nothing here can drop a bluetooth link".into())
        );
    }

    #[test]
    fn only_existing_commands_are_bluetooth_commands() {
        for verb in ["list_paired", "connect", "disconnect-profile", "connect-full", "disconnect", "remove"] {
            assert_eq!(Command::parse(verb).unwrap().verb(), verb);
        }
        for verb in ["subscribe", "deauth-ap", "playback-status", "CONNECT", "bogus"] {
            assert_eq!(Command::parse(verb), None);
        }
    }
}
