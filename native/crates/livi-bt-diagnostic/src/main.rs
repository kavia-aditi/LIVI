// SPDX-License-Identifier: GPL-3.0-or-later
#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    use livi_bt_diagnostic::{Options, Radio, Runner, Stage};
    use livi_native_bluetooth::{Address, Backend, Error, Event, MacBackend};

    static CANCELLED: AtomicBool = AtomicBool::new(false);
    extern "C" fn cancel(_: libc::c_int) { CANCELLED.store(true, Ordering::Relaxed); }

    // Observations only: never write to an output sink while callbacks need pumping.
    // The one-shot runner has seven stages; retain a fixed, bounded history.
    struct StageHistory {
        stages: [Option<Stage>; 7],
        len: usize,
    }
    impl StageHistory {
        fn new() -> Self { Self { stages: [None; 7], len: 0 } }
        fn record(&mut self, stage: Stage) {
            if self.len > 0 && self.stages[self.len - 1] == Some(stage) { return; }
            if self.len < self.stages.len() {
                self.stages[self.len] = Some(stage);
                self.len += 1;
            }
        }
        fn iter(&self) -> impl Iterator<Item = Stage> + '_ {
            self.stages[..self.len].iter().copied().flatten()
        }
    }

    unsafe extern "C" {
        static kCFRunLoopDefaultMode: *const c_void;
        fn CFRunLoopRunInMode(mode: *const c_void, seconds: f64, return_after_source: bool) -> i32;
    }

    fn state_description(stage: Stage) -> &'static str {
        match stage {
            Stage::Starting => "waiting-for-local-service; no-transport",
            Stage::Paired => "selected-bond-checked; awaiting-owned-rfcomm",
            Stage::Connected => "selected-peer-owned-stream-accepted; wpp-not-validated",
            Stage::VersionSent => "version-request-queued-to-owned-bridge; response-not-validated",
            Stage::IdentityValid => "wpp-version-status-and-self-reported-labels-validated",
            Stage::Cleaning => "closing-owned-stream-and-withdrawing-service",
            Stage::Stopped => "attempt-ended; no-active-runner-stream",
        }
    }

    struct Native(MacBackend);
    impl Radio for Native {
        fn start(&mut self) -> Result<(), Error> { self.0.diagnostic_start() }
        fn connect_paired(&mut self, generation: u64, peer: Address) -> Result<(), Error> {
            // Runner selection arms only this peer on the existing local service.
            // No outgoing SDP query or RFCOMM open is appropriate for this test.
            self.0.listen_paired(generation, peer)
        }
        fn tick(&mut self, now: Duration) { self.0.tick(now); }
        fn next_event(&mut self) -> Option<Event> { self.0.next_event() }
        fn cancel(&mut self, generation: u64) { self.0.cancel(generation); }
        fn stop(&mut self) { self.0.stop(); }
        fn idle(&self) -> bool { self.0.diagnostic_idle() }
        fn diagnostic_failure(&self) -> Option<livi_native_bluetooth::DiagnosticFailure> {
            self.0.diagnostic_failure()
        }
        fn next_diagnostic_event(&mut self) -> Option<livi_native_bluetooth::DiagnosticEvent> {
            self.0.next_diagnostic_event()
        }
        fn cleanup_snapshot(&self) -> Option<livi_native_bluetooth::CleanupSnapshot> {
            self.0.cleanup_snapshot()
        }
    }

    pub fn run() -> bool {
        // Authorization is parsed before even allocating the dormant backend.
        let args: Vec<String> = std::env::args().skip(1).collect();
        let options = match Options::parse(&args) {
            Ok(options) => options,
            Err(_) => {
                eprintln!("REFUSED: explicit Bluetooth authorization, paired-only confirmation and one phone address required.");
                return false;
            }
        };
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("REFUSED: do not run as root.");
            return false;
        }
        unsafe {
            for signum in [libc::SIGINT, libc::SIGTERM] {
                let previous = libc::signal(signum, Some(cancel));
                if previous.is_some_and(|handler| handler as usize == libc::SIG_ERR) {
                    eprintln!("REFUSED: cancellation setup failed.");
                    return false;
                }
            }
        }
        println!("Bluetooth-only diagnostic: paired listener-only; outgoing SDP/open disabled. Up to 60s including service startup, plus 2s cleanup. Ctrl-C cancels. Stage output is deferred until completion.");
        let origin = Instant::now();
        let backend = match MacBackend::new() {
            Ok(backend) => backend,
            Err(_) => {
                eprintln!("FAIL: category=OwnerUnavailable operation=Start IOReturn=unavailable. No bootstrap fallback; no readiness claim.");
                return false;
            }
        };
        let mut runner = match Runner::new(Native(backend), options, origin.elapsed()) {
            Ok(runner) => runner,
            Err(livi_bt_diagnostic::Failure::Native(detail)) => {
                eprintln!("FAIL: {detail}. Startup refused; cleanup not independently confirmed. No bootstrap fallback; no readiness claim.");
                return false;
            }
            Err(_) => { eprintln!("FAIL: native owner startup refused."); return false; }
        };
        let mut history = StageHistory::new();
        history.record(runner.stage());
        loop {
            runner.step(origin.elapsed(), CANCELLED.load(Ordering::Relaxed));
            history.record(runner.stage());
            if let Some(result) = runner.result() {
                // Output only after cleanup/outcome commitment; never block deadlines
                // or cancellation on a lifecycle log sink during the attempt.
                for stage in history.iter() {
                    println!("Stage: {stage:?} state={}", state_description(stage));
                }
                for event in runner.diagnostic_transcript() { println!("{event}"); }
                println!("TransportEvidence: selected_peer_owned_stream_observed={} active_runner_stream=false",
                    runner.transport_observed());
                println!("TransportEvidence is historical after cleanup, not OS Connected, authenticated identity or projection readiness.");
                println!("IncomingChannelOpen reports native isOpen() at the incoming-open notification; it does not invent an outgoing open-completion status.");
                if !runner.transport_observed() && runner.diagnostic_transcript().iter().any(|event|
                    event.kind == livi_native_bluetooth::DiagnosticLifecycle::PairedListenerArmed)
                {
                    println!("Activation: paired listener was armed, but no selected-peer owned incoming transport was accepted. Phone-side AA discovery/activation is not established; exact trigger unknown.");
                }
                if let Some(snapshot) = runner.cleanup_report() {
                    println!("{snapshot}");
                    println!("Cleanup snapshot describes local ownership only; OS-side teardown is not independently verified.");
                }
                return match result {
                    Ok(()) => {
                        println!("PASS: selected bond verified; RFCOMM connected; WPP 6.0/status/identity valid; owned cleanup confirmed.");
                        println!("Identity labels are self-reported. No TCP authentication or projection readiness established.");
                        true
                    }
                    Err(error) => {
                        let cleanup = if error == livi_bt_diagnostic::Failure::Cleanup {
                            "unconfirmed"
                        } else { "confirmed" };
                        if let Some(detail) = runner.diagnostic_failure() {
                            eprintln!("FAIL: {detail}; outcome={error:?}; owned cleanup={cleanup}. No bootstrap fallback; no readiness claim.");
                        } else {
                            eprintln!("FAIL: {error:?}; owned cleanup={cleanup}. No bootstrap fallback; no readiness claim.");
                        }
                        false
                    }
                };
            }
            // IOBluetooth and CoreBluetooth callbacks stay on the main thread.
            unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.02, true); }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn state_reporting_does_not_claim_transport_from_bond_or_submission() {
            assert_eq!(state_description(Stage::Paired),
                "selected-bond-checked; awaiting-owned-rfcomm");
            assert!(state_description(Stage::VersionSent).contains("response-not-validated"));
            assert!(state_description(Stage::Stopped).contains("no-active-runner-stream"));
        }

        #[test]
        fn stage_history_preserves_order_without_repeated_poll_observations() {
            let expected = [Stage::Starting, Stage::Paired, Stage::Connected,
                Stage::VersionSent, Stage::IdentityValid, Stage::Cleaning, Stage::Stopped];
            let mut history = StageHistory::new();
            for stage in expected {
                for _ in 0..100 { history.record(stage); }
            }
            assert_eq!(history.iter().collect::<Vec<_>>(), expected);
        }

        #[test]
        fn stage_history_is_bounded_even_for_unexpected_transitions() {
            let mut history = StageHistory::new();
            for _ in 0..100 {
                history.record(Stage::Starting);
                history.record(Stage::Cleaning);
            }
            assert_eq!(history.iter().count(), 7);
            assert_eq!(history.stages.len(), 7);
        }
    }
}

fn main() -> std::process::ExitCode {
    #[cfg(target_os = "macos")]
    if mac::run() { return std::process::ExitCode::SUCCESS; }
    #[cfg(not(target_os = "macos"))]
    eprintln!("REFUSED: this default-off diagnostic requires macOS.");
    std::process::ExitCode::FAILURE
}
