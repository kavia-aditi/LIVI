// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use livi_runtime::AsyncAuth;
use livi_runtime::bringup::CpConfig;
use livi_runtime::ident::{Identity, Transport};
use livi_runtime::livi_sock::{Bluez, Broadcaster, LiviSockConfig, serve};
use livi_runtime::state::HelperState;
use livi_runtime::vehicle::{Fuels, Plugs};
use std::sync::Arc;
use tokio::sync::{Notify, watch};

#[derive(Clone)]
struct MockAuth;

impl AsyncAuth for MockAuth {
    async fn read_certificate(&mut self) -> Result<Vec<u8>, String> {
        Ok(vec![0xDE, 0xAD, 0xBE, 0xEF])
    }
    async fn sign(&mut self, challenge: Vec<u8>) -> Result<Vec<u8>, String> {
        Ok(challenge.iter().rev().copied().collect())
    }
    async fn protocol_major(&mut self) -> Result<u8, String> {
        Ok(3)
    }
}

fn config(path: &str) -> LiviSockConfig {
    LiviSockConfig {
        path: path.into(),
        identity: Identity {
            name: "LIVI".into(),
            ssid: "LIVI".into(),
            bt_mac: [0; 6],
            fuels: Fuels::default(),
            plugs: Plugs::default(),
            theme: None,
        },
        cp: CpConfig {
            device_id: None,
            ap_mac: None,
            ap_on_air: None,
            wifi_iface: "none0".into(),
            ssid: "LIVI".into(),
            passphrase: "12345678".into(),
            channel: livi_wifi::Channel::of_number(36),
            airplay_port: 7000,
            source_version: "950.7.1".into(),
            public_key: String::new(),
            transport: Transport::Wireless,
            av_iface: None,
            av_iface_late: None,
            available_current_ma: 500,
            on_cable: None,
            start_again: None,
            theme: None,
        },
        disconnect: None,
        targets: None,
        cp_live: None,
    }
}

fn no_bluez() -> watch::Receiver<Option<Bluez>> {
    watch::channel(None).1
}

