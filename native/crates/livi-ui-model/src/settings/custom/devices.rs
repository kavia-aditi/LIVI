// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

use livi_core_proto::message::Action;
use livi_core_proto::state::{DeviceStatus, DeviceView, Protocol};
use serde_json::Value;

use super::info;
use crate::i18n::Strings;
use crate::settings::view::{Device, Local, Presence, Row, RowKind, Tool};

pub fn devices(state: &Value, t: &Strings) -> Vec<Row> {
    let list: Vec<DeviceView> =
        serde_json::from_value(state["devices"].clone()).unwrap_or_default();
    let paired = macos_paired_rows(state);
    let mut rows = if list.is_empty() && paired.is_empty() {
        vec![info(
            t.t("settings.noPairedDevices", "No paired devices"),
            None,
            String::new(),
        )]
    } else {
        list.iter().map(|d| device(d, t)).collect()
    };
    rows.extend(paired);
    rows.extend(native_rows(state, &list));
    rows
}

fn macos_paired_rows(state: &Value) -> Vec<Row> {
    let inventory = &state["macosPairedDevices"];
    if inventory["loading"].as_bool() == Some(true) {
        return vec![info("Paired Bluetooth devices (macOS)".into(), None,
            "Reading local paired inventory… No connection is being attempted.".into())];
    }
    if inventory["error"].as_bool() == Some(true) {
        return vec![info("macOS paired-device inventory unavailable".into(), None,
            "Check Bluetooth access in macOS settings; this is not an empty paired list.".into()), refresh_row()];
    }
    let Some(devices) = inventory["devices"].as_array() else { return Vec::new() };
    let mut seen = std::collections::BTreeSet::new();
    let mut rows = Vec::new();
    for (index, device) in devices.iter().enumerate() {
        let Some(address) = device["address"].as_str().filter(|a| !a.is_empty()) else { continue };
        if !seen.insert(address.to_ascii_uppercase()) { continue; }
        let name = device["name"].as_str().filter(|n| !n.is_empty()).unwrap_or("Unnamed Bluetooth device");
        rows.push(Row {
            kind: RowKind::Pick,
            label: name.into(),
            value: address.into(),
            checked: state["uiSelectedPairedAddress"].as_str() == Some(address),
            enabled: true,
            local: Some(Local::SelectPaired(index)),
            ..Row::default()
        });
    }
    if !rows.is_empty() {
        rows.insert(0, info("Paired Bluetooth devices (macOS)".into(), None,
            "Paired inventory only. Select a phone to highlight it locally; this does not connect or authenticate it.".into()));
    }
    if rows.is_empty() {
        rows.push(info("No paired devices".into(), None,
            "No local paired phones were reported by macOS.".into()));
    }
    rows.push(refresh_row());
    rows
}

fn refresh_row() -> Row {
    Row {
        kind: RowKind::Button,
        label: "Local Bluetooth inventory".into(),
        button: "Refresh paired list".into(),
        enabled: true,
        local: Some(Local::RefreshPaired),
        ..Row::default()
    }
}

