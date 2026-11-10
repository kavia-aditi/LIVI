// Standalone experimental validation only. No production caller or connection.
#[cfg(target_os = "macos")]
mod validation {
    use livi_video_view::experimental_metal::Prototype;
    use std::collections::BTreeSet;
    use std::ffi::c_void;

    unsafe extern "C" {
        fn livi_decode_validation_run(
            context: *mut c_void,
            tick: extern "C" fn(*mut c_void, usize, u32) -> i32,
        ) -> i32;
        fn livi_decode_validation_resize();
        fn livi_decode_validation_measure();
    }

    struct ResizeBarrier {
        epoch: u64,
        outstanding: BTreeSet<u64>,
    }

    impl ResizeBarrier {
        fn accepts(&self, epoch: u64, frame: u64) -> bool {
            epoch > self.epoch && !self.outstanding.contains(&frame)
        }
    }

    struct Test {
        fixture: Vec<u8>,
        players: [Option<Prototype>; 3],
        counts: [usize; 7],
        reported_samples: [u64; 3],
        seen: BTreeSet<u64>,
        epochs: [u64; 3],
        last_visibility: [u64; 3],
        latency: Vec<f64>,
        resize_barrier: Option<ResizeBarrier>,
        cancel_observed: bool,
        replacement_observed: bool,
        resized_observed: bool,
        errors: usize,
    }

    impl Test {
        fn create(&mut self, index: usize, parent: usize) -> bool {
            // Parent is provided by our main-thread-owned AppKit harness.
            self.players[index] = unsafe { Prototype::new(parent, index as u64 + 1, "h264", &[]) };
            if let Some(player) = &self.players[index] {
                unsafe { livi_decode_validation_measure() };
                player.start()
            } else {
                false
            }
        }

        fn tick(&mut self, parent: usize, tick: u32) -> i32 {
            let stage = (tick / 60).min(6) as usize;
            match tick {
                0 if !self.create(0, parent) => return -1,
                60 => self.players[0].as_ref().unwrap().set_visible(false),
                120 => self.players[0].as_ref().unwrap().set_visible(true),
                240 => {
                    let player = self.players[0].as_ref().unwrap();
                    player.cancel();
                    self.cancel_observed = !player.push(&self.fixture) && !player.start();
                }
                300 if !self.create(1, parent) => return -1,
                360 => {
                    if !self.create(2, parent) {
                        return -1;
                    }
                    let old = self.players[1].as_ref().unwrap();
                    self.replacement_observed = !old.push(&self.fixture) && !old.start();
                }
                _ => {}
            }
            // Wait briefly for a genuinely outstanding decoded frame. Do not
            // claim the negative resize case was exercised if none was present.
            if (180..=210).contains(&tick) && self.resize_barrier.is_none() {
                let before = self.players[0].as_ref().unwrap().snapshot();
                if !before.pending_frames.is_empty() {
                    self.resize_barrier = Some(ResizeBarrier {
                        epoch: before.visibility_epoch,
                        outstanding: before.pending_frames.iter().map(|id| id.frame_id).collect(),
                    });
                    unsafe { livi_decode_validation_resize() };
                }
            }
            for (index, player) in self.players.iter().enumerate() {
                let Some(player) = player else { continue };
                for presentation in player.take_presentations() {
                    let id = presentation.identity;
                    if index == 0 && stage == 3 {
                        let Some(barrier) = &self.resize_barrier else {
                            // Pre-barrier samples are excluded and disclosed by
                            // issued-minus-reported accounting below.
                            continue;
                        };
                        if !barrier.accepts(id.visibility_epoch, id.frame_id) {
                            self.errors += 1;
                            continue;
                        }
                        self.resized_observed = true;
                    }
                    let forbidden = stage == 1
                        || stage == 4
                        || (index == 0 && stage >= 4)
                        || (index == 1 && stage >= 6);
                    let Some(decoded) = presentation.decoded_ready_time else {
                        self.errors += 1;
                        continue;
                    };
                    let ms = (presentation.presented_time - decoded) * 1000.0;
                    if forbidden
                        || id.session_epoch != index as u64 + 1
                        || id.player_epoch == 0
                        || id.visibility_epoch == 0
                        || !self.seen.insert(id.frame_id)
                        || !ms.is_finite()
                        || ms < 0.0
                        || (self.epochs[index] != 0 && self.epochs[index] != id.player_epoch)
                        || id.visibility_epoch < self.last_visibility[index]
                    {
                        self.errors += 1;
                        continue;
                    }
                    self.epochs[index] = id.player_epoch;
                    self.last_visibility[index] = id.visibility_epoch;
                    self.latency.push(ms);
                    self.reported_samples[index] += 1;
                    self.counts[stage] += 1;
                }
                if tick < 420 {
                    let pushed = player.push(&self.fixture);
                    let terminal = (index == 0 && stage >= 4) || (index == 1 && stage >= 6);
                    if pushed == terminal {
                        self.errors += 1;
                    }
                }
            }
            if tick < 420 {
                return 0;
            }
            self.latency.sort_by(f64::total_cmp);
            let distinct_players = self.epochs.iter().copied().collect::<BTreeSet<_>>().len() == 3;
            let passed = self.errors == 0
                && [0, 2, 3, 5, 6].iter().all(|i| self.counts[*i] > 0)
                && self.counts[1] == 0
                && self.counts[4] == 0
                && self.cancel_observed
                && self.replacement_observed
                && self.resize_barrier.is_some()
                && self.resized_observed
                && distinct_players;
            println!(
                "REAL_DECODE_METAL_VALIDATION {}",
                if passed { "PASS" } else { "FAIL" }
            );
            println!(
                "phase_counts={:?} errors={} unique_frames={} player_epochs={:?}",
                self.counts,
                self.errors,
                self.seen.len(),
                self.epochs
            );
            println!(
                "cancel={} replacement={} fresh_post_resize={}",
                self.cancel_observed, self.replacement_observed, self.resized_observed
            );
            if let Some(barrier) = &self.resize_barrier {
                println!(
                    "pre_resize_epoch={} outstanding_pre_resize_frames={:?}",
                    barrier.epoch, barrier.outstanding
                );
            }
            if !self.latency.is_empty() {
                let n = self.latency.len() - 1;
                println!(
                    "decoded_delivery_to_present_ms samples={} p50={:.3} p95={:.3} max={:.3}",
                    n + 1,
                    self.latency[n / 2],
                    self.latency[n * 95 / 100],
                    self.latency[n]
                );
            }
            for (index, player) in self.players.iter().enumerate() {
                let Some(player) = player else { continue };
                player.cancel();
                let snapshot = player.snapshot();
                let stats = snapshot.statistics;
                println!(
                    "player={} issued={} pending_evictions={} result_evictions={} \
                     rejected_callbacks={} accepted={} invalidated_pending={} \
                     cleared_results={} pending={} queued_results={} reported={} \
                     issued_minus_reported={}",
                    index,
                    stats.issued_frames,
                    stats.pending_evictions,
                    stats.result_evictions,
                    stats.rejected_callbacks,
                    stats.accepted_presentations,
                    stats.invalidated_pending_frames,
                    stats.cleared_results,
                    snapshot.pending_frames.len(),
                    snapshot.queued_results,
                    self.reported_samples[index],
                    stats
                        .issued_frames
                        .saturating_sub(self.reported_samples[index])
                );
            }
            println!(
                "LATENCY_SCOPE=retained_ACKs_only; missing_samples_include_eviction_\
                 lifecycle_invalidation_native_drops_and_unreported_results"
            );
            println!("SCOPE=local_H264_software_decode_calibration_appsink_Rust_gate_real_Metal");
            if passed { 1 } else { -1 }
        }
    }

