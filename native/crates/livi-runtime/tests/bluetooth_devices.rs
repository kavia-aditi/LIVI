// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use livi_runtime::livi_sock::{Bluez, Broadcaster};
use livi_runtime::shared_sock::{serve, SharedSockDeps};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::watch;

type Objects = HashMap<
    zbus::OwnedObjectPath,
    HashMap<String, HashMap<String, zbus::OwnedVariant>>,
>;

struct Manager(Vec<String>);

#[zbus::interface(name = "org.freedesktop.DBus.ObjectManager")]
impl Manager {
    fn get_managed_objects(&self) -> Objects {
        self.0.iter().map(|path| (
            zbus::OwnedObjectPath::try_from(path.clone()).unwrap(),
            HashMap::from([("org.bluez.Device1".into(), HashMap::new())]),
        )).collect()
    }
}

struct Device {
    mac: String,
    name: &'static str,
    alias: &'static str,
    connected: bool,
    paired: bool,
}

#[zbus::interface(name = "org.bluez.Device1")]
impl Device {
    #[zbus(property)]
    fn address(&self) -> &str { &self.mac }
    #[zbus(property)]
    fn name(&self) -> &str { self.name }
    #[zbus(property)]
    fn alias(&self) -> &str { self.alias }
    #[zbus(property)]
    fn connected(&self) -> bool { self.connected }
    #[zbus(property)]
    fn paired(&self) -> bool { self.paired }
    #[zbus(property)]
    fn trusted(&self) -> bool { true }
    #[zbus(property)]
    fn class(&self) -> u32 { 42 }
}

struct Adapter(Arc<Mutex<Vec<String>>>);

#[zbus::interface(name = "org.bluez.Adapter1")]
impl Adapter {
    fn remove_device(&self, path: zbus::ObjectPath<'_>) {
        self.0.lock().unwrap().push(path.to_string());
    }
}

async fn request(path: &str, command: &str) -> serde_json::Value {
    let mut reader = BufReader::new(UnixStream::connect(path).await.unwrap());
    reader.get_mut().write_all(format!("{command}\n").as_bytes()).await.unwrap();
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line))
        .await.unwrap().unwrap();
    serde_json::from_str(&line).unwrap()
}

#[tokio::test]
#[ignore = "requires the private D-Bus session configuration"]
async fn paired_devices_keep_adapter_filter_sort_fields_alias_and_remove_contracts() {
    let removed = Arc::new(Mutex::new(Vec::new()));
    let fixtures = [
        ("hci2", 1, "zulu", "", true, true),
        ("hci2", 2, "beta", "", false, true),
        ("hci2", 3, "alpha", "", false, true),
        ("hci2", 4, "ALPHA", "", false, true),
        ("hci2", 5, "", "omega", false, true),
        ("hci2", 6, "not paired", "", false, false),
        ("hci3", 7, "other adapter", "", true, true),
        ("hci20", 8, "prefix collision", "", true, true),
    ];
    let mut builder = zbus::connection::Builder::session().name("org.bluez")
        .serve_at("/org/bluez/hci2", Adapter(removed.clone()));
    let mut paths = Vec::new();
    for (adapter, index, name, alias, connected, paired) in fixtures {
        let mac = format!("00:aa:22:33:44:{index:02x}");
        let path = format!("/org/bluez/{adapter}/dev_{}", mac.replace(':', "_").to_uppercase());
        paths.push(path.clone());
        builder = builder.serve_at(path, Device { mac, name, alias, connected, paired });
    }
    let _service = builder.serve_at("/", Manager(paths)).build().await.unwrap();
    let selected = Bluez {
        bus: zbus::Connection::session().await.unwrap(), adapter: "hci2".into(), bt_mac: [0; 6],
    };
    let path = std::env::temp_dir().join(format!("livi-paired-{}", std::process::id()))
        .to_string_lossy().to_string();
    let server = tokio::spawn({
        let path = path.clone();
        async move {
            serve(&path, watch::channel(Some(selected)).1, SharedSockDeps {
                wifi_iface: "none0".into(),
                events: Broadcaster::default(),
                set_playback_status: Box::new(|_| {}),
                deauth_dongle: None,
            }).await
        }
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while !std::path::Path::new(&path).exists() { tokio::task::yield_now().await; }
    }).await.unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o666);
    let reply = request(&path, "list_paired").await;
    assert_eq!(reply["ok"], true, "{reply}");
    let devices = reply["devices"].as_array().unwrap();
    let macs: Vec<_> = devices.iter().map(|d| d["mac"].as_str().unwrap()).collect();
    assert_eq!(macs, [
        "00:AA:22:33:44:01", "00:AA:22:33:44:03", "00:AA:22:33:44:04",
        "00:AA:22:33:44:02", "00:AA:22:33:44:05",
    ]);
    assert_eq!(devices[0]["connected"], true);
    assert_eq!(devices[0]["trusted"], true);
    assert_eq!(devices[0]["class"], 42);
    assert_eq!(devices[0]["path"], "/org/bluez/hci2/dev_00_AA_22_33_44_01");
    assert_eq!(devices[4]["name"], "omega");
    assert_eq!(request(&path, "remove 00:aa:22:33:44:05").await["ok"], true);
    assert_eq!(*removed.lock().unwrap(), ["/org/bluez/hci2/dev_00_AA_22_33_44_05"]);
    server.abort();
    let _ = server.await;
    let _ = std::fs::remove_file(path);
}
