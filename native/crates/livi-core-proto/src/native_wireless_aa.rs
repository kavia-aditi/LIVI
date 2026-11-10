// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

//! Native wireless Android Auto contracts only; no adapter, credentials, or I/O.
//! The enable flag records intent, not runtime availability. Stage 1 cannot connect.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const INVALID_CONTRACT: &str = "invalid native wireless Android Auto contract";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", try_from = "Value")]
pub struct NativeWirelessAaConfig {
    pub enabled: bool,
}

/// Public status contains only closed codes and opaque attempt numbers.
/// `Connected` is a future transport status, not proof of projection/video readiness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "camelCase", try_from = "Value")]
pub enum NativeWirelessAaStatus {
    #[default]
    Disabled,
    Unavailable,
    Idle,
    #[serde(rename_all = "camelCase")]
    Connecting {
        attempt_id: u64,
    },
    #[serde(rename_all = "camelCase")]
    Connected {
        attempt_id: u64,
    },
    #[serde(rename_all = "camelCase")]
    Cancelled {
        attempt_id: u64,
    },
    #[serde(rename_all = "camelCase")]
    Failed {
        attempt_id: u64,
        code: NativeWirelessAaFailure,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", try_from = "Value")]
pub enum NativeWirelessAaFailure {
    Disabled,
    Unavailable,
    Busy,
    DeviceUnavailable,
    TimedOut,
    TransportFailed,
}

/// References an existing device identifier; never carries Wi-Fi credentials.
/// Keep identifiers out of Debug output, even if a caller supplies arbitrary text.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", try_from = "Value")]
pub struct NativeWirelessAaConnect {
    pub device_id: String,
}

impl std::fmt::Debug for NativeWirelessAaConnect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeWirelessAaConnect")
            .field("device_id", &"[redacted]")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", try_from = "Value")]
pub struct NativeWirelessAaCancel {
    pub attempt_id: u64,
}

// Validate without exposing serde's value/unknown-field diagnostics to the
// existing frame logger. No rejected field names or values escape this module.
fn fields<'a>(value: &'a Value, allowed: &[&str]) -> Result<&'a Map<String, Value>, &'static str> {
    let object = value.as_object().ok_or(INVALID_CONTRACT)?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(INVALID_CONTRACT);
    }
    Ok(object)
}

fn attempt_id(object: &Map<String, Value>) -> Result<u64, &'static str> {
    object
        .get("attemptId")
        .and_then(Value::as_u64)
        .ok_or(INVALID_CONTRACT)
}

impl TryFrom<Value> for NativeWirelessAaConfig {
    type Error = &'static str;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        let object = fields(&value, &["enabled"])?;
        let enabled = match object.get("enabled") {
            None => false,
            Some(Value::Bool(enabled)) => *enabled,
            _ => return Err(INVALID_CONTRACT),
        };
        Ok(Self { enabled })
    }
}

impl TryFrom<Value> for NativeWirelessAaConnect {
    type Error = &'static str;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        let object = fields(&value, &["deviceId"])?;
        let device_id = object
            .get("deviceId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or(INVALID_CONTRACT)?
            .to_string();
        Ok(Self { device_id })
    }
}

impl TryFrom<Value> for NativeWirelessAaCancel {
    type Error = &'static str;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        Ok(Self {
            attempt_id: attempt_id(fields(&value, &["attemptId"])?)?,
        })
    }
}

impl TryFrom<Value> for NativeWirelessAaFailure {
    type Error = &'static str;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        match value.as_str() {
            Some("disabled") => Ok(Self::Disabled),
            Some("unavailable") => Ok(Self::Unavailable),
            Some("busy") => Ok(Self::Busy),
            Some("deviceUnavailable") => Ok(Self::DeviceUnavailable),
            Some("timedOut") => Ok(Self::TimedOut),
            Some("transportFailed") => Ok(Self::TransportFailed),
            _ => Err(INVALID_CONTRACT),
        }
    }
}