    extern "C" fn tick(context: *mut c_void, parent: usize, tick: u32) -> i32 {
        let test = unsafe { &mut *context.cast::<Test>() };
        // Do not unwind across AppKit's callback boundary.
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| test.tick(parent, tick)))
            .unwrap_or(-1)
    }

    pub fn run() -> i32 {
        let Some(path) = std::env::args_os().nth(1) else {
            eprintln!("Pass the local offline H.264 fixture path");
            return 2;
        };
        let Ok(fixture) = std::fs::read(path) else {
            return 2;
        };
        if fixture.is_empty() || fixture.len() > 1024 * 1024 {
            return 2;
        }
        let mut test = Test {
            fixture,
            players: [None, None, None],
            counts: [0; 7],
            reported_samples: [0; 3],
            seen: BTreeSet::new(),
            epochs: [0; 3],
            last_visibility: [0; 3],
            latency: Vec::new(),
            resize_barrier: None,
            cancel_observed: false,
            replacement_observed: false,
            resized_observed: false,
            errors: 0,
        };
        unsafe { livi_decode_validation_run((&raw mut test).cast(), tick) }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn barrier() -> ResizeBarrier {
            ResizeBarrier {
                epoch: 7,
                outstanding: [41, 42].into_iter().collect(),
            }
        }

        #[test]
        fn resize_barrier_rejects_equal_and_older_epochs() {
            assert!(!barrier().accepts(7, 43));
            assert!(!barrier().accepts(6, 43));
        }

        #[test]
        fn resize_barrier_rejects_outstanding_ids_even_if_relabelled() {
            assert!(!barrier().accepts(8, 41));
            assert!(!barrier().accepts(9, 42));
        }

        #[test]
        fn resize_barrier_separately_accepts_fresh_post_resize_frame() {
            assert!(barrier().accepts(8, 43));
        }
    }
}

fn main() {
    #[cfg(target_os = "macos")]
    std::process::exit(validation::run());
    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("This isolated prototype requires macOS");
        std::process::exit(2);
    }
}