fn native_rows(state: &Value, list: &[DeviceView]) -> Vec<Row> {
    use livi_core_proto::native_wireless_aa::{
        NativeProjectionMilestone as M, NativeProjectionProgress as P,
        NativeWirelessAaCancel, NativeWirelessAaStatus as S,
    };
    let enabled = state["config"]["nativeWirelessAa"]["enabled"].as_bool() == Some(true);
    let status = serde_json::from_value::<S>(state["system"]["nativeWirelessAa"].clone())
        .unwrap_or(if enabled { S::Unavailable } else { S::Disabled });
    let progress = serde_json::from_value::<P>(state["system"]["nativeProjection"].clone()).ok();
    let presented = match (status, progress) {
        (S::Connected { attempt_id }, Some(P::Presented { owner, .. }))
            if attempt_id == owner.attempt_id
                && state["front"]["main"].as_str() == Some("projection")
                && state["sessions"]["active"].as_str() == Some("androidauto") =>
        {
            let id = format!("native-aa/{}/{}/{}/{}", owner.runtime_id, owner.attempt_id,
                owner.generation, owner.session_id.unwrap_or(0));
            list.iter().any(|d| d.id == id && d.protocol == Some(Protocol::Androidauto)
                && d.status == DeviceStatus::Active && d.session.is_some())
        }
        _ => false,
    };
    let text = match status {
        S::Connecting { .. } => "Connecting",
        S::Connected { .. } if presented => "Projection visible",
        S::Connected { .. } => "Connected; projection not visible",
        S::Cancelled { .. } => "Cancelled",
        S::Failed { .. } => "Failed — connection stopped",
        _ if enabled => "Unavailable — hardware authorization is closed",
        _ => "Disabled",
    };
    let mut rows = vec![info("Native wireless Android Auto".into(), None, text.into())];
    let attempt = match status {
        S::Connecting { attempt_id } | S::Connected { attempt_id } => Some(attempt_id),
        _ => None,
    };
    if let Some(attempt_id) = attempt.filter(|id| *id != 0) {
        rows.push(Row {
            kind: RowKind::Button, label: "Native wireless Android Auto".into(),
            button: "Cancel / Disconnect".into(), enabled: true,
            action: Some(Action::NativeWirelessAaCancel(NativeWirelessAaCancel { attempt_id })),
            ..Row::default()
        });
    } else {
        for d in list.iter().filter(|d| d.protocol == Some(Protocol::Androidauto) && !d.id.starts_with("native-aa/")) {
            rows.push(Row {
                kind: RowKind::Button, label: d.name.clone().unwrap_or_else(|| "Android device".into()),
                button: "Connect native".into(), enabled: false,
                action: None,
                ..Row::default()
            });
        }
    }
    let current = match (status, progress) {
        (S::Connecting { attempt_id } | S::Connected { attempt_id },
            Some(P::Milestone { owner, milestone })) if attempt_id == owner.attempt_id => Some(milestone),
        _ => None,
    };
    let failure = match (status, progress) {
        (S::Failed { code, .. }, _) => Some(format!("Failed: {code:?}. Connection stopped; no automatic retry.")),
        (S::Connecting { attempt_id } | S::Connected { attempt_id },
            Some(P::Failed { owner, code })) if attempt_id == owner.attempt_id =>
            Some(format!("Failed: {code:?}. No verified projection; connection security remains enforced.")),
        _ => None,
    };
    if let Some(failure) = &failure {
        let mut row = info("Connection error".into(), None, failure.clone());
        row.alert = true;
        rows.push(row);
    }
    if attempt.is_none() {
        rows.push(info("Connection unavailable".into(), None,
            "Native connection is not available in this build. Selecting a paired phone does not start RFCOMM, WPP or TCP. Hardware authorization and identity verification remain enforced.".into()));
    }
    let aa = presented || matches!(current, Some(M::AaRunning | M::DecodedBgra));
    let video = presented || current == Some(M::DecodedBgra);
    let history = serde_json::from_value::<Vec<M>>(state["uiNativeObserved"].clone())
        .unwrap_or_default();
    for (label, observed) in [
        ("Discovery", current == Some(M::Discovery) || history.contains(&M::Discovery)),
        ("Pairing / RFCOMM", current == Some(M::Rfcomm) || history.contains(&M::Rfcomm)),
        ("WPP", current == Some(M::Wpp) || history.contains(&M::Wpp)),
        ("Waiting for TCP", current == Some(M::WaitingTcp) || history.contains(&M::WaitingTcp)),
        ("AA established", aa),
        ("First video received", video),
        ("Projection visible", presented),
    ] {
        let value = if observed {
            match current {
                Some(M::Discovery) if label == "Discovery" => "Discovering",
                Some(M::WaitingTcp) if label == "Waiting for TCP" => "Waiting for TCP",
                _ => "Observed",
            }
        } else if failure.is_some() {
            "Stopped — see connection error"
        } else if attempt.is_none() {
            "Not started — native connection unavailable"
        } else {
            "Not reported for the current attempt"
        };
        rows.push(info(label.into(), None, value.into()));
    }
    rows
}