impl TryFrom<Value> for NativeWirelessAaStatus {
    type Error = &'static str;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        let phase = value
            .get("phase")
            .and_then(Value::as_str)
            .ok_or(INVALID_CONTRACT)?;
        match phase {
            "disabled" | "unavailable" | "idle" => {
                fields(&value, &["phase"])?;
                Ok(match phase {
                    "disabled" => Self::Disabled,
                    "unavailable" => Self::Unavailable,
                    _ => Self::Idle,
                })
            }
            "connecting" | "connected" | "cancelled" => {
                let attempt_id = attempt_id(fields(&value, &["phase", "attemptId"])?)?;
                Ok(match phase {
                    "connecting" => Self::Connecting { attempt_id },
                    "connected" => Self::Connected { attempt_id },
                    _ => Self::Cancelled { attempt_id },
                })
            }
            "failed" => {
                let object = fields(&value, &["phase", "attemptId", "code"])?;
                Ok(Self::Failed {
                    attempt_id: attempt_id(object)?,
                    code: NativeWirelessAaFailure::try_from(
                        object.get("code").ok_or(INVALID_CONTRACT)?.clone(),
                    )?,
                })
            }
            _ => Err(INVALID_CONTRACT),
        }
    }
}

/// Projection progress is separate from NativeWirelessAaStatus::Connected.
/// IDs are opaque correlation values, not credentials or phone identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", try_from = "Value")]
pub struct NativeProjectionOwner {
    pub runtime_id: u64,
    pub attempt_id: u64,
    pub generation: u64,
    pub session_id: Option<u64>,
    pub connection_id: Option<u64>,
    pub player_id: Option<u64>,
}

impl NativeProjectionOwner {
    fn valid(self) -> bool {
        self.runtime_id != 0 && self.attempt_id != 0 && self.generation != 0
            && self.session_id != Some(0) && self.connection_id != Some(0)
            && self.player_id != Some(0)
            && (self.connection_id.is_none() || self.session_id.is_some())
            && (self.player_id.is_none() || self.connection_id.is_some())
    }

    fn render_owned(self) -> bool {
        self.valid() && self.player_id.is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NativeProjectionMilestone {
    Discovery,
    Rfcomm,
    Wpp,
    WaitingTcp,
    Tcp,
    Tls,
    AaRunning,
    /// Appsink delivery after BGRA conversion; not decoder execution latency.
    DecodedBgra,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NativeProjectionFailure {
    Unsupported,
    WppFailed,
    AdmissionFailed,
    TlsFailed,
    DecodeFailed,
    RenderFailed,
    TimedOut,
    Disconnected,
    Backpressure,
}

/// A snapshot, not an authorization token or automatic state transition.
/// Publishers must validate current ownership and observed milestone ordering.
/// Presented may originate only from the exact-frame renderer receipt gate;
/// successful deserialization alone is never proof of physical presentation.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "camelCase", try_from = "Value")]
pub enum NativeProjectionProgress {
    #[default]
    Disabled,
    Idle,
    #[serde(rename_all = "camelCase")]
    Milestone { owner: NativeProjectionOwner, milestone: NativeProjectionMilestone },
    #[serde(rename_all = "camelCase")]
    Presented {
        owner: NativeProjectionOwner,
        frame_id: u64,
        visibility_epoch: u64,
        presented_time: f64,
    },
    #[serde(rename_all = "camelCase")]
    Invalidated { owner: NativeProjectionOwner, visibility_epoch: u64 },
    Cancelled { owner: NativeProjectionOwner },
    Failed { owner: NativeProjectionOwner, code: NativeProjectionFailure },
}

impl TryFrom<Value> for NativeProjectionOwner {
    type Error = &'static str;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        let object = fields(&value, &[
            "runtimeId", "attemptId", "generation", "sessionId", "connectionId", "playerId",
        ])?;
        let id = |key: &str| object.get(key).and_then(Value::as_u64).ok_or(INVALID_CONTRACT);
        let optional = |key: &str| match object.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value.as_u64().map(Some).ok_or(INVALID_CONTRACT),
        };
        let owner = Self {
            runtime_id: id("runtimeId")?, attempt_id: id("attemptId")?,
            generation: id("generation")?, session_id: optional("sessionId")?,
            connection_id: optional("connectionId")?, player_id: optional("playerId")?,
        };
        owner.valid().then_some(owner).ok_or(INVALID_CONTRACT)
    }
}

