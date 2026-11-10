// SPDX-License-Identifier: GPL-3.0-or-later

use super::{WiredPhones, native_handshake};
use livi_aa::wpp;
use livi_native_handoff::config::Config;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

fn config() -> Config {
    Config::from_json(br#"{"ssid":"offline-ap","passphrase":"offline-key-only","security":"wpa2","channel":1,"bssid":"02:11:22:33:44:55","ap_ip":"192.168.1.20","port":5277,"phone_address":"02:01:02:03:04:05"}"#).unwrap()
}

fn version(instance: &str, serial: &str) -> Vec<u8> {
    fn string(tag: u8, value: &str) -> Vec<u8> {
        assert!(value.len() < 128);
        [vec![tag, value.len() as u8], value.as_bytes().to_vec()].concat()
    }
    let nested = string(10, instance);
    let body = [vec![8, 6, 16, 0, 32, 0], string(26, serial),
                vec![50, nested.len() as u8], nested].concat();
    wpp::frame(wpp::MSG_WIFI_VERSION_RESPONSE, &body)
}

async fn packet(phone: &mut DuplexStream) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut header = [0; 4];
        phone.read_exact(&mut header).await.unwrap();
        let mut body = vec![0; u16::from_be_bytes([header[0], header[1]]) as usize];
        phone.read_exact(&mut body).await.unwrap();
        [header.to_vec(), body].concat()
    }).await.unwrap()
}

fn start(wired: WiredPhones) -> (
    DuplexStream,
    tokio::task::JoinHandle<std::io::Result<wpp::Identity>>,
) {
    let (stream, phone) = tokio::io::duplex(8192);
    let worker = tokio::spawn(async move { native_handshake(stream, config(), &wired).await });
    (phone, worker)
}

async fn result(
    worker: tokio::task::JoinHandle<std::io::Result<wpp::Identity>>,
) -> std::io::Result<wpp::Identity> {
    tokio::time::timeout(Duration::from_secs(2), worker).await.unwrap().unwrap()
}

async fn closed(phone: &mut DuplexStream) {
    assert_eq!(tokio::time::timeout(Duration::from_secs(2), phone.read(&mut [0]))
        .await.unwrap().unwrap(), 0);
}

#[tokio::test]
async fn learns_real_response_labels_without_config_ids_and_never_reuses_previous_attempt() {
    // Each new owned stream must establish its own identity; no global cache.
    for (instance, serial) in [("phone-a", "serial-a"), ("phone-b", "serial-b")] {
        let (mut phone, worker) = start(WiredPhones::default());
        assert_eq!(packet(&mut phone).await, wpp::wifi_version_request(config().channel()));
        phone.write_all(&version(instance, serial)).await.unwrap();
        assert_eq!(packet(&mut phone).await, wpp::wifi_start_request("192.168.1.20", 5277));
        // An identical repeat response is allowed, not a new identity.
        phone.write_all(&version(instance, serial)).await.unwrap();
        phone.write_all(&wpp::frame(wpp::MSG_WIFI_INFO_REQUEST, &[])).await.unwrap();
        assert_eq!(packet(&mut phone).await, wpp::wifi_info_response(
            "offline-ap", "offline-key-only", "02:11:22:33:44:55",
            livi_wifi::Security::Wpa2));
        phone.write_all(&wpp::frame(wpp::MSG_WIFI_CONNECT_STATUS, &[8, 0]))
            .await.unwrap();
        let learned = result(worker).await.unwrap();
        assert_eq!((learned.instance_id.as_str(), learned.serial.as_str()), (instance, serial));
        closed(&mut phone).await;
    }
    // No previous success can make an info-first stream valid.
    let (mut phone, worker) = start(WiredPhones::default());
    packet(&mut phone).await;
    phone.write_all(&wpp::frame(wpp::MSG_WIFI_INFO_REQUEST, &[])).await.unwrap();
    assert!(result(worker).await.is_err());
    closed(&mut phone).await;
}

#[tokio::test]
async fn changing_either_learned_label_rejects_before_credentials() {
    for (instance, serial) in [("phone-b", "serial-a"), ("phone-a", "serial-b")] {
        let (mut phone, worker) = start(WiredPhones::default());
        packet(&mut phone).await;
        phone.write_all(&version("phone-a", "serial-a")).await.unwrap();
        packet(&mut phone).await;
        phone.write_all(&[
            version(instance, serial),
            wpp::frame(wpp::MSG_WIFI_INFO_REQUEST, &[]),
        ].concat()).await.unwrap();
        assert!(result(worker).await.is_err());
        closed(&mut phone).await;
    }
}

#[tokio::test]
async fn missing_or_unsafe_learned_identity_rejects_before_ap_start() {
    for response in [
        version("", "serial-a"),
        version("phone-a", ""),
        version("has space", "serial-a"),
        version("phone-a", "line\nbreak"),
        wpp::frame(wpp::MSG_WIFI_VERSION_RESPONSE, &[255]),
    ] {
        let (mut phone, worker) = start(WiredPhones::default());
        packet(&mut phone).await;
        phone.write_all(&response).await.unwrap();
        assert!(result(worker).await.is_err());
        closed(&mut phone).await;
    }
}

#[tokio::test]
async fn learned_usb_identity_suppresses_initial_and_late_credential_offers() {
    for late in [false, true] {
        let wired = WiredPhones::default();
        if !late {
            wired.set(vec!["SERIAL-A".into()]);
        }
        let (mut phone, worker) = start(wired.clone());
        packet(&mut phone).await;
        phone.write_all(&version("phone-a", "serial-a")).await.unwrap();
        if late {
            packet(&mut phone).await;
            wired.set(vec!["PHONE-A".into()]);
            phone.write_all(&wpp::frame(wpp::MSG_WIFI_INFO_REQUEST, &[])).await.unwrap();
        }
        assert!(result(worker).await.is_err());
        closed(&mut phone).await;
    }
}