async fn listening(path: &str) {
    for _ in 0..50 {
        if UnixStream::connect(path).await.is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn a_wired_phone_hears_the_start_again_without_its_session_ending() {
    let path = std::env::temp_dir()
        .join(format!("livi-sock-cable-{}", std::process::id()))
        .to_string_lossy()
        .to_string();
    let state = Arc::new(HelperState::default());
    let again = Arc::new(Notify::new());
    state.wired_started("00008120", Arc::new(Notify::new()), again.clone());
    let server = tokio::spawn(serve(
        config(&path),
        MockAuth,
        no_bluez(),
        Broadcaster::default(),
        state,
    ));
    listening(&path).await;

    assert!(
        request(&path, "start-wired 00008120")
            .await
            .contains("\"ok\":true")
    );
    again.notified().await;
    assert!(
        request(&path, "start-wired 00008030")
            .await
            .contains("\"ok\":false")
    );

    server.abort();
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn before_bluetooth_is_up_a_phone_cannot_be_dropped_but_the_socket_serves() {
    let path = std::env::temp_dir()
        .join(format!("livi-sock-no-bt-{}", std::process::id()))
        .to_string_lossy()
        .to_string();
    let state = Arc::new(HelperState::default());
    let (_bluez, later) = watch::channel(None);
    let server = tokio::spawn(serve(
        config(&path),
        MockAuth,
        later,
        Broadcaster::default(),
        state,
    ));
    listening(&path).await;

    let dropped = request(&path, "disconnect 0C:6A:C4:4E:F3:2A").await;
    assert!(dropped.contains("\"ok\":false"), "{dropped}");
    assert!(request(&path, "certificate").await.contains("\"ok\":true"));

    server.abort();
    let _ = std::fs::remove_file(&path);
}

async fn request(path: &str, line: &str) -> String {
    let stream = UnixStream::connect(path).await.unwrap();
    let mut reader = BufReader::new(stream);
    reader
        .get_mut()
        .write_all(format!("{line}\n").as_bytes())
        .await
        .unwrap();
    let mut resp = String::new();
    reader.read_line(&mut resp).await.unwrap();
    resp.trim().to_string()
}

#[tokio::test]
async fn disconnect_fallback_receives_the_original_mac_only_when_supplied() {
    let path = std::env::temp_dir().join(format!("livi-fallback-{}", std::process::id()))
        .to_string_lossy().to_string();
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = calls.clone();
    let mut cfg = config(&path);
    cfg.disconnect = Some(Arc::new(move |mac| {
        seen.lock().unwrap().push(mac);
        Ok(())
    }));
    let server = tokio::spawn(serve(cfg, MockAuth, no_bluez(), Broadcaster::default(), Arc::default()));
    listening(&path).await;
    let missing: serde_json::Value = serde_json::from_str(&request(&path, "disconnect").await).unwrap();
    assert_eq!(missing["error"], "disconnect requires a MAC");
    assert!(calls.lock().unwrap().is_empty());
    let reply: serde_json::Value = serde_json::from_str(&request(&path, "disconnect aa:BB").await).unwrap();
    assert_eq!(reply["ok"], true);
    assert_eq!(*calls.lock().unwrap(), ["aa:BB"]);
    server.abort();
    let _ = server.await;
    let _ = std::fs::remove_file(path);
}

struct AcceptedAuth(Arc<Notify>);

impl Clone for AcceptedAuth {
    fn clone(&self) -> Self {
        self.0.notify_one();
        Self(self.0.clone())
    }
}

impl AsyncAuth for AcceptedAuth {
    async fn read_certificate(&mut self) -> Result<Vec<u8>, String> { Ok(vec![]) }
    async fn sign(&mut self, challenge: Vec<u8>) -> Result<Vec<u8>, String> { Ok(challenge) }
    async fn protocol_major(&mut self) -> Result<u8, String> { Ok(3) }
}

struct DisconnectPhone(Arc<std::sync::atomic::AtomicUsize>);

#[zbus::interface(name = "org.bluez.Device1")]
impl DisconnectPhone {
    fn disconnect(&self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[tokio::test]
#[ignore = "requires the private D-Bus session configuration"]
async fn disconnect_pins_the_accept_snapshot_and_never_falls_back_after_bluez_error() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let drops = Arc::new(AtomicUsize::new(0));
    let service = zbus::connection::Builder::session().name("org.bluez")
        .serve_at("/org/bluez/hci2/dev_00_11_22_33_44_55", DisconnectPhone(drops.clone()))
        .build().await.unwrap();
    let selected = Bluez {
        bus: zbus::Connection::session().await.unwrap(),
        adapter: "hci2".into(), bt_mac: [1; 6],
    };
    let (tx, rx) = watch::channel(Some(selected.clone()));
    let fallback = Arc::new(AtomicUsize::new(0));
    let count = fallback.clone();
    let path = std::env::temp_dir().join(format!("livi-snapshot-{}", std::process::id()))
        .to_string_lossy().to_string();
    let accepted = Arc::new(Notify::new());
    let mut cfg = config(&path);
    cfg.disconnect = Some(Arc::new(move |_| { count.fetch_add(1, Ordering::SeqCst); Ok(()) }));
    let server = tokio::spawn(serve(cfg, AcceptedAuth(accepted.clone()), rx, Broadcaster::default(), Arc::default()));
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !std::path::Path::new(&path).exists() { tokio::task::yield_now().await; }
    }).await.unwrap();

    // Current-thread scheduling: clone notification cannot be observed until accept has
    // completed its synchronous snapshot, and no probe connections consume the signal.
    for step in 0..3 {
        let stream = UnixStream::connect(&path).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), accepted.notified()).await.unwrap();
        match step {
            0 => { tx.send_replace(None); }
            1 => { tx.send_replace(Some(selected.clone())); }
            _ => { service.release_name("org.bluez").await.unwrap(); }
        }
        let mut reader = BufReader::new(stream);
        reader.get_mut().write_all(b"disconnect 00:11:22:33:44:55\n").await.unwrap();
        let mut line = String::new();
        tokio::time::timeout(std::time::Duration::from_secs(2), reader.read_line(&mut line)).await.unwrap().unwrap();
        let reply: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["ok"], step != 2);
    }
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(fallback.load(Ordering::SeqCst), 1);
    server.abort();
    let _ = server.await;
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn certificate_sign_and_subscribe() {
    let dir = std::env::temp_dir().join(format!("livi-sock-{}", std::process::id()));
    let path = dir.to_string_lossy().to_string();
    let bcast = Broadcaster::default();
    let state = Arc::new(HelperState::default());
    let bluez = no_bluez();
    let server = tokio::spawn(serve(config(&path), MockAuth, bluez, bcast.clone(), state));

    for _ in 0..50 {
        if UnixStream::connect(&path).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let cert = request(&path, "certificate").await;
    assert!(cert.contains("\"ok\":true"));
    assert!(cert.contains(&STANDARD.encode([0xDE, 0xAD, 0xBE, 0xEF])));
    assert!(cert.contains("\"protocolMajor\":3"));

    let digest = STANDARD.encode([1u8, 2, 3, 4]);
    let sig = request(&path, &format!("sign {digest}")).await;
    assert!(sig.contains(&STANDARD.encode([4u8, 3, 2, 1])));

    let unknown = request(&path, "bogus").await;
    assert!(unknown.contains("\"ok\":false"));

    let stream = UnixStream::connect(&path).await.unwrap();
    let mut reader = BufReader::new(stream);
    reader.get_mut().write_all(b"subscribe\n").await.unwrap();
    bcast.subscribed().await;
    bcast.push_json("{\"type\":\"nowplaying\",\"title\":\"Song\"}".into());
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.contains("nowplaying"));

    server.abort();
    let _ = std::fs::remove_file(&path);
}