impl TryFrom<Value> for NativeProjectionProgress {
    type Error = &'static str;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        let phase = value.get("phase").and_then(Value::as_str).ok_or(INVALID_CONTRACT)?;
        let allowed: &[&str] = match phase {
            "disabled" | "idle" => &["phase"],
            "milestone" => &["phase", "owner", "milestone"],
            "presented" => &["phase", "owner", "frameId", "visibilityEpoch", "presentedTime"],
            "invalidated" => &["phase", "owner", "visibilityEpoch"],
            "cancelled" => &["phase", "owner"],
            "failed" => &["phase", "owner", "code"],
            _ => return Err(INVALID_CONTRACT),
        };
        let object = fields(&value, allowed)?;
        if phase == "disabled" { return Ok(Self::Disabled); }
        if phase == "idle" { return Ok(Self::Idle); }
        let owner = NativeProjectionOwner::try_from(object.get("owner").ok_or(INVALID_CONTRACT)?.clone())?;
        let nonzero = |key: &str| object.get(key).and_then(Value::as_u64)
            .filter(|id| *id != 0).ok_or(INVALID_CONTRACT);
        match phase {
            "milestone" => {
                let milestone = match object.get("milestone").and_then(Value::as_str) {
                    Some("discovery") => NativeProjectionMilestone::Discovery,
                    Some("rfcomm") => NativeProjectionMilestone::Rfcomm,
                    Some("wpp") => NativeProjectionMilestone::Wpp,
                    Some("waitingTcp") => NativeProjectionMilestone::WaitingTcp,
                    Some("tcp") => NativeProjectionMilestone::Tcp,
                    Some("tls") => NativeProjectionMilestone::Tls,
                    Some("aaRunning") => NativeProjectionMilestone::AaRunning,
                    Some("decodedBgra") => NativeProjectionMilestone::DecodedBgra,
                    _ => return Err(INVALID_CONTRACT),
                };
                if matches!(milestone, NativeProjectionMilestone::Tls | NativeProjectionMilestone::AaRunning)
                    && owner.session_id.is_none()
                    || milestone == NativeProjectionMilestone::DecodedBgra && !owner.render_owned()
                {
                    return Err(INVALID_CONTRACT);
                }
                Ok(Self::Milestone { owner, milestone })
            }
            "presented" => {
                let time = object.get("presentedTime").and_then(Value::as_f64)
                    .filter(|t| t.is_finite() && *t > 0.0).ok_or(INVALID_CONTRACT)?;
                if !owner.render_owned() { return Err(INVALID_CONTRACT); }
                Ok(Self::Presented { owner, frame_id: nonzero("frameId")?,
                    visibility_epoch: nonzero("visibilityEpoch")?, presented_time: time })
            }
            "invalidated" => {
                if !owner.render_owned() { return Err(INVALID_CONTRACT); }
                Ok(Self::Invalidated { owner, visibility_epoch: nonzero("visibilityEpoch")? })
            }
            "cancelled" => Ok(Self::Cancelled { owner }),
            "failed" => {
                let code = match object.get("code").and_then(Value::as_str) {
                    Some("unsupported") => NativeProjectionFailure::Unsupported,
                    Some("wppFailed") => NativeProjectionFailure::WppFailed,
                    Some("admissionFailed") => NativeProjectionFailure::AdmissionFailed,
                    Some("tlsFailed") => NativeProjectionFailure::TlsFailed,
                    Some("decodeFailed") => NativeProjectionFailure::DecodeFailed,
                    Some("renderFailed") => NativeProjectionFailure::RenderFailed,
                    Some("timedOut") => NativeProjectionFailure::TimedOut,
                    Some("disconnected") => NativeProjectionFailure::Disconnected,
                    Some("backpressure") => NativeProjectionFailure::Backpressure,
                    _ => return Err(INVALID_CONTRACT),
                };
                Ok(Self::Failed { owner, code })
            }
            _ => Err(INVALID_CONTRACT),
        }
    }
}

#[cfg(test)]
mod projection_tests {
    use super::*;
    use serde_json::json;

    fn owner() -> NativeProjectionOwner {
        NativeProjectionOwner { runtime_id: 1, attempt_id: 2, generation: 3,
            session_id: Some(4), connection_id: Some(5), player_id: Some(6) }
    }

