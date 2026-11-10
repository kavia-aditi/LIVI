use std::net::Ipv4Addr;
use livi_wifi::Channel;

pub struct Config {
    pub ssid: String,
    pub password: String,
    pub bssid: String,
    pub channel: Channel,
    pub ip: Ipv4Addr,
    pub port: u16,
    pub phone: String,
    pub wired_ids: Vec<String>,
}

pub fn mac(text: &str) -> Result<String, String> {
    let parts: Vec<_> = text.split(':').collect();
    if parts.len() != 6 || parts.iter().any(|p| p.len() != 2 || u8::from_str_radix(p, 16).is_err()) {
        return Err("expected a six-octet colon-separated Bluetooth/BSSID address".into());
    }
    if text == "00:00:00:00:00:00" || text.eq_ignore_ascii_case("ff:ff:ff:ff:ff:ff") {
        return Err("zero/broadcast addresses are not valid targets".into());
    }
    Ok(text.to_uppercase())
}

impl Config {
    pub fn parse(text: &str) -> Result<Self, String> {
        let v: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let string = |key: &str| -> Result<String, String> {
            v[key].as_str().filter(|s| !s.is_empty() && !s.contains('\0'))
                .map(str::to_owned).ok_or_else(|| format!("missing/invalid {key}"))
        };
        let ssid = string("ssid")?;
        if ssid.len() > 32 { return Err("SSID exceeds 32 bytes".into()); }
        let password = string("password")?;
        if !(8..=63).contains(&password.len()) || !password.is_ascii() {
            return Err("this experimental WPA2 path requires an 8–63 ASCII byte password".into());
        }
        if string("security")? != "wpa2" {
            return Err("only explicitly provisioned WPA2-Personal is supported by this experiment".into());
        }
        let channel = v["channel"].as_u64().ok_or("missing channel")?;
        if !(1..=13).contains(&channel)
            && ![36, 40, 44, 48, 149, 153, 157, 161, 165].contains(&channel)
        {
            return Err("use a locally permitted 2.4 GHz channel (1–13) or non-DFS 5 GHz channel (36, 40, 44, 48, 149, 153, 157, 161, 165)".into());
        }
        let ip: Ipv4Addr = string("mac_ipv4")?.parse().map_err(|_| "invalid mac_ipv4")?;
        if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() || ip == Ipv4Addr::BROADCAST {
            return Err("mac_ipv4 must be the Mac's reachable LAN address, not loopback/gateway".into());
        }
        let port = match v.get("port") {
            None => 5277,
            Some(port) => port.as_u64().ok_or("port must be an integer")?,
        };
        if !(1024..=65535).contains(&port) { return Err("port must be 1024–65535".into()); }
        let wired_ids = match v.get("wired_ids") {
            None => Vec::new(),
            Some(v) => v.as_array().ok_or("wired_ids must be an array")?
                .iter().map(|id| id.as_str().map(str::to_owned).ok_or("invalid wired ID"))
                .collect::<Result<Vec<_>, _>>()?,
        };
        if v["phone_not_projecting_usb"].as_bool() != Some(true) {
            return Err("confirm phone_not_projecting_usb before starting this isolated receiver".into());
        }
        Ok(Self {
            ssid, password, bssid: mac(&string("bssid")?)?,
            phone: mac(&string("phone_address")?)?,
            channel: Channel::of_number(channel as u32),
            ip, port: port as u16, wired_ids,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn valid() -> serde_json::Value {
        serde_json::json!({
            "ssid": "test-only", "password": "not-a-real-secret", "security": "wpa2",
            "bssid": "02:11:22:33:44:55", "phone_address": "02:aa:bb:cc:dd:ee",
            "channel": 36, "mac_ipv4": "192.168.10.2", "port": 5277,
            "phone_not_projecting_usb": true
        })
    }
    #[test]
    fn explicit_configuration_round_trips() {
        let c = Config::parse(&valid().to_string()).unwrap();
        assert_eq!(c.phone, "02:AA:BB:CC:DD:EE");
        assert_eq!(c.channel.freq_mhz(), 5180);
        assert_eq!(c.port, 5277);
    }
    #[test]
    fn accepts_every_2_4_ghz_channel_and_advertises_its_frequency() {
        for number in 1..=13 {
            let mut v = valid();
            v["channel"] = serde_json::json!(number);
            let c = Config::parse(&v.to_string()).unwrap();
            assert_eq!(c.channel.band, livi_wifi::Band::Ghz24);
            assert_eq!(c.channel.number, number);
            let mhz = 2407 + 5 * number;
            assert_eq!(c.channel.freq_mhz(), mhz);
            // Independent protobuf wire expectation: version 6.0, frequency field 4.
            assert_eq!(livi_aa::wpp::wifi_version_request(c.channel), vec![
                0, 7, 0, 4, 8, 6, 16, 0, 32,
                ((mhz & 0x7f) as u8) | 0x80, (mhz >> 7) as u8,
            ]);
        }
    }
    #[test]
    fn retains_every_supported_5_ghz_channel() {
        for number in [36, 40, 44, 48, 149, 153, 157, 161, 165] {
            let mut v = valid();
            v["channel"] = serde_json::json!(number);
            let c = Config::parse(&v.to_string()).unwrap();
            assert_eq!(c.channel.band, livi_wifi::Band::Ghz5);
            assert_eq!(c.channel.number, number);
            assert_eq!(c.channel.freq_mhz(), 5000 + 5 * number);
        }
    }
    #[test]
    fn rejects_invalid_channels_before_integer_narrowing() {
        for bad in [
            serde_json::json!(0), serde_json::json!(14), serde_json::json!(15),
            serde_json::json!(35), serde_json::json!(37), serde_json::json!(52),
            serde_json::json!(100), serde_json::json!(144), serde_json::json!(166),
            serde_json::json!(u32::MAX), serde_json::json!(4294967297u64),
            serde_json::json!(u64::MAX), serde_json::json!(-1),
            serde_json::json!(1.5), serde_json::json!(1.0), serde_json::json!("1"),
            serde_json::json!(true), serde_json::Value::Null,
            serde_json::json!([]), serde_json::json!({}),
        ] {
            let mut v = valid();
            v["channel"] = bad.clone();
            assert!(Config::parse(&v.to_string()).is_err(), "channel={bad}");
        }
        let mut v = valid();
        v.as_object_mut().unwrap().remove("channel");
        assert!(Config::parse(&v.to_string()).is_err());
    }
    #[test]
    fn rejects_unsafe_or_missing_configuration() {
        for (key, bad) in [
            ("ssid", serde_json::json!("")),
            ("password", serde_json::json!("short")),
            ("security", serde_json::json!("wpa3")),
            ("channel", serde_json::json!(14)),
            ("channel", serde_json::json!(52)),
            ("mac_ipv4", serde_json::json!("127.0.0.1")),
            ("phone_address", serde_json::json!("not-an-address")),
            ("bssid", serde_json::json!("00:00:00:00:00:00")),
            ("port", serde_json::json!(70000)),
            ("port", serde_json::json!("not-a-port")),
            ("phone_not_projecting_usb", serde_json::json!(false)),
        ] {
            let mut v = valid(); v[key] = bad;
            assert!(Config::parse(&v.to_string()).is_err(), "{key}");
        }
    }
    #[test]
    fn no_defaults_for_network_credentials() {
        assert!(Config::parse("{}").is_err());
        assert!(Config::parse("null").is_err());
        assert!(mac("AA:BB:CC:DD:EE").is_err());
    }
}
