// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

//! Simulated callback tests ONLY. No decoder, NSView, Metal device or compositor.
use super::*;

#[test]
fn allocated_player_epochs_never_repeat() {
    let counter = AtomicU64::new(1);
    let mut epochs = std::collections::BTreeSet::new();
    for _ in 0..100 {
        assert!(epochs.insert(allocate_epoch(&counter).unwrap()));
    }
}

#[test]
fn player_epoch_exhaustion_cannot_wrap() {
    let counter = AtomicU64::new(u64::MAX);
    assert!(allocate_epoch(&counter).is_none());
    assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
}

fn ready(session: u64, player: u64) -> Gate {
    let mut gate = Gate::new(session, player);
    gate.visibility(1, true);
    gate
}

#[test]
fn enqueue_and_decode_alone_cannot_acknowledge() {
    let mut gate = ready(10, 20);
    gate.issue().unwrap();
    assert!(gate.presented.is_empty());
}

#[test]
fn simulated_presented_callback_preserves_exact_identity() {
    let mut gate = ready(10, 20);
    let id = gate.issue().unwrap();
    gate.acknowledge(id.frame_id, id.visibility_epoch, 42.5);
    assert_eq!(
        gate.presented.pop_front(),
        Some(Presentation {
            identity: id,
            presented_time: 42.5,
            decoded_ready_time: None,
        })
    );
}

#[test]
fn decoded_clock_is_correlated_only_after_real_ack_event() {
    let mut gate = ready(10, 20);
    let id = gate.issue_decoded(40.0).unwrap();
    assert!(gate.presented.is_empty());
    gate.acknowledge(id.frame_id, id.visibility_epoch, 40.025);
    let result = gate.presented.pop_front().unwrap();
    assert_eq!(result.identity, id);
    assert_eq!(result.decoded_ready_time, Some(40.0));
    assert!((result.presented_time - 40.025).abs() < f64::EPSILON);
}

#[test]
fn invalid_decoded_clock_and_backwards_presentation_fail_closed() {
    let mut gate = ready(10, 20);
    for time in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert!(gate.issue_decoded(time).is_none());
    }
    let id = gate.issue_decoded(40.0).unwrap();
    gate.acknowledge(id.frame_id, id.visibility_epoch, 39.0);
    assert!(gate.presented.is_empty());
}

#[test]
fn frame_ids_are_unique_and_not_reused_across_visibility_epochs() {
    let mut gate = ready(10, 20);
    let a = gate.issue().unwrap();
    gate.visibility(2, true);
    let b = gate.issue().unwrap();
    assert!(b.frame_id > a.frame_id);
    assert_ne!(a.visibility_epoch, b.visibility_epoch);
}

#[test]
fn different_players_get_globally_distinct_frame_ids() {
    let a = ready(10, 20).issue().unwrap();
    let b = ready(11, 21).issue().unwrap();
    assert_ne!(a.frame_id, b.frame_id);
}

#[test]
fn unpresented_and_nonfinite_timestamps_do_not_succeed() {
    for time in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        let mut gate = ready(10, 20);
        let id = gate.issue().unwrap();
        gate.acknowledge(id.frame_id, id.visibility_epoch, time);
        assert!(gate.presented.is_empty());
    }
}

#[test]
fn duplicate_or_unknown_frame_cannot_succeed() {
    let mut gate = ready(10, 20);
    let id = gate.issue().unwrap();
    gate.acknowledge(u64::MAX, 1, 42.0);
    gate.acknowledge(id.frame_id, 1, 42.0);
    gate.acknowledge(id.frame_id, 1, 43.0);
    assert_eq!(gate.presented.len(), 1);
}

#[test]
fn hiding_rejects_inflight_frames_and_clears_queued_results() {
    let mut gate = ready(10, 20);
    let a = gate.issue().unwrap();
    let b = gate.issue().unwrap();
    gate.acknowledge(a.frame_id, 1, 42.0);
    gate.visibility(2, false);
    gate.acknowledge(b.frame_id, 1, 43.0);
    assert!(gate.issue().is_none());
    assert!(gate.presented.is_empty());
}

#[test]
fn showing_again_requires_a_new_frame() {
    let mut gate = ready(10, 20);
    let old = gate.issue().unwrap();
    gate.visibility(2, false);
    gate.visibility(3, true);
    gate.acknowledge(old.frame_id, 1, 42.0);
    assert!(gate.presented.is_empty());
    let fresh = gate.issue().unwrap();
    gate.acknowledge(fresh.frame_id, 3, 43.0);
    assert_eq!(gate.presented[0].identity, fresh);
}