    #[test]
    fn projection_progress_round_trips_independently_of_transport_connected() {
        let owner = owner();
        let mut values = vec![
            NativeProjectionProgress::Disabled, NativeProjectionProgress::Idle,
            NativeProjectionProgress::Presented { owner, frame_id: 7, visibility_epoch: 8, presented_time: 1.0 },
            NativeProjectionProgress::Invalidated { owner, visibility_epoch: 9 },
            NativeProjectionProgress::Cancelled { owner },
        ];
        for milestone in [NativeProjectionMilestone::Discovery, NativeProjectionMilestone::Rfcomm,
            NativeProjectionMilestone::Wpp, NativeProjectionMilestone::WaitingTcp,
            NativeProjectionMilestone::Tcp, NativeProjectionMilestone::Tls,
            NativeProjectionMilestone::AaRunning, NativeProjectionMilestone::DecodedBgra] {
            values.push(NativeProjectionProgress::Milestone { owner, milestone });
        }
        for code in [NativeProjectionFailure::Unsupported, NativeProjectionFailure::WppFailed,
            NativeProjectionFailure::AdmissionFailed, NativeProjectionFailure::TlsFailed,
            NativeProjectionFailure::DecodeFailed, NativeProjectionFailure::RenderFailed,
            NativeProjectionFailure::TimedOut, NativeProjectionFailure::Disconnected,
            NativeProjectionFailure::Backpressure] {
            values.push(NativeProjectionProgress::Failed { owner, code });
        }
        for value in values {
            let wire = serde_json::to_value(value).unwrap();
            assert_eq!(serde_json::from_value::<NativeProjectionProgress>(wire).unwrap(), value);
        }
        assert_eq!(serde_json::to_value(NativeWirelessAaStatus::Connected { attempt_id: 7 }).unwrap(),
            json!({"phase":"connected","attemptId":7}));
    }

    #[test]
    fn progress_requires_render_ownership_positive_time_and_exact_fields() {
        let valid = json!({"phase":"presented","owner":owner(),"frameId":7,
            "visibilityEpoch":8,"presentedTime":1.0});
        for (key, bad) in [("frameId", json!(0)), ("frameId", json!(-1)),
            ("visibilityEpoch", json!(0)), ("presentedTime", json!(0)),
            ("presentedTime", json!(-1)), ("presentedTime", Value::Null),
            ("presentedTime", json!("1")), ("phase", json!("connected"))] {
            let mut wire = valid.clone(); wire[key] = bad;
            assert!(serde_json::from_value::<NativeProjectionProgress>(wire).is_err());
        }
        for key in ["runtimeId", "attemptId", "generation", "sessionId", "connectionId", "playerId"] {
            for bad in [json!(0), Value::Null] {
                let mut wire = valid.clone(); wire["owner"][key] = bad;
                assert!(serde_json::from_value::<NativeProjectionProgress>(wire).is_err());
            }
        }
        let mut wire = valid; wire["password"] = json!("native-secret-sentinel");
        let error = serde_json::from_value::<NativeProjectionProgress>(wire).unwrap_err();
        assert!(!error.to_string().contains("native-secret-sentinel"));
    }

