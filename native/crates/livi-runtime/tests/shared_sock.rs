// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::watch;

use livi_runtime::livi_sock::{Bluez, Broadcaster};
use livi_runtime::shared_sock::{SharedSockDeps, serve};

fn deps() -> SharedSockDeps {
    SharedSockDeps {
        wifi_iface: "none0".into(),
        events: Broadcaster::default(),
        set_playback_status: Box::new(|_| {}),
        deauth_dongle: Some(|| Some(2)),
    }
}

fn path(name: &str) -> String {
    std::env::temp_dir()
        .join(format!("livi-shared-{name}-{}", std::process::id()))
        .to_string_lossy()
        .to_string()
}

async fn listening(path: &str) {
    for _ in 0..50 {
        if UnixStream::connect(path).await.is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("shared socket did not start: {path}");
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
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        reader.read_line(&mut resp),
    )
    .await
    .unwrap()
    .unwrap();
    resp.trim().to_string()
}

#[tokio::test]
async fn every_phone_leaves_the_dongles_access_point() {
    let path = path("deauth");
    let server = tokio::spawn({
        let path = path.clone();
        async move { serve(&path, watch::channel(None).1, deps()).await }
    });
    listening(&path).await;

    assert_eq!(
        request(&path, "deauth-ap").await,
        "{\"ok\":true,\"count\":2}"
    );

    server.abort();
    let _ = server.await;
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn the_player_state_and_the_events_reach_their_ends() {
    let path = path("player");
    let states = Arc::new(Mutex::new(Vec::new()));
    let heard = states.clone();
    let events = Broadcaster::default();
    let deps = SharedSockDeps {
        set_playback_status: Box::new(move |s| heard.lock().unwrap().push(s.to_string())),
        events: events.clone(),
        ..deps()
    };
    let server = tokio::spawn({
        let path = path.clone();
        async move { serve(&path, watch::channel(None).1, deps).await }
    });
    listening(&path).await;

    assert!(
        request(&path, "playback-status playing")
            .await
            .contains("\"ok\":true")
    );
    assert!(
        request(&path, "playback-status rewinding")
            .await
            .contains("\"ok\":false")
    );
    assert_eq!(*states.lock().unwrap(), vec!["playing".to_string()]);
    assert!(
        request(&path, "list_paired")
            .await
            .contains("bluetooth unavailable")
    );
    assert!(request(&path, "bogus").await.contains("unknown command"));

    let stream = UnixStream::connect(&path).await.unwrap();
    let mut reader = BufReader::new(stream);
    reader.get_mut().write_all(b"subscribe\n").await.unwrap();
    events.subscribed().await;
    events.push_json("{\"event\":\"input\",\"command\":\"next\"}".into());
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.contains("\"command\":\"next\""));

    server.abort();
    let _ = server.await;
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn unavailable_bluetooth_rejects_only_bluetooth_commands() {
    let path = path("unavailable");
    let server = tokio::spawn({
        let path = path.clone();
        async move { serve(&path, watch::channel(None).1, deps()).await }
    });
    listening(&path).await;
    for command in [
        "list_paired",
        "connect",
        "disconnect-profile",
        "connect-full",
        "disconnect",
        "remove",
    ] {
        let reply: serde_json::Value =
            serde_json::from_str(&request(&path, command).await).unwrap();
        assert_eq!(reply["ok"], false, "{command}");
        assert_eq!(reply["error"], "bluetooth unavailable", "{command}");
    }
    assert_eq!(
        request(&path, "deauth-ap").await,
        "{\"ok\":true,\"count\":2}"
    );
    assert_eq!(
        request(&path, "playback-status paused").await,
        "{\"ok\":true}"
    );
    server.abort();
    let _ = server.await;
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
#[ignore = "requires the private D-Bus session configuration"]
async fn availability_is_selected_after_the_command_line_and_profile_defaults_are_preserved() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let _service = zbus::connection::Builder::session().name("org.bluez")
        .serve_at("/org/bluez/hci2/dev_00_11_22_33_44_55", Phone(calls.clone()))
        .build().await.unwrap();
    let ready = Bluez {
        bus: zbus::Connection::session().await.unwrap(),
        adapter: "hci2".into(), bt_mac: [0; 6],
    };
    let (tx, rx) = watch::channel(None);
    let path = path("command-snapshot");
    let server = tokio::spawn({
        let path = path.clone();
        async move { serve(&path, rx, deps()).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !std::path::Path::new(&path).exists() { tokio::task::yield_now().await; }
    }).await.unwrap();
    for step in 0..2 {
        let mut reader = BufReader::new(UnixStream::connect(&path).await.unwrap());
        reader.get_mut().write_all(b"connect-full 00:11:22:33:44:55").await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while tx.receiver_count() < 2 { tokio::task::yield_now().await; }
        }).await.unwrap();
        tx.send_replace(if step == 0 { Some(ready.clone()) } else { None });
        reader.get_mut().write_all(b"\n").await.unwrap();
        let mut line = String::new();
        tokio::time::timeout(std::time::Duration::from_secs(2), reader.read_line(&mut line)).await.unwrap().unwrap();
        let reply: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["ok"], step == 0);
        drop(reader);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while tx.receiver_count() != 1 { tokio::task::yield_now().await; }
        }).await.unwrap();
    }
    tx.send_replace(Some(ready));
    for command in [
        "connect 00:11:22:33:44:55",
        "connect 00:11:22:33:44:55 CUSTOM",
        "disconnect-profile 00:11:22:33:44:55",
        "disconnect-profile 00:11:22:33:44:55 CUSTOM",
    ] {
        let reply: serde_json::Value = serde_json::from_str(&request(&path, command).await).unwrap();
        assert_eq!(reply["ok"], true, "{command}");
    }
    for (command, error) in [
        ("connect", "ConnectProfile requires a MAC"),
        ("disconnect-profile", "DisconnectProfile requires a MAC"),
        ("connect-full", "Connect requires a MAC"),
        ("disconnect", "Disconnect requires a MAC"),
        ("remove", "remove requires a MAC"),
    ] {
        let reply: serde_json::Value = serde_json::from_str(&request(&path, command).await).unwrap();
        assert_eq!(reply["error"], error);
    }
    assert_eq!(*calls.lock().unwrap(), [
        "connect",
        "profile:0000111f-0000-1000-8000-00805f9b34fb",
        "profile:CUSTOM",
        "unprofile:0000111f-0000-1000-8000-00805f9b34fb",
        "unprofile:CUSTOM",
    ]);
    server.abort();
    let _ = server.await;
    let _ = std::fs::remove_file(path);
}

