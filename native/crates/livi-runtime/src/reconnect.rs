// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

use std::sync::Arc;
use std::time::Instant;
use zbus::Connection;
use crate::bluetooth::policy::{Action, Backend, Observation, Reconnect, INTERVAL, PAGE_TIMEOUT};
use crate::state::HelperState;

const HFP_AG_UUID: &str = "0000111f-0000-1000-8000-00805f9b34fb";

pub async fn run(conn: Connection, adapter: String, state: Arc<HelperState>) {
    run_with_readiness(conn, adapter, state, || false).await;
}

/// Existing AP readiness is observed passively; this does not configure Wi-Fi.
pub async fn run_with_wifi(conn: Connection, adapter: String, ap_iface: String, state: Arc<HelperState>) {
    run_with_readiness(conn, adapter, state, move || crate::net::wlan_link_local(&ap_iface).is_some()).await;
}

async fn run_with_readiness(
    conn: Connection, adapter: String, state: Arc<HelperState>,
    wifi_ready: impl Fn() -> bool,
) {
    let mut policy = Reconnect::default();
    loop {
        tokio::time::sleep(INTERVAL).await;
        let Some((mac, uuid)) = policy.next(&state.reconnect_targets()) else { continue };
        let path = device_path(&adapter, &mac);
        let Some(connected) = device_connected(&conn, &path).await else { continue };
        if !paging_allowed(connected, wifi_ready()) { continue; }
        let action = policy.decide(Backend::Linux, &mac, Observation {
            connected,
            link_active: connected && state.link_active(&mac),
            hands_free: uuid.as_deref().is_some_and(|u| u.eq_ignore_ascii_case(HFP_AG_UUID)),
            has_wake: uuid.is_some(),
            ..Default::default()
        }, Instant::now());
        match action {
            Action::Skip => {}
            Action::Disconnect => {
                println!("[cp] reconnect: {mac} connected but no session, disconnecting");
                let _ = disconnect(&conn, &path).await;
            }
            Action::Page { track: false } => {
                let _ = tokio::time::timeout(PAGE_TIMEOUT, page(&conn, &path, uuid.as_deref())).await;
            }
            Action::Page { track: true } => {
                println!("[cp] reconnect: paging {mac}");
                let success = match tokio::time::timeout(PAGE_TIMEOUT, page(&conn, &path, uuid.as_deref())).await {
                    Ok(Ok(())) => {
                        println!("[cp] reconnect: {mac} connected");
                        true
                    }
                    Ok(Err(e)) => {
                        println!("[cp] reconnect: {mac} page failed: {e}");
                        false
                    }
                    Err(_) => false,
                };
                policy.completed(&mac, success, Instant::now());
            }
        }
    }
}

fn paging_allowed(connected: bool, wifi_ready: bool) -> bool {
    connected || wifi_ready
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnected_paging_waits_for_wifi_but_connected_stale_handling_does_not() {
        assert!(!paging_allowed(false, false));
        assert!(paging_allowed(false, true));
        assert!(paging_allowed(true, false));
        assert!(paging_allowed(true, true));
    }
}

fn device_path(adapter: &str, mac: &str) -> String {
    format!(
        "/org/bluez/{}/dev_{}",
        adapter,
        mac.replace(':', "_").to_uppercase()
    )
}

async fn device_connected(conn: &Connection, path: &str) -> Option<bool> {
    let reply = conn
        .call_method(
            Some("org.bluez"),
            path,
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.bluez.Device1", "Connected"),
        )
        .await
        .ok()?;
    let value: zbus::OwnedVariant = reply.body().deserialize().ok()?;
    bool::try_from(value).ok()
}

async fn page(conn: &Connection, path: &str, uuid: Option<&str>) -> Result<(), zbus::Error> {
    match uuid {
        Some(uuid) => {
            conn.call_method(
                Some("org.bluez"),
                path,
                Some("org.bluez.Device1"),
                "ConnectProfile",
                &(uuid,),
            )
            .await?;
        }
        None => {
            conn.call_method(
                Some("org.bluez"),
                path,
                Some("org.bluez.Device1"),
                "Connect",
                &(),
            )
            .await?;
        }
    }
    Ok(())
}

async fn disconnect(conn: &Connection, path: &str) -> Result<(), zbus::Error> {
    conn.call_method(
        Some("org.bluez"),
        path,
        Some("org.bluez.Device1"),
        "Disconnect",
        &(),
    )
    .await?;
    Ok(())
}