#[test]
fn resize_invalidates_even_if_view_remains_visible() {
    let mut gate = ready(10, 20);
    let old = gate.issue().unwrap();
    gate.visibility(2, true);
    gate.acknowledge(old.frame_id, 1, 42.0);
    assert!(gate.presented.is_empty());
}

#[test]
fn stale_visibility_events_cannot_rearm() {
    let mut gate = ready(10, 20);
    gate.visibility(3, false);
    gate.visibility(2, true);
    gate.visibility(3, true);
    assert!(gate.issue().is_none());
}

#[test]
fn cancellation_is_terminal_even_after_visible_event() {
    let mut gate = ready(10, 20);
    let old = gate.issue().unwrap();
    gate.cancel();
    gate.visibility(2, true);
    gate.acknowledge(old.frame_id, 1, 42.0);
    assert!(gate.issue().is_none());
    assert!(gate.presented.is_empty());
}

#[test]
fn replacement_routes_late_callback_to_cancelled_old_owner() {
    let old = Mutex::new(ready(10, 20));
    let id = old.lock().unwrap().issue().unwrap();
    old.lock().unwrap().cancel();
    let new = Mutex::new(ready(10, 21));
    native_event((&raw const old).cast(), 3, id.frame_id, 1, 42.0);
    assert!(old.lock().unwrap().presented.is_empty());
    assert!(new.lock().unwrap().presented.is_empty());
    assert_eq!(new.lock().unwrap().issue().unwrap().player_epoch, 21);
}

#[test]
fn session_change_uses_a_new_immutable_context() {
    let mut old = ready(10, 20);
    let id = old.issue().unwrap();
    old.cancel();
    let mut new = ready(11, 21);
    old.acknowledge(id.frame_id, 1, 42.0);
    assert!(new.presented.is_empty());
    assert_eq!(new.issue().unwrap().session_epoch, 11);
}

#[test]
fn pending_and_result_queues_are_bounded() {
    let mut gate = ready(10, 20);
    let evicted = gate.issue().unwrap();
    for _ in 0..100 {
        gate.issue().unwrap();
    }
    assert_eq!(gate.pending.len(), LIMIT);
    gate.acknowledge(evicted.frame_id, 1, 42.0);
    assert!(gate.presented.is_empty());
    for _ in 0..100 {
        let id = gate.issue().unwrap();
        gate.acknowledge(id.frame_id, 1, 42.0);
    }
    assert_eq!(gate.presented.len(), LIMIT);
}

#[test]
fn frame_counter_exhaustion_fails_closed() {
    let mut gate = ready(10, 20);
    let counter = AtomicU64::new(u64::MAX);
    assert!(gate.issue_with_counter(&counter).is_none());
    assert!(gate.cancelled);
}

#[test]
fn zero_epochs_are_inert() {
    for (session, player) in [(0, 1), (1, 0)] {
        let mut gate = ready(session, player);
        assert!(gate.issue().is_none());
    }
}

#[test]
fn only_drawable_presented_event_can_publish() {
    let state = Mutex::new(ready(10, 20));
    let id = state.lock().unwrap().issue().unwrap();
    for event in [0, 5, 99] {
        native_event((&raw const state).cast(), event, id.frame_id, 1, 42.0);
    }
    assert!(state.lock().unwrap().presented.is_empty());
    native_event((&raw const state).cast(), 3, id.frame_id, 1, 42.0);
    assert_eq!(state.lock().unwrap().presented.len(), 1);
}

#[test]
fn production_routes_do_not_construct_the_prototype() {
    let server = include_str!("../server.rs");
    let screens = include_str!("../screens.rs");
    assert!(!server.contains("Prototype::"));
    assert!(!screens.contains("Prototype::"));
    assert!(screens.contains("Player::new(codec, window, codec_data)"));
}

