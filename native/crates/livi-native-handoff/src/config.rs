// SPDX-License-Identifier: GPL-3.0-or-later

use crate::Error;
use livi_native_bluetooth::Address;
use livi_wifi::Channel;
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    ssid: String,
    passphrase: String,
    security: String,
    channel: u32,
    bssid: String,
    ap_ip: String,
    port: u16,
    phone_address: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    instance_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    serial: Option<String>,
}

/// Per-attempt memory only. Intentionally neither Clone nor Serialize.
pub struct Config {
    wire: Wire,
    phone: Address,
    ip: Ipv4Addr,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NativeBootstrapConfig([redacted])")
    }
}

pub fn address(text: &str) -> Result<Address, Error> {
    let mut bytes = [0; 6];
    let parts: Vec<_> = text.split(':').collect();
    if parts.len() != 6 {
        return Err(Error::Configuration);
    }
    for (out, part) in bytes.iter_mut().zip(parts) {
        if part.len() != 2 {
            return Err(Error::Configuration);
        }
        *out = u8::from_str_radix(part, 16).map_err(|_| Error::Configuration)?;
    }
    let address = Address(bytes);
    address
        .valid()
        .then_some(address)
        .ok_or(Error::Configuration)
}

impl Config {
    pub fn from_json(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > 2048 {
            return Err(Error::Configuration);
        }
        let wire: Wire = serde_json::from_slice(bytes).map_err(|_| Error::Configuration)?;
        let ip: Ipv4Addr = wire.ap_ip.parse().map_err(|_| Error::Configuration)?;
        let phone = address(&wire.phone_address)?;
        address(&wire.bssid)?;
        // Explicit null is not an omitted pin. Reject it rather than silently
        // converting an operator-supplied identity into discovery mode.
        let value: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|_| Error::Configuration)?;
        let pins_valid = match (&wire.instance_id, &wire.serial) {
            (None, None) => value.get("instance_id").is_none() && value.get("serial").is_none(),
            (Some(instance), Some(serial)) => valid_identity(instance, serial),
            _ => false,
        };
        if wire.ssid.is_empty()
            || wire.ssid.len() > 32
            || wire.ssid.chars().any(char::is_control)
            || !(8..=63).contains(&wire.passphrase.len())
            || !wire.passphrase.bytes().all(|b| (32..=126).contains(&b))
            || wire.security != "wpa2"
            || !matches!(
                wire.channel,
                1..=13 | 36 | 40 | 44 | 48 | 149 | 153 | 157 | 161 | 165
            )
            || !ip.is_private()
            || matches!(ip.octets()[3], 0 | 255)
            || wire.port != 5277
            || !pins_valid
        {
            return Err(Error::Configuration);
        }
        Ok(Self { wire, phone, ip })
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, Error> {
        serde_json::to_vec(&self.wire).map_err(|_| Error::Configuration)
    }

    pub fn ssid(&self) -> &str {
        &self.wire.ssid
    }

    pub fn passphrase(&self) -> &str {
        &self.wire.passphrase
    }

    pub fn channel(&self) -> Channel {
        Channel::of_number(self.wire.channel)
    }

    pub fn bssid(&self) -> String {
        self.wire.bssid.to_ascii_lowercase()
    }

    pub fn ip(&self) -> Ipv4Addr {
        self.ip
    }

    pub fn port(&self) -> u16 {
        self.wire.port
    }

    pub fn phone(&self) -> Address {
        self.phone
    }

    pub fn instance_id(&self) -> &str {
        self.wire.instance_id.as_deref().unwrap_or("")
    }

    pub fn serial(&self) -> &str {
        self.wire.serial.as_deref().unwrap_or("")
    }

    /// Checks optional pins, not authentication. The caller must additionally
    /// latch the first identity on its current owned stream and reject changes.
    pub fn matches_identity(&self, instance: &str, serial: &str) -> bool {
        valid_identity(instance, serial)
            && match (&self.wire.instance_id, &self.wire.serial) {
                (None, None) => true,
                (Some(want_instance), Some(want_serial)) => {
                    instance == want_instance && serial == want_serial
                }
                _ => false,
            }
    }
}