    #[test]
    fn early_progress_does_not_require_a_render_owner_but_late_progress_does() {
        let owner = json!({"runtimeId":1,"attemptId":2,"generation":3});
        for milestone in ["rfcomm", "wpp", "tcp"] {
            assert!(serde_json::from_value::<NativeProjectionProgress>(
                json!({"phase":"milestone","owner":owner,"milestone":milestone})).is_ok());
        }
        for milestone in ["tls", "aaRunning", "decodedBgra", "presented"] {
            assert!(serde_json::from_value::<NativeProjectionProgress>(
                json!({"phase":"milestone","owner":owner,"milestone":milestone})).is_err());
        }
        for wire in [
            json!({"phase":"failed","owner":owner,"code":"native-secret-sentinel"}),
            json!({"phase":"milestone","owner":owner,"milestone":"native-secret-sentinel"}),
            json!({"phase":"cancelled","owner":{"runtimeId":1,"attemptId":2,"generation":3,
                "password":"native-secret-sentinel"}}),
        ] {
            let error = serde_json::from_value::<NativeProjectionProgress>(wire).unwrap_err();
            assert!(!error.to_string().contains("native-secret-sentinel"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_defaults_are_disabled_and_contain_no_credentials() {
        assert!(!NativeWirelessAaConfig::default().enabled);
        assert_eq!(
            serde_json::to_value(NativeWirelessAaConfig::default()).unwrap(),
            json!({"enabled": false})
        );
        assert_eq!(
            serde_json::from_value::<NativeWirelessAaConfig>(json!({})).unwrap(),
            NativeWirelessAaConfig::default()
        );
        assert_eq!(
            serde_json::to_value(NativeWirelessAaStatus::default()).unwrap(),
            json!({"phase": "disabled"})
        );
    }

    #[test]
    fn native_contracts_reject_credential_and_free_form_error_fields() {
        for field in [
            "wifiPassword",
            "password",
            "psk",
            "credentials",
            "error",
            "native-secret-sentinel",
        ] {
            let secret = "native-secret-sentinel";
            for (kind, mut value) in [
                ("config", json!({"enabled": true})),
                ("connect", json!({"deviceId": "device-1"})),
                ("cancel", json!({"attemptId": 7})),
                ("status", json!({"phase": "disabled"})),
            ] {
                value[field] = json!(secret);
                let error = match kind {
                    "config" => {
                        serde_json::from_value::<NativeWirelessAaConfig>(value).unwrap_err()
                    }
                    "connect" => {
                        serde_json::from_value::<NativeWirelessAaConnect>(value).unwrap_err()
                    }
                    "cancel" => {
                        serde_json::from_value::<NativeWirelessAaCancel>(value).unwrap_err()
                    }
                    _ => serde_json::from_value::<NativeWirelessAaStatus>(value).unwrap_err(),
                };
                assert!(!error.to_string().contains(secret));
                assert!(error.to_string().contains(INVALID_CONTRACT));
            }
        }
    }

    #[test]
    fn malformed_native_values_have_non_echoing_errors() {
        let secret = "native-secret-sentinel";
        let errors = [
            serde_json::from_value::<NativeWirelessAaConfig>(json!({"enabled": secret}))
                .unwrap_err(),
            serde_json::from_value::<NativeWirelessAaConnect>(json!({"deviceId": [secret]}))
                .unwrap_err(),
            serde_json::from_value::<NativeWirelessAaCancel>(json!({"attemptId": secret}))
                .unwrap_err(),
            serde_json::from_value::<NativeWirelessAaStatus>(json!({"phase": secret})).unwrap_err(),
            serde_json::from_value::<NativeWirelessAaStatus>(json!({
                "phase": "failed", "attemptId": 7, "code": secret
            }))
            .unwrap_err(),
        ];
        for error in errors {
            assert!(!error.to_string().contains(secret));
            assert!(error.to_string().contains(INVALID_CONTRACT));
        }
    }

    #[test]
    fn native_status_round_trips_closed_codes_only() {
        for status in [
            NativeWirelessAaStatus::Disabled,
            NativeWirelessAaStatus::Unavailable,
            NativeWirelessAaStatus::Idle,
            NativeWirelessAaStatus::Connecting { attempt_id: 7 },
            NativeWirelessAaStatus::Connected { attempt_id: 7 },
            NativeWirelessAaStatus::Cancelled { attempt_id: 7 },
            NativeWirelessAaStatus::Failed {
                attempt_id: 7,
                code: NativeWirelessAaFailure::TransportFailed,
            },
        ] {
            let wire = serde_json::to_value(status).unwrap();
            assert_eq!(
                serde_json::from_value::<NativeWirelessAaStatus>(wire).unwrap(),
                status
            );
        }
        for code in [
            NativeWirelessAaFailure::Disabled,
            NativeWirelessAaFailure::Unavailable,
            NativeWirelessAaFailure::Busy,
            NativeWirelessAaFailure::DeviceUnavailable,
            NativeWirelessAaFailure::TimedOut,
            NativeWirelessAaFailure::TransportFailed,
        ] {
            let wire = serde_json::to_value(code).unwrap();
            assert_eq!(
                serde_json::from_value::<NativeWirelessAaFailure>(wire).unwrap(),
                code
            );
        }
    }

    #[test]
    fn connect_debug_redacts_caller_supplied_text() {
        let connect = NativeWirelessAaConnect {
            device_id: "native-secret-sentinel".into(),
        };
        assert!(!format!("{connect:?}").contains("native-secret-sentinel"));
    }
}
