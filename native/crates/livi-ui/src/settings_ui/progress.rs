// SPDX-License-Identifier: GPL-3.0-or-later
//! Display-only evidence; never used to authorize or connect a session.
use livi_core_proto::native_wireless_aa::{
    NativeProjectionMilestone as M, NativeProjectionProgress as P,
    NativeWirelessAaStatus as S,
};
use serde_json::Value;

#[derive(Default)]
pub(super) struct Evidence {
    owner: Option<(u64, u64, u64)>,
    milestones: Vec<M>,
}

impl Evidence {
    pub(super) fn observe(&mut self, state: &Value) {
        let status = serde_json::from_value::<S>(state["system"]["nativeWirelessAa"].clone()).ok();
        let attempt = match status {
            Some(S::Connecting { attempt_id } | S::Connected { attempt_id }
                | S::Failed { attempt_id, .. } | S::Cancelled { attempt_id }) => attempt_id,
            _ => {
                self.owner = None;
                self.milestones.clear();
                return;
            }
        };
        if self.owner.is_some_and(|(_, old, _)| old != attempt) {
            self.owner = None;
            self.milestones.clear();
        }
        let progress = serde_json::from_value::<P>(state["system"]["nativeProjection"].clone()).ok();
        let (owner, milestone) = match progress {
            Some(P::Milestone { owner, milestone }) => (owner, Some(milestone)),
            Some(P::Presented { owner, .. } | P::Failed { owner, .. }
                | P::Cancelled { owner } | P::Invalidated { owner, .. }) => (owner, None),
            _ => return,
        };
        if owner.attempt_id != attempt {
            self.owner = None;
            self.milestones.clear();
            return;
        }
        let key = (owner.runtime_id, owner.attempt_id, owner.generation);
        if self.owner != Some(key) {
            self.owner = Some(key);
            self.milestones.clear();
        }
        if let Some(milestone) = milestone {
            if !self.milestones.contains(&milestone) {
                self.milestones.push(milestone);
            }
        }
    }

    pub(super) fn supplement(&self, state: &mut Value) {
        // Overwrite any similarly named input; this is local UI state only.
        state["uiNativeObserved"] = serde_json::to_value(&self.milestones).unwrap_or(Value::Null);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state(attempt: u64, generation: u64, milestone: &str) -> Value {
        json!({"system":{"nativeWirelessAa":{"phase":"connecting","attemptId":attempt},
            "nativeProjection":{"phase":"milestone",
                "owner":{"runtimeId":1,"attemptId":attempt,"generation":generation},
                "milestone":milestone}}})
    }

    #[test]
    fn retains_only_observed_milestones_and_resets_on_owner_change() {
        let mut evidence = Evidence::default();
        let mut s = state(2, 3, "rfcomm");
        evidence.observe(&s);
        s = state(2, 3, "wpp");
        evidence.observe(&s);
        evidence.supplement(&mut s);
        assert_eq!(s["uiNativeObserved"], json!(["rfcomm", "wpp"]));
        assert!(!s["uiNativeObserved"].as_array().unwrap().contains(&json!("discovery")));
        evidence.observe(&state(2, 4, "wpp"));
        evidence.supplement(&mut s);
        assert_eq!(s["uiNativeObserved"], json!(["wpp"]));
        evidence.observe(&state(9, 4, "waitingTcp"));
        evidence.supplement(&mut s);
        assert_eq!(s["uiNativeObserved"], json!(["waitingTcp"]));
        evidence.observe(&json!({}));
        evidence.supplement(&mut s);
        assert_eq!(s["uiNativeObserved"], json!([]));
    }

    #[test]
    fn stale_attempt_cannot_publish_success_and_failure_preserves_measured_stages() {
        let mut evidence = Evidence::default();
        let mut s = state(2, 3, "rfcomm");
        evidence.observe(&s);
        s["system"]["nativeProjection"] = json!({"phase":"failed",
            "owner":{"runtimeId":1,"attemptId":2,"generation":3},"code":"admissionFailed"});
        evidence.observe(&s);
        evidence.supplement(&mut s);
        assert_eq!(s["uiNativeObserved"], json!(["rfcomm"]));
        s["system"]["nativeWirelessAa"]["attemptId"] = json!(8);
        evidence.observe(&s);
        evidence.supplement(&mut s);
        assert_eq!(s["uiNativeObserved"], json!([]));
    }
}