fn valid_identity(instance: &str, serial: &str) -> bool {
    [instance, serial].iter().all(|s| {
        !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_graphic())
    })
}

#[cfg(test)]
pub(crate) fn fixture() -> Config {
    Config::from_json(br#"{"ssid":"offline-ap","passphrase":"offline-key-only","security":"wpa2","channel":1,"bssid":"02:11:22:33:44:55","ap_ip":"192.168.1.20","port":5277,"phone_address":"02:01:02:03:04:05","instance_id":"offline-instance","serial":"offline-serial"}"#).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phone_identity_pins_are_optional_but_never_partial_null_or_invalid() {
        let mut value: serde_json::Value =
            serde_json::from_slice(&fixture().encode().unwrap()).unwrap();
        value.as_object_mut().unwrap().remove("instance_id");
        value.as_object_mut().unwrap().remove("serial");
        let cfg = Config::from_json(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(cfg.instance_id(), "");
        assert_eq!(cfg.serial(), "");
        assert!(cfg.matches_identity("phone-a", "serial-a"));
        assert!(!cfg.matches_identity("", "serial-a"));
        assert!(!cfg.matches_identity("phone-a", "has space"));
        assert!(!cfg.matches_identity(&"x".repeat(129), "serial-a"));
        let roundtrip: serde_json::Value = serde_json::from_slice(&cfg.encode().unwrap()).unwrap();
        assert_eq!(roundtrip, value);
        for pins in [
            serde_json::json!({"instance_id": "phone-a"}),
            serde_json::json!({"serial": "serial-a"}),
            serde_json::json!({"instance_id": null, "serial": null}),
            serde_json::json!({"instance_id": "", "serial": ""}),
        ] {
            let mut bad = value.clone();
            bad.as_object_mut().unwrap().extend(pins.as_object().unwrap().clone());
            assert_eq!(Config::from_json(&serde_json::to_vec(&bad).unwrap()).err(),
                       Some(Error::Configuration));
        }
        assert!(fixture().matches_identity("offline-instance", "offline-serial"));
        assert!(!fixture().matches_identity("phone-a", "serial-a"));
    }

    #[test]
    fn channel_one_and_existing_five_ghz_channels_validate() {
        let value: serde_json::Value =
            serde_json::from_slice(&fixture().encode().unwrap()).unwrap();
        for channel in [1, 13, 36, 40, 44, 48, 149, 153, 157, 161, 165] {
            let mut value = value.clone();
            value["channel"] = channel.into();
            let cfg = Config::from_json(&serde_json::to_vec(&value).unwrap()).unwrap();
            assert_eq!(cfg.channel().number, channel);
        }
        assert_eq!(fixture().channel().freq_mhz(), 2412);
    }

    #[test]
    fn invalid_config_and_unknown_fields_have_fixed_redacted_errors() {
        let value: serde_json::Value =
            serde_json::from_slice(&fixture().encode().unwrap()).unwrap();
        for (field, bad) in [
            ("ssid", serde_json::json!("")),
            ("passphrase", serde_json::json!("short")),
            ("security", serde_json::json!("open")),
            ("bssid", serde_json::json!("03:11:22:33:44:55")),
            ("phone_address", serde_json::json!("00:00:00:00:00:00")),
            ("ap_ip", serde_json::json!("127.0.0.1")),
            ("port", serde_json::json!(5288)),
            ("channel", serde_json::json!(14)),
            ("instance_id", serde_json::json!("")),
            ("serial", serde_json::json!("")),
            ("extra", serde_json::json!("offline-key-only")),
        ] {
            let mut value = value.clone();
            value[field] = bad;
            assert_eq!(
                Config::from_json(&serde_json::to_vec(&value).unwrap()).err(),
                Some(Error::Configuration)
            );
        }
        assert!(!format!("{:?}", fixture()).contains("offline"));
        assert!(!format!("{}", Error::Configuration).contains("offline"));
    }
}