#[test]
fn validation_example_and_native_shim_require_explicit_feature() {
    let manifest = include_str!("../../Cargo.toml");
    let build = include_str!("../../build.rs");
    assert!(manifest.contains(r#"required-features = ["experimental-metal-validation"]"#));
    assert!(build.contains("CARGO_FEATURE_EXPERIMENTAL_METAL_VALIDATION"));
    assert!(build.contains(r#".file("../../../utils/decode_metal_validation.mm")"#));
}

#[test]
fn delayed_evicted_ack_is_counted_not_silently_sampled() {
    let mut gate = ready(10, 20);
    let old = gate.issue_decoded(40.0).unwrap();
    let mut latest = old;
    for _ in 0..LIMIT {
        latest = gate.issue_decoded(40.0).unwrap();
    }
    gate.acknowledge(old.frame_id, 1, 41.0);
    gate.acknowledge(latest.frame_id, 1, 41.0);
    let snapshot = gate.snapshot();
    assert_eq!(snapshot.statistics.issued_frames, 4);
    assert_eq!(snapshot.statistics.pending_evictions, 1);
    assert_eq!(snapshot.statistics.rejected_callbacks, 1);
    assert_eq!(snapshot.statistics.accepted_presentations, 1);
    assert_eq!(snapshot.pending_frames.len(), 2);
    assert_eq!(snapshot.queued_results, 1);
}

#[test]
fn result_evictions_remain_visible_in_cumulative_statistics() {
    let mut gate = ready(10, 20);
    for _ in 0..LIMIT + 2 {
        let id = gate.issue_decoded(40.0).unwrap();
        gate.acknowledge(id.frame_id, 1, 41.0);
    }
    let before = gate.snapshot();
    assert_eq!(before.statistics.issued_frames, 5);
    assert_eq!(before.statistics.accepted_presentations, 5);
    assert_eq!(before.statistics.result_evictions, 2);
    assert_eq!(before.queued_results, LIMIT);
    gate.presented.clear();
    assert_eq!(gate.snapshot().statistics, before.statistics);
}

#[test]
fn invalid_unknown_duplicate_stale_and_cancelled_acks_are_counted() {
    let mut gate = ready(10, 20);
    let id = gate.issue().unwrap();
    gate.acknowledge(id.frame_id, 1, 0.0);
    gate.acknowledge(u64::MAX, 1, 41.0);
    gate.acknowledge(id.frame_id, 2, 41.0);
    gate.acknowledge(id.frame_id, 1, 41.0);
    gate.acknowledge(id.frame_id, 1, 41.0);
    let late = gate.issue().unwrap();
    gate.cancel();
    gate.acknowledge(late.frame_id, 1, 41.0);
    assert_eq!(gate.snapshot().statistics.rejected_callbacks, 5);
}

#[test]
fn lifecycle_clearing_is_reported_without_resetting_counters() {
    let mut gate = ready(10, 20);
    let first = gate.issue().unwrap();
    gate.issue().unwrap();
    gate.acknowledge(first.frame_id, 1, 41.0);
    gate.visibility(2, false);
    let snapshot = gate.snapshot();
    assert_eq!(snapshot.statistics.issued_frames, 2);
    assert_eq!(snapshot.statistics.invalidated_pending_frames, 1);
    assert_eq!(snapshot.statistics.cleared_results, 1);
    assert!(snapshot.pending_frames.is_empty());
    assert_eq!(snapshot.queued_results, 0);
    gate.cancel();
    gate.cancel();
    assert_eq!(gate.snapshot().statistics, snapshot.statistics);
}

#[test]
fn explicit_resize_barrier_rejects_outstanding_then_accepts_fresh_frame() {
    let mut gate = ready(10, 20);
    let a = gate.issue_decoded(40.0).unwrap();
    let b = gate.issue_decoded(40.0).unwrap();
    let before = gate.snapshot();
    assert_eq!(before.pending_frames.len(), 2);
    gate.visibility(before.visibility_epoch + 1, true);
    for old in [a, b] {
        gate.acknowledge(old.frame_id, before.visibility_epoch, 41.0);
    }
    assert!(gate.presented.is_empty());
    assert_eq!(gate.snapshot().statistics.rejected_callbacks, 2);
    assert_eq!(gate.snapshot().statistics.invalidated_pending_frames, 2);
    let fresh = gate.issue_decoded(41.0).unwrap();
    assert!(fresh.visibility_epoch > before.visibility_epoch);
    gate.acknowledge(fresh.frame_id, fresh.visibility_epoch, 42.0);
    assert_eq!(gate.presented.len(), 1);
    assert_eq!(gate.presented[0].identity, fresh);
}

#[test]
fn diagnostics_saturate_instead_of_wrapping() {
    let mut gate = ready(10, 20);
    gate.statistics.issued_frames = u64::MAX;
    gate.statistics.pending_evictions = u64::MAX;
    gate.statistics.rejected_callbacks = u64::MAX;
    for _ in 0..LIMIT + 1 {
        gate.issue().unwrap();
    }
    gate.acknowledge(u64::MAX, 1, 41.0);
    assert_eq!(gate.statistics.issued_frames, u64::MAX);
    assert_eq!(gate.statistics.pending_evictions, u64::MAX);
    assert_eq!(gate.statistics.rejected_callbacks, u64::MAX);
}
