// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

//! Run with `dbus-run-session -- cargo test -p livi-runtime --test reconnect
//! -- --ignored --test-threads=1`. No Bluetooth or Wi-Fi hardware is used.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use livi_runtime::{reconnect, state::HelperState};
use tokio::sync::mpsc;

const MAC: &str = "00:11:22:33:44:55";
const PATH: &str = "/org/bluez/hci2/dev_00_11_22_33_44_55";
const HFP: &str = "0000111F-0000-1000-8000-00805F9B34FB";
const IAP: &str = "00000000-deca-fade-deca-deafdecacafe";

#[derive(Clone)]
struct Phone {
    connected: Arc<AtomicBool>,
    available: Arc<AtomicBool>,
    calls: mpsc::UnboundedSender<String>,
}

#[zbus::interface(name = "org.bluez.Device1")]
impl Phone {
    #[zbus(property)]
    fn connected(&self) -> zbus::fdo::Result<bool> {
        if self.available.load(Ordering::SeqCst) {
            Ok(self.connected.load(Ordering::SeqCst))
        } else {
            Err(zbus::fdo::Error::Failed("Bluetooth unavailable".into()))
        }
    }

    fn connect(&self) {
        self.connected.store(true, Ordering::SeqCst);
        let _ = self.calls.send("connect".into());
    }

    fn connect_profile(&self, uuid: String) {
        self.connected.store(true, Ordering::SeqCst);
        let _ = self.calls.send(format!("profile:{uuid}"));
    }

    fn disconnect(&self) {
        self.connected.store(false, Ordering::SeqCst);
        let _ = self.calls.send("disconnect".into());
    }
}

struct Harness {
    phone: Phone,
    calls: mpsc::UnboundedReceiver<String>,
    state: Arc<HelperState>,
    worker: tokio::task::JoinHandle<()>,
    _service: zbus::Connection,
}

impl Harness {
    async fn start(uuid: Option<&str>, connected: bool, available: bool) -> Self {
        let (calls, received) = mpsc::unbounded_channel();
        let phone = Phone {
            connected: Arc::new(AtomicBool::new(connected)),
            available: Arc::new(AtomicBool::new(available)),
            calls,
        };
        let service = zbus::connection::Builder::session()
            .name("org.bluez")
            .serve_at(PATH, phone.clone())
            .build()
            .await
            .unwrap();
        let bus = zbus::Connection::session().await.unwrap();
        let state = Arc::new(HelperState::default());
        state.set_reconnect_targets(vec![(MAC.into(), uuid.map(str::to_string))]);
        let worker = tokio::spawn(reconnect::run(bus, "hci2".into(), state.clone()));
        Self {
            phone,
            calls: received,
            state,
            worker,
            _service: service,
        }
    }

    async fn next(&mut self) -> String {
        tokio::time::timeout(Duration::from_secs(4), self.calls.recv())
            .await
            .expect("reconnect worker did not call BlueZ")
            .expect("fake BlueZ ended")
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

#[tokio::test]
#[ignore = "requires a private D-Bus session bus, not radio hardware"]
async fn disconnected_paging_waits_without_a_wifi_interface() {
    let mut h = Harness::start(Some(IAP), false, true).await;
    assert!(tokio::time::timeout(Duration::from_millis(2200), h.calls.recv()).await.is_err());
    assert!(!h.phone.connected.load(Ordering::SeqCst));
    assert_eq!(h.state.reconnect_targets(), vec![(MAC.into(), Some(IAP.into()))]);
}

#[tokio::test]
#[ignore = "requires a private D-Bus session bus, not radio hardware"]
async fn unavailable_bluetooth_recovers_without_replacing_the_worker_or_targets() {
    let mut h = Harness::start(None, false, false).await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(h.calls.try_recv().is_err());
    assert_eq!(h.state.reconnect_targets(), vec![(MAC.into(), None)]);

    h.phone.available.store(true, Ordering::SeqCst);
    assert!(tokio::time::timeout(Duration::from_millis(2200), h.calls.recv()).await.is_err());
    assert!(!h.phone.connected.load(Ordering::SeqCst));
}

#[tokio::test]
#[ignore = "requires a private D-Bus session bus, not radio hardware"]
async fn hands_free_retains_the_original_stale_disconnect() {
    let mut h = Harness::start(Some(HFP), true, true).await;
    tokio::time::timeout(Duration::from_secs(15), async {
        while h.next().await != "disconnect" {}
    }).await.expect("stale HFP target was not disconnected");
    assert!(!h.phone.connected.load(Ordering::SeqCst));
    assert!(!h.state.link_active(MAC));
}

#[tokio::test]
#[ignore = "requires a private D-Bus session bus, not radio hardware"]
async fn stale_iap_disconnects_but_does_not_repage_without_wifi() {
    let mut h = Harness::start(Some(IAP), true, true).await;
    tokio::time::timeout(Duration::from_secs(15), async {
        while h.next().await != "disconnect" {}
    })
    .await
    .expect("stale iAP link was not disconnected");
    assert!(tokio::time::timeout(Duration::from_millis(2200), h.calls.recv()).await.is_err());
}
