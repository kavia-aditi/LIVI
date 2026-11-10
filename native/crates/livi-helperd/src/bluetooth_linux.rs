// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

//! Linux Bluetooth ownership. Protocol bootstrap and independent services live in composition.

use std::sync::Mutex;
use std::time::Duration;
use livi_runtime::bt;
use livi_runtime::livi_sock::Bluez;
use tokio::sync::{mpsc, watch};

pub fn arrival(
    incoming: bt::IncomingConn,
    local: [u8; 6],
) -> livi_runtime::bluetooth::arrival::Arrival<std::os::fd::OwnedFd> {
    livi_runtime::bluetooth::arrival::Arrival {
        peer: incoming.peer_mac,
        local,
        stream: incoming.fd,
    }
}

pub fn adapters(adapter: String) -> watch::Receiver<Option<String>> {
    if adapter != livi_link_host::link::CHOICE {
        return watch::channel(Some(adapter)).1;
    }
    let (tx, rx) = watch::channel(None);
    let lost = tx.clone();
    livi_link_host::bt::attach(
        move |index| { tx.send_replace(Some(format!("hci{index}"))); },
        move || { lost.send_replace(None); },
    );
    rx
}

#[derive(Default)]
pub struct Owner {
    active: Mutex<Option<(Bluez, bt::Held)>>,
}

impl Owner {
    pub async fn start(
        &self,
        adapter: &str,
        name: &str,
        wireless: bt::Wireless,
    ) -> Result<(Bluez, mpsc::UnboundedReceiver<bt::IncomingConn>), Box<dyn std::error::Error>> {
        livi_runtime::bluetoothd::setup(wireless);
        println!(
            "[helperd] starting BlueZ profile on {adapter} (wireless CarPlay {}, wireless Android Auto {})",
            if wireless.carplay { "on" } else { "off" },
            if wireless.android_auto { "on" } else { "off" }
        );
        let (conn, incoming, held) = bt::start(adapter, name, wireless).await?;
        let selected = Bluez {
            bus: conn,
            adapter: adapter.to_string(),
            bt_mac: [0; 6],
        };
        *self.active.lock().unwrap() = Some((selected.clone(), held));
        Ok((selected, incoming))
    }

    pub async fn release(&self) {
        let active = self.active.lock().unwrap().take();
        if let Some((bluez, held)) = active {
            bt::let_go(&bluez.bus, &bluez.adapter, &held).await;
        }
    }
}

pub async fn healthy(selected: &Bluez, owner: &str) -> bool {
    let ready = tokio::time::timeout(Duration::from_secs(3), async {
        bt::adapter_address(&selected.bus, &selected.adapter).await?;
        bt::service_owner(&selected.bus).await
    }).await;
    matches!(ready, Ok(Ok(current)) if current == owner)
}