fn device(d: &DeviceView, t: &Strings) -> Row {
    let presence = match d.status {
        DeviceStatus::Active => Presence::Active,
        DeviceStatus::Available => Presence::Available,
        DeviceStatus::Offline => Presence::Offline,
    };
    let selectable = d.session.is_some() && presence != Presence::Offline;
    let label = [&d.name, &d.model]
        .into_iter()
        .flatten()
        .find(|s| !s.is_empty())
        .cloned()
        .unwrap_or_else(|| d.id.clone());
    Row {
        kind: RowKind::Device,
        label,
        value: match presence {
            Presence::Active => "Connected · selected session",
            Presence::Available if selectable => "Available · select session",
            Presence::Available => "Available · no session",
            Presence::Offline => "Offline",
        }.into(),
        enabled: selectable,
        dimmed: presence == Presence::Offline,
        action: selectable.then(|| Action::SelectDevice { id: d.id.clone() }),
        local: selectable.then_some(Local::ToProjection),
        device: Some(Device {
            protocol_icon: match d.protocol {
                Some(Protocol::Carplay) => "PhoneIphone",
                Some(Protocol::Androidauto) => "Android",
                None => "DirectionsCar",
            },
            transport_icon: match d.last_transport.as_deref() {
                Some("usb") => Some("CableOutlined"),
                Some("wifi") => Some("WifiOutlined"),
                _ => None,
            },
            battery: d.battery_level.map(|l| {
                (
                    l.round().clamp(0.0, 100.0) as u8,
                    d.battery_charging == Some(true),
                )
            }),
            signal: d.signal_strength.map(|s| s.round().clamp(0.0, 5.0) as u8),
            presence,
        }),
        tools: vec![Tool {
            icon: "Close",
            label: if d.id.starts_with("native-aa/") { "Disconnect native session".into() }
                else { t.t("settings.forgetDevice", "Delete device") },
            on: false,
            tint: None,
            enabled: true,
            switch: false,
            action: Action::ForgetDevice { id: d.id.clone() },
        }],
        ..Row::default()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn only_devices_with_a_session_can_be_picked() {
        let s = json!({ "devices": [
            { "id": "a", "name": "iPhone", "protocol": "carplay", "lastTransport": "wifi",
              "status": "active", "batteryLevel": 54.4, "batteryCharging": true, "signalStrength": 3, "session": 1 },
            { "id": "b", "model": "Pixel 8", "protocol": "androidauto", "status": "offline", "session": 2 },
            { "id": "c", "status": "available" }
        ] });
        let rows = devices(&s, &Strings::new("en"));
        assert_eq!(rows[0].label, "iPhone");
        assert!(rows[0].enabled);
        let d = rows[0].device.as_ref().unwrap();
        assert_eq!(d.battery, Some((54, true)));
        assert_eq!(d.transport_icon, Some("WifiOutlined"));
        assert_eq!(rows[1].label, "Pixel 8");
        assert!(!rows[1].enabled && rows[1].dimmed);
        assert_eq!(rows[2].label, "c");
        assert!(rows[2].action.is_none());
    }

    #[test]
    fn no_devices_says_so() {
        let rows = devices(&json!({}), &Strings::new("en"));
        assert_eq!(rows[0].kind, RowKind::Info);
    }

    #[test]
    fn macos_paired_inventory_displays_name_and_address_without_actions_or_success() {
        let state = json!({"macosPairedDevices":{"devices":[
            {"name":"Offline phone","address":"02:00:00:00:00:02"},
            {"name":"Duplicate","address":"02:00:00:00:00:02"},
            {"name":"","address":"02:00:00:00:00:03"}
        ]}});
        let rows = devices(&state, &Strings::new("en"));
        assert!(!rows.iter().any(|r| r.label == "No paired devices"));
        let phone = rows.iter().find(|r| r.label == "Offline phone").unwrap();
        assert_eq!(phone.value, "02:00:00:00:00:02");
        for row in macos_paired_rows(&state) {
            assert!(matches!(row.kind, RowKind::Info | RowKind::Pick | RowKind::Button));
            assert!(row.action.is_none() && row.tools.is_empty());
            if row.kind == RowKind::Pick {
                assert!(matches!(row.local, Some(Local::SelectPaired(_))));
                assert!(!row.checked);
            }
            assert!(row.device.is_none());
        }
        assert!(!rows.iter().any(|r| r.label == "Duplicate"));
        assert!(rows.iter().any(|r| r.label == "Unnamed Bluetooth device"));
        assert_eq!(rows.iter().filter(|r| r.value == "Not started — native connection unavailable").count(), 7);
    }

    #[test]
    fn inventory_empty_failure_and_absence_are_distinct_and_keep_legacy_rows() {
        let t = Strings::new("en");
        let empty = devices(&json!({"macosPairedDevices":{"devices":[]}}), &t);
        assert_eq!(empty[0].label, "No paired devices");
        let failed = devices(&json!({"macosPairedDevices":{"error":true}}), &t);
        assert_eq!(failed[0].label, "macOS paired-device inventory unavailable");
        assert!(!failed.iter().any(|r| r.label == "No paired devices"));
        let legacy = json!({"devices":[{"id":"known","name":"Known projection phone","status":"active","session":1}]});
        let before = devices(&legacy, &t);
        let mut supplemented = legacy.clone();
        supplemented["macosPairedDevices"] = json!({"devices":[{"name":"Local phone","address":"02:00:00:00:00:02"}]});
        let after = devices(&supplemented, &t);
        assert_eq!(before[0], after[0]);
        assert_eq!(legacy["devices"], supplemented["devices"]);
    }

    #[test]
    fn transport_and_decode_are_not_projection_success() {
        for (milestone, video) in [("tcp", false), ("tls", false), ("aaRunning", false), ("decodedBgra", true)] {
            let s = json!({"config":{"nativeWirelessAa":{"enabled":true}},
                "system":{"nativeWirelessAa":{"phase":"connected","attemptId":2},
                "nativeProjection":{"phase":"milestone","owner":{"runtimeId":1,"attemptId":2,
                    "generation":3,"sessionId":4,"connectionId":5,"playerId":6},"milestone":milestone}}});
            let rows = devices(&s, &Strings::new("en"));
            assert_eq!(rows.iter().find(|r| r.label == "Projection visible").unwrap().value, "Not reported for the current attempt");
            assert_eq!(rows.iter().find(|r| r.label == "First video received").unwrap().value,
                if video { "Observed" } else { "Not reported for the current attempt" });
            assert!(rows.iter().any(|r| matches!(&r.action,
                Some(Action::NativeWirelessAaCancel(c)) if c.attempt_id == 2)));
        }
    }

    #[test]
    fn native_connect_is_explicit_and_does_not_replace_legacy_selection() {
        let s = json!({"devices":[{"id":"a","protocol":"androidauto","status":"available","session":1}],
            "config":{"nativeWirelessAa":{"enabled":true}}});
        let rows = devices(&s, &Strings::new("en"));
        assert!(matches!(&rows[0].action, Some(Action::SelectDevice { id }) if id == "a"));
        let connect = rows.iter().find(|r| r.button == "Connect native").unwrap();
        assert!(!connect.enabled && connect.action.is_none());
        assert_eq!(rows.iter().filter(|r| r.value == "Not started — native connection unavailable").count(), 7);
    }

    #[test]
    fn ordinary_hardware_closed_disables_connect_for_every_published_status() {
        for phase in ["disabled", "unavailable", "idle", "cancelled", "failed"] {
            for enabled in [false, true] {
                let status = match phase {
                    "cancelled" => json!({"phase":phase,"attemptId":2}),
                    "failed" => json!({"phase":phase,"attemptId":2,"code":"unavailable"}),
                    _ => json!({"phase":phase}),
                };
                let s = json!({"devices":[{"id":"a","protocol":"androidauto","status":"available"}],
                    "config":{"nativeWirelessAa":{"enabled":enabled}},
                    "system":{"nativeWirelessAa":status}});
                let rows = devices(&s, &Strings::new("en"));
                let connect = rows.iter().find(|r| r.button == "Connect native").unwrap();
                assert!(!connect.enabled && connect.action.is_none());
            }
        }
    }

    #[test]
    fn presentation_receipt_must_match_connected_active_native_device_and_front() {
        let valid = json!({"config":{"nativeWirelessAa":{"enabled":true}},
            "front":{"main":"projection"},"sessions":{"active":"androidauto"},
            "devices":[{"id":"native-aa/1/2/3/4","protocol":"androidauto","status":"active","session":1}],
            "system":{"nativeWirelessAa":{"phase":"connected","attemptId":2},
                "nativeProjection":{"phase":"presented","owner":{"runtimeId":1,"attemptId":2,
                    "generation":3,"sessionId":4,"connectionId":5,"playerId":6},
                    "frameId":7,"visibilityEpoch":8,"presentedTime":1.0}}});
        let visible = |s: &Value| devices(s, &Strings::new("en")).iter()
            .find(|r| r.label == "Projection visible").unwrap().value == "Observed";
        assert!(visible(&valid));
        for (pointer, value) in [
            ("/front/main", json!("livi")), ("/sessions/active", json!("carplay")),
            ("/system/nativeWirelessAa/attemptId", json!(9)),
            ("/devices/0/id", json!("native-aa/1/2/9/4")),
            ("/devices/0/status", json!("available")), ("/devices/0/session", Value::Null),
            ("/system/nativeProjection/owner/sessionId", json!(9)),
            ("/system/nativeProjection/visibilityEpoch", json!(0)),
            ("/system/nativeProjection/phase", json!("invalidated")),
        ] {
            let mut stale = valid.clone();
            *stale.pointer_mut(pointer).unwrap() = value;
            assert!(!visible(&stale), "{pointer}");
        }
    }

    #[test]
    fn paired_selection_is_not_a_connection_and_failure_is_actionable_information() {
        let mut s = json!({"macosPairedDevices":{"devices":[
            {"name":"Phone","address":"02:00:00:00:00:02"}]},
            "uiSelectedPairedAddress":"02:00:00:00:00:02",
            "system":{"nativeWirelessAa":{"phase":"failed","attemptId":2,"code":"timedOut"}}});
        let rows = devices(&s, &Strings::new("en"));
        let selected = rows.iter().find(|r| r.kind == RowKind::Pick).unwrap();
        assert!(selected.checked && selected.action.is_none());
        assert!(rows.iter().any(|r| r.alert && r.value.contains("TimedOut")));
        assert!(!rows.iter().any(|r| r.value == "Observed"));
        s["uiSelectedPairedAddress"] = json!("02:00:00:00:00:03");
        assert!(!devices(&s, &Strings::new("en")).iter().any(|r| r.checked));
    }

    #[test]
    fn stale_milestone_and_projection_failure_never_report_connection_success() {
        let mut s = json!({"system":{
            "nativeWirelessAa":{"phase":"connecting","attemptId":9},
            "nativeProjection":{"phase":"milestone",
                "owner":{"runtimeId":1,"attemptId":2,"generation":3},"milestone":"wpp"}}});
        assert!(!devices(&s, &Strings::new("en")).iter().any(|r| r.value == "Observed"));
        s["system"]["nativeWirelessAa"]["attemptId"] = json!(2);
        s["system"]["nativeProjection"] = json!({"phase":"failed",
            "owner":{"runtimeId":1,"attemptId":2,"generation":3},"code":"admissionFailed"});
        let rows = devices(&s, &Strings::new("en"));
        assert!(rows.iter().any(|r| r.alert && r.value.contains("AdmissionFailed")));
        assert!(!rows.iter().any(|r| r.value == "Observed"));
    }
}