struct Phone(Arc<Mutex<Vec<String>>>);

#[zbus::interface(name = "org.bluez.Device1")]
impl Phone {
    fn connect(&self) {
        self.0.lock().unwrap().push("connect".into());
    }

    fn disconnect(&self) {
        self.0.lock().unwrap().push("disconnect".into());
    }

    fn connect_profile(&self, uuid: String) {
        self.0.lock().unwrap().push(format!("profile:{uuid}"));
    }

    fn disconnect_profile(&self, uuid: String) {
        self.0.lock().unwrap().push(format!("unprofile:{uuid}"));
    }
}

/// Run with a private session bus: DBUS_SESSION_BUS_ADDRESS=... cargo test
/// -p livi-runtime --test shared_sock bluetooth_recovers -- --ignored
#[tokio::test]
#[ignore = "requires a private D-Bus session bus, not a host Bluetooth adapter"]
async fn bluetooth_recovers_without_rebinding_the_shared_socket() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let service = zbus::connection::Builder::session()
        .name("org.bluez")
        .serve_at(
            "/org/bluez/hci2/dev_00_11_22_33_44_55",
            Phone(calls.clone()),
        )
        .build()
        .await
        .unwrap();
    let bus = zbus::Connection::session().await.unwrap();
    let (tx, rx) = watch::channel(None);
    let path = path("recovery");
    let server = tokio::spawn({
        let path = path.clone();
        async move { serve(&path, rx, deps()).await }
    });
    listening(&path).await;

    assert!(
        request(&path, "connect-full 00:11:22:33:44:55")
            .await
            .contains("unavailable")
    );
    let ready = Bluez {
        bus,
        adapter: "hci2".into(),
        bt_mac: [0; 6],
    };
    tx.send(Some(ready.clone())).unwrap();
    assert_eq!(
        request(&path, "connect-full 00:11:22:33:44:55").await,
        "{\"ok\":true}"
    );
    assert_eq!(
        request(&path, "disconnect 00:11:22:33:44:55").await,
        "{\"ok\":true}"
    );
    tx.send(None).unwrap();
    assert!(
        request(&path, "connect 00:11:22:33:44:55")
            .await
            .contains("unavailable")
    );
    assert_eq!(
        request(&path, "deauth-ap").await,
        "{\"ok\":true,\"count\":2}"
    );
    tx.send(Some(ready)).unwrap();
    assert_eq!(
        request(&path, "connect 00:11:22:33:44:55").await,
        "{\"ok\":true}"
    );
    assert_eq!(
        *calls.lock().unwrap(),
        vec![
            "connect",
            "disconnect",
            "profile:0000111f-0000-1000-8000-00805f9b34fb"
        ]
    );
    server.abort();
    let _ = server.await;
    let _ = std::fs::remove_file(&path);
    drop(service);
}
