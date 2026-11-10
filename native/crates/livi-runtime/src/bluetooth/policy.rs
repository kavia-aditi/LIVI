// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

//! Reconnect decisions without I/O or an implicit clock. Adapters supply observations.

use std::collections::HashMap;
use std::time::{Duration, Instant};

pub const INTERVAL: Duration = Duration::from_secs(1);
pub const PAGE_TIMEOUT: Duration = Duration::from_secs(1);
const FAST_ATTEMPTS: u32 = 15;
const SLOW_INTERVAL: Duration = Duration::from_secs(30);
const STALE: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend { Linux, Dongle }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Skip,
    /// Only disconnected-device attempts affect backoff.
    Page { track: bool },
    Disconnect,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Observation {
    pub connected: bool,
    pub link_active: bool,
    pub hands_free: bool,
    pub has_wake: bool,
    pub hfp_linked: bool,
    pub in_use: bool,
    pub in_call: bool,
}

#[derive(Default)]
pub struct Reconnect {
    turn: usize,
    attempts: HashMap<String, u32>,
    next_try: HashMap<String, Instant>,
    stale_since: HashMap<String, Instant>,
}

impl Reconnect {
    /// Advance even when the adapter subsequently rejects a malformed/unavailable target.
    pub fn next(&mut self, targets: &[(String, Option<String>)]) -> Option<(String, Option<String>)> {
        let listed = |mac: &String| targets.iter().any(|(m, _)| m == mac);
        self.attempts.retain(|mac, _| listed(mac));
        self.next_try.retain(|mac, _| listed(mac));
        self.stale_since.retain(|mac, _| listed(mac));
        if targets.is_empty() { return None; }
        let target = targets[self.turn % targets.len()].clone();
        self.turn = self.turn.wrapping_add(1);
        Some(target)
    }

    pub fn decide(&mut self, backend: Backend, mac: &str, seen: Observation, now: Instant) -> Action {
        if seen.connected {
            self.attempts.remove(mac);
            self.next_try.remove(mac);
            if seen.link_active {
                self.stale_since.remove(mac);
                return Action::Skip;
            }
            let since = *self.stale_since.entry(mac.to_string()).or_insert(now);
            if now.duration_since(since) >= STALE {
                self.stale_since.remove(mac);
                return Action::Disconnect;
            }
            return if match backend {
                Backend::Linux => seen.has_wake,
                Backend::Dongle => !seen.in_use,
            } {
                Action::Page { track: false }
            } else { Action::Skip };
        }
        self.stale_since.remove(mac);
        if self.next_try.get(mac).is_some_and(|deadline| now < *deadline)
            || (backend == Backend::Dongle && seen.in_call)
        {
            return Action::Skip;
        }
        Action::Page { track: true }
    }

    /// Call only for paging that actually started, using completion/timeout time.
    pub fn completed(&mut self, mac: &str, success: bool, now: Instant) {
        if success {
            self.attempts.remove(mac);
            self.next_try.remove(mac);
        } else {
            let n = self.attempts.entry(mac.to_string()).or_insert(0);
            *n += 1;
            if *n >= FAST_ATTEMPTS {
                self.next_try.insert(mac.to_string(), now + SLOW_INTERVAL);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifteenth_failure_backs_off_from_completion_and_success_resets() {
        let mut p = Reconnect::default();
        let now = Instant::now();
        let seen = Observation::default();
        for n in 0..15 {
            assert_eq!(p.decide(Backend::Linux, "A", seen, now), Action::Page { track: true });
            p.completed("A", false, now);
            assert_eq!(p.attempts["A"], n + 1);
        }
        assert_eq!(p.decide(Backend::Linux, "A", seen, now + Duration::from_secs(29)), Action::Skip);
        assert_eq!(p.decide(Backend::Linux, "A", seen, now + Duration::from_secs(30)), Action::Page { track: true });
        p.completed("A", true, now + Duration::from_secs(30));
        assert!(!p.attempts.contains_key("A"));
        assert!(!p.next_try.contains_key("A"));
    }

    #[test]
    fn stale_projection_disconnects_at_ten_seconds_on_each_backend() {
        for backend in [Backend::Linux, Backend::Dongle] {
            let mut p = Reconnect::default();
            let now = Instant::now();
            let seen = Observation { connected: true, has_wake: true, ..Default::default() };
            assert_eq!(p.decide(backend, "A", seen, now), Action::Page { track: false });
            assert_eq!(p.decide(backend, "A", seen, now + Duration::from_secs(9)), Action::Page { track: false });
            assert_eq!(p.decide(backend, "A", seen, now + Duration::from_secs(10)), Action::Disconnect);
            assert!(!p.stale_since.contains_key("A"));
        }
    }

    #[test]
    fn hands_free_retains_the_original_stale_disconnect_and_wake_rules() {
        let now = Instant::now();
        for backend in [Backend::Linux, Backend::Dongle] {
            let mut p = Reconnect::default();
            let seen = Observation {
                connected: true, hands_free: true, hfp_linked: true, in_use: true,
                has_wake: true,
                ..Default::default()
            };
            let expected = if backend == Backend::Linux { Action::Page { track: false } } else { Action::Skip };
            assert_eq!(p.decide(backend, "A", seen, now), expected);
            assert_eq!(p.decide(backend, "A", seen, now + Duration::from_secs(9)), expected);
            assert_eq!(p.decide(backend, "A", seen, now + STALE), Action::Disconnect);
            assert!(p.stale_since.is_empty());
        }
    }

    #[test]
    fn calls_and_missing_wake_keep_backend_specific_behavior() {
        let now = Instant::now();
        let mut p = Reconnect::default();
        let in_call = Observation { in_call: true, ..Default::default() };
        assert_eq!(p.decide(Backend::Dongle, "A", in_call, now), Action::Skip);
        assert_eq!(p.decide(Backend::Linux, "A", in_call, now), Action::Page { track: true });
        let acl = Observation { connected: true, ..Default::default() };
        assert_eq!(p.decide(Backend::Linux, "A", acl, now), Action::Skip);
        assert_eq!(p.decide(Backend::Dongle, "A", acl, now), Action::Page { track: false });
        assert_eq!(p.decide(Backend::Dongle, "A", Observation { in_use: true, ..acl }, now), Action::Skip);
    }

    #[test]
    fn round_robin_prunes_history_without_resetting_turn() {
        let mut p = Reconnect::default();
        let targets = vec![("A".into(), None), ("B".into(), Some("wake".into()))];
        assert_eq!(p.next(&targets).unwrap().0, "A");
        assert_eq!(p.next(&targets).unwrap().0, "B");
        p.completed("A", false, Instant::now());
        assert_eq!(p.next(&targets[1..]).unwrap().0, "B");
        assert!(!p.attempts.contains_key("A"));
        assert_eq!(p.next(&[]), None);
    }

    #[test]
    fn active_projection_clears_staleness_before_it_can_drop_a_link() {
        let mut p = Reconnect::default();
        let now = Instant::now();
        let acl = Observation { connected: true, ..Default::default() };
        p.decide(Backend::Linux, "A", acl, now);
        assert_eq!(p.decide(Backend::Linux, "A", Observation { link_active: true, ..acl }, now + STALE), Action::Skip);
        assert_eq!(p.decide(Backend::Linux, "A", acl, now + STALE + STALE), Action::Skip);
    }
}
