use super::*;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use livi_aa_proto::MobileDeviceIdentity;
use livi_native_bluetooth::ComparisonCode;

const PEER: Address = Address([2, 1, 2, 3, 4, 5]);
const OTHER: Address = Address([2, 9, 8, 7, 6, 5]);

#[derive(Default)]
struct State {
    events: VecDeque<Event>,
    starts: usize,
    connects: usize,
    cancels: usize,
    stops: usize,
    unpaired: bool,
    pending: bool,
    fail_start: bool,
    connect_error: Option<Error>,
    detail: Option<DiagnosticFailure>,
    lifecycle: VecDeque<DiagnosticEvent>,
    snapshot: Option<CleanupSnapshot>,
    snapshot_reads: usize,
}
struct Fake(Rc<RefCell<State>>);
impl Radio for Fake {
    fn start(&mut self) -> Result<(), Error> {
        let mut s = self.0.borrow_mut(); s.starts += 1;
        if s.fail_start { Err(Error::Unavailable) } else { Ok(()) }
    }
    fn connect_paired(&mut self, generation: u64, peer: Address) -> Result<(), Error> {
        assert_eq!(generation, GENERATION);
        assert!(peer == PEER);
        let mut s = self.0.borrow_mut(); s.connects += 1;
        if let Some(error) = s.connect_error { Err(error) }
        else if s.unpaired { Err(Error::PairingRejected) } else { Ok(()) }
    }
    fn tick(&mut self, _: Duration) {}
    fn next_event(&mut self) -> Option<Event> { self.0.borrow_mut().events.pop_front() }
    fn cancel(&mut self, _: u64) { self.0.borrow_mut().cancels += 1; }
    fn stop(&mut self) { self.0.borrow_mut().stops += 1; }
    fn idle(&self) -> bool {
        let state = self.0.borrow();
        !state.pending && state.events.is_empty()
    }
    fn diagnostic_failure(&self) -> Option<DiagnosticFailure> { self.0.borrow().detail }
    fn next_diagnostic_event(&mut self) -> Option<DiagnosticEvent> {
        self.0.borrow_mut().lifecycle.pop_front()
    }
    fn cleanup_snapshot(&self) -> Option<CleanupSnapshot> {
        let mut state = self.0.borrow_mut();
        state.snapshot_reads += 1;
        state.snapshot
    }
}

fn setup() -> (Runner<Fake>, Rc<RefCell<State>>) {
    let state = Rc::new(RefCell::new(State::default()));
    let runner = Runner::new(Fake(state.clone()), Options { peer: PEER, authorized: true }, Duration::ZERO).unwrap();
    (runner, state)
}
fn connected(state: &Rc<RefCell<State>>, generation: u64, peer: Address) -> UnixStream {
    let (ours, remote) = UnixStream::pair().unwrap();
    remote.set_nonblocking(true).unwrap();
    state.borrow_mut().events.push_back(Event::Connected {
        generation, stream: ours.into(), peer, local: PEER,
    });
    remote
}
fn ready(state: &Rc<RefCell<State>>) {
    state.borrow_mut().events.push_back(Event::Availability(true));
}
fn response() -> WifiVersionResponse {
    WifiVersionResponse {
        major_version: Some(6), minor_version: Some(0), status: Some(0),
        device_serial: Some("synthetic-serial".into()),
        mobile_device_identity: Some(MobileDeviceIdentity {
            mobile_device_id: Some("synthetic-instance".into()), ..Default::default()
        }), ..Default::default()
    }
}
fn frame(id: u16, body: &[u8]) -> Vec<u8> {
    let mut bytes = (body.len() as u16).to_be_bytes().to_vec();
    bytes.extend_from_slice(&id.to_be_bytes()); bytes.extend_from_slice(body); bytes
}
fn drive(runner: &mut Runner<Fake>) {
    for _ in 0..12 { runner.step(Duration::from_millis(1), false); }
}
fn finish(runner: &mut Runner<Fake>) { runner.step(Duration::from_millis(2), false); }
fn request(remote: &mut UnixStream) {
    let mut bytes = [0; 8]; remote.read_exact(&mut bytes).unwrap();
    assert_eq!(bytes, [0, 4, 0, 4, 8, 6, 16, 0]);
}
fn eof(remote: &mut UnixStream) {
    assert_eq!(remote.read(&mut [0; 16]).unwrap(), 0);
}

#[test]
fn request_is_exactly_version_only_without_endpoint_or_capabilities() {
    assert_eq!(version_request(), [0, 4, 0, 4, 8, 6, 16, 0]);
    let decoded = WifiVersionRequest::decode(&version_request()[4..]).unwrap();
    assert!(decoded.wifi_projection_protocol_endpoint.is_none());
    assert!(decoded.car_info.is_none());
    assert!(decoded.supported_channel_frequencies_mhz.is_empty());
}
#[test]
fn transport_evidence_requires_selected_owned_fd_not_bond_or_sdp_observation() {
    let (mut runner, state) = setup();
    assert!(!runner.transport_observed());
    ready(&state);
    state.borrow_mut().lifecycle.push_back(lifecycle(DiagnosticLifecycle::QueryAccepted));
    runner.step(Duration::ZERO, false);
    assert_eq!(runner.stage(), Stage::Paired);
    assert!(!runner.transport_observed());
    let mut remote = connected(&state, GENERATION, PEER);
    runner.step(Duration::from_millis(1), false);
    assert!(runner.transport_observed());
    assert!(runner.result().is_none()); // fd acceptance is not protocol PASS.
    runner.step(Duration::from_millis(2), true);
    assert_eq!(runner.result(), Some(Err(Failure::Cancelled)));
    assert!(runner.transport_observed()); // Historical, stream is now closed.
    assert!(runner.stream.is_none());
    // A queued version request may precede EOF; consume it without requiring a response.
    let mut bytes = Vec::new();
    remote.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, version_request());
}

#[test]
fn rejected_or_stale_descriptor_never_becomes_transport_evidence() {
    for (generation, peer) in [(GENERATION + 1, PEER), (GENERATION, OTHER)] {
        let (mut runner, state) = setup(); ready(&state);
        let mut remote = connected(&state, generation, peer);
        runner.step(Duration::ZERO, false);
        assert!(!runner.transport_observed());
        eof(&mut remote);
    }
}

#[test]
fn complete_authorization_and_strict_options_required() {
    let good = ["--authorize-bluetooth-hardware", "--paired-phone-only", "--phone-address", "02:01:02:03:04:05"]
        .map(str::to_owned).to_vec();
    assert!(Options::parse(&good).is_ok());
    for flag in ["--authorize-bluetooth-hardware", "--paired-phone-only"] {
        let bad = good.iter().filter(|a| a.as_str() != flag).cloned().collect::<Vec<_>>();
        assert!(Options::parse(&bad).is_err());
    }
    for extra in ["--projection", "--isolated-helper", "--development-receiver", "--ap-config",
        "--seconds", "--authorize-bluetooth-hardware", "--paired-phone-only"] {
        let mut bad = good.clone(); bad.push(extra.into());
        assert!(Options::parse(&bad).is_err());
    }
    for address in ["", "00:00:00:00:00:00", "FF:FF:FF:FF:FF:FF", "03:01:02:03:04:05",
        "02:01:02:03:04", "02:01:02:03:04:0g", "02:01:02:03:04:05:06"] {
        let mut bad = good.clone(); bad[3] = address.into();
        assert!(Options::parse(&bad).is_err());
    }
}
#[test]
fn authorization_failure_never_starts_radio() {
    let state = Rc::new(RefCell::new(State::default()));
    assert!(Runner::new(Fake(state.clone()), Options { peer: PEER, authorized: false }, Duration::ZERO).is_err());
    assert_eq!(state.borrow().starts, 0);
}
#[test]
fn partial_start_failure_stops_owned_backend() {
    let state = Rc::new(RefCell::new(State { fail_start: true, ..Default::default() }));
    assert!(Runner::new(Fake(state.clone()), Options { peer: PEER, authorized: true }, Duration::ZERO).is_err());
    assert_eq!(state.borrow().stops, 1);
}
#[test]
fn positive_fragmented_identity_response_closes_without_further_writes() {
    let (mut runner, state) = setup(); ready(&state);
    let mut remote = connected(&state, GENERATION, PEER);
    runner.step(Duration::ZERO, false); request(&mut remote);
    let bytes = frame(5, &response().encode_to_vec());
    for byte in bytes {
        remote.write_all(&[byte]).unwrap();
        runner.step(Duration::from_millis(1), false);
    }
    drive(&mut runner);
    assert_eq!(runner.result(), Some(Ok(()))); eof(&mut remote);
    assert_eq!(state.borrow().connects, 1);
}
#[test]
fn invalid_response_version_status_and_identity_fail() {
    let valid = response();
    let mut cases = vec![WifiVersionResponse::default()];
    for (major, minor, status) in [(None, Some(0), Some(0)), (Some(5), Some(0), Some(0)),
        (Some(6), None, Some(0)), (Some(6), Some(1), Some(0)), (Some(6), Some(0), None),
        (Some(6), Some(0), Some(-1))] {
        let mut r = valid.clone(); r.major_version = major; r.minor_version = minor; r.status = status;
        cases.push(r);
    }
    for text in ["".to_owned(), " ".into(), "line\n".into(), "\0".into(), "é".into(), "x".repeat(129)] {
        let mut r = valid.clone(); r.device_serial = Some(text.clone()); cases.push(r);
        let mut r = valid.clone(); r.mobile_device_identity.as_mut().unwrap().mobile_device_id = Some(text);
        cases.push(r);
    }
    let mut r = valid.clone(); r.device_serial = None; cases.push(r);
    let mut r = valid.clone(); r.mobile_device_identity = None; cases.push(r);
    let mut r = valid.clone(); r.mobile_device_identity.as_mut().unwrap().mobile_device_id = None; cases.push(r);
    for case in cases { assert_eq!(validate_response(&case.encode_to_vec()), Err(Failure::Protocol)); }
    assert!(validate_response(&valid.encode_to_vec()).is_ok());
    assert_eq!(validate_response(&[0xff]), Err(Failure::Protocol));
    assert_eq!(validate_response(&vec![0; MAX_BODY + 1]), Err(Failure::Protocol));
}
#[test]
fn all_unexpected_message_ids_close_without_reply() {
    for id in [1, 2, 3, 4, 6, 7, 8, 9, 10, 11, 65535] {
        let (mut runner, state) = setup(); ready(&state);
        let mut remote = connected(&state, GENERATION, PEER);
        runner.step(Duration::ZERO, false); request(&mut remote);
        remote.write_all(&frame(id, &[8, 0])).unwrap(); drive(&mut runner);
        assert_eq!(runner.result(), Some(Err(Failure::Protocol))); eof(&mut remote);
    }
}
#[test]
fn oversized_and_empty_headers_fail_before_body_allocation() {
    for header in [[0x10, 1, 0, 5], [0xff, 0xff, 0, 5], [0, 0, 0, 5]] {
        let (mut runner, state) = setup(); ready(&state);
        let mut remote = connected(&state, GENERATION, PEER);
        runner.step(Duration::ZERO, false); request(&mut remote);
        remote.write_all(&header).unwrap(); drive(&mut runner);
        assert_eq!(runner.result(), Some(Err(Failure::Protocol)));
        assert!(runner.exchange.body.is_empty()); eof(&mut remote);
    }
}
#[test]
fn malformed_protobuf_fails_without_bootstrap() {
    let (mut runner, state) = setup(); ready(&state);
    let mut remote = connected(&state, GENERATION, PEER);
    runner.step(Duration::ZERO, false); request(&mut remote);
    remote.write_all(&frame(5, &[0xff])).unwrap(); drive(&mut runner);
    assert_eq!(runner.result(), Some(Err(Failure::Protocol))); eof(&mut remote);
}
#[test]
fn duplicate_coalesced_responses_cannot_send_more_requests() {
    let (mut runner, state) = setup(); ready(&state);
    let mut remote = connected(&state, GENERATION, PEER);
    runner.step(Duration::ZERO, false); request(&mut remote);
    let bytes = frame(5, &response().encode_to_vec());
    remote.write_all(&[bytes.clone(), bytes].concat()).unwrap(); drive(&mut runner);
    assert_eq!(runner.result(), Some(Ok(())));
    // Peer may observe reset rather than EOF because the duplicate is deliberately unread.
    assert!(matches!(remote.read(&mut [0; 8]), Ok(0) | Err(_)));
    assert_eq!(runner.exchange.written, 8);
}
#[test]
fn unpaired_target_never_retries_or_opens_stream() {
    let (mut runner, state) = setup(); state.borrow_mut().unpaired = true; ready(&state);
    drive(&mut runner);
    assert_eq!(runner.result(), Some(Err(Failure::Pairing)));
    ready(&state); drive(&mut runner);
    assert_eq!(state.borrow().connects, 1);
}
#[test]
fn pairing_events_always_cancel_without_confirmation_api() {
    for passkey in [false, true] {
        let (mut runner, state) = setup(); ready(&state);
        state.borrow_mut().events.push_back(if passkey {
            Event::Passkey { generation: GENERATION, code: ComparisonCode(123456) }
        } else { Event::Confirmation { generation: GENERATION, code: ComparisonCode(123456) } });
        drive(&mut runner);
        assert_eq!(runner.result(), Some(Err(Failure::Pairing)));
    }
}
#[test]
fn wrong_peer_and_second_connection_fail_and_close_only_owned_fds() {
    for second in [false, true] {
        let (mut runner, state) = setup(); ready(&state);
        let mut first = if second {
            connected(&state, GENERATION, PEER)
        } else { connected(&state, GENERATION, OTHER) };
        let mut extra = if second { Some(connected(&state, GENERATION, PEER)) } else { None };
        drive(&mut runner);
        assert_eq!(runner.result(), Some(Err(Failure::Peer))); eof(&mut first);
        if let Some(stream) = &mut extra { eof(stream); }
    }
}
#[test]
fn stale_arrival_is_closed_without_interrupting_current_attempt() {
    let (mut runner, state) = setup(); ready(&state);
    let mut stale = connected(&state, GENERATION + 1, PEER);
    let mut current = connected(&state, GENERATION, PEER);
    runner.step(Duration::ZERO, false); eof(&mut stale); request(&mut current);
    current.write_all(&frame(5, &response().encode_to_vec())).unwrap(); drive(&mut runner);
    assert_eq!(runner.result(), Some(Ok(())));
}
#[test]
fn unsolicited_arrival_before_selection_is_rejected() {
    let (mut runner, state) = setup();
    let mut remote = connected(&state, GENERATION, PEER); drive(&mut runner);
    assert_eq!(runner.result(), Some(Err(Failure::Peer))); eof(&mut remote);
}
#[test]
fn silence_and_slow_partial_frames_do_not_extend_version_deadline() {
    for bytes in [vec![], vec![0], vec![0, 20, 0, 5, 8]] {
        let (mut runner, state) = setup(); ready(&state);
        let mut remote = connected(&state, GENERATION, PEER);
        runner.step(Duration::ZERO, false); request(&mut remote);
        remote.write_all(&bytes).unwrap();
        runner.step(VERSION - Duration::from_millis(1), false);
        runner.step(VERSION, false); runner.step(VERSION, false);
        assert_eq!(runner.result(), Some(Err(Failure::Timeout))); eof(&mut remote);
    }
}
#[test]
fn overall_deadline_covers_startup_and_connection() {
    for selected in [false, true] {
        let (mut runner, state) = setup();
        if selected { ready(&state); runner.step(Duration::ZERO, false); }
        runner.step(OVERALL, false); runner.step(OVERALL, false);
        assert_eq!(runner.result(), Some(Err(Failure::Timeout)));
    }
}
#[test]
fn version_phase_cannot_extend_overall_deadline() {
    let (mut runner, state) = setup(); ready(&state);
    let _remote = connected(&state, GENERATION, PEER);
    runner.step(OVERALL - Duration::from_secs(1), false);
    runner.step(OVERALL, false); runner.step(OVERALL, false);
    assert_eq!(runner.result(), Some(Err(Failure::Timeout)));
}
#[test]
fn cancellation_wins_even_with_valid_response_queued() {
    let (mut runner, state) = setup(); ready(&state);
    let mut remote = connected(&state, GENERATION, PEER);
    runner.step(Duration::ZERO, false); request(&mut remote);
    remote.write_all(&frame(5, &response().encode_to_vec())).unwrap();
    runner.step(Duration::from_millis(1), true);
    assert_eq!(runner.result(), Some(Err(Failure::Cancelled)));
    assert_eq!(runner.exchange.body_read, 0);
}
#[test]
fn cancellation_at_startup_selection_write_header_and_body_is_owned() {
    for phase in 0..5 {
        let (mut runner, state) = setup();
        if phase > 0 { ready(&state); }
        let mut remote = if phase > 1 { Some(connected(&state, GENERATION, PEER)) } else { None };
        if phase > 0 { runner.step(Duration::ZERO, false); }
        if phase > 2 {
            let remote = remote.as_mut().unwrap(); request(remote);
            remote.write_all(&[0, 20, 0, 5]).unwrap();
            runner.step(Duration::from_millis(1), false);
        }
        if phase > 3 {
            remote.as_mut().unwrap().write_all(&[8]).unwrap();
            runner.step(Duration::from_millis(1), false);
        }
        runner.step(Duration::from_millis(2), true);
        assert_eq!(runner.result(), Some(Err(Failure::Cancelled)));
        assert!(runner.stream.is_none());
        assert!(state.borrow().stops > 0);
    }
}
#[test]
fn cleanup_waits_for_native_callbacks_and_never_falsely_passes() {
    let (mut runner, state) = setup(); state.borrow_mut().pending = true;
    runner.step(Duration::ZERO, true); assert_eq!(runner.result(), None);
    runner.step(CLEANUP, false);
    assert_eq!(runner.result(), Some(Err(Failure::Cleanup)));
}
#[test]
fn cleanup_completes_when_pending_callback_releases_and_drains_stale_fd() {
    let (mut runner, state) = setup(); state.borrow_mut().pending = true;
    runner.step(Duration::ZERO, true);
    let mut late = connected(&state, GENERATION, PEER);
    state.borrow_mut().pending = false; finish(&mut runner);
    eof(&mut late); assert_eq!(runner.result(), Some(Err(Failure::Cancelled)));
}
#[test]
fn drop_and_repeated_steps_do_not_reconnect_or_write() {
    let (mut runner, state) = setup(); ready(&state);
    let mut remote = connected(&state, GENERATION, PEER);
    runner.step(Duration::ZERO, false); request(&mut remote);
    drop(runner); eof(&mut remote);
    assert!(state.borrow().stops > 0);
    assert_eq!(state.borrow().connects, 1);
}
#[test]
fn transport_eof_and_radio_failure_fail_closed() {
    for radio_failure in [false, true] {
        let (mut runner, state) = setup(); ready(&state);
        let remote = connected(&state, GENERATION, PEER);
        runner.step(Duration::ZERO, false);
        if radio_failure {
            state.borrow_mut().events.push_back(Event::Failure { generation: GENERATION, error: Error::Transport });
        } else { drop(remote); }
        drive(&mut runner);
        assert_eq!(runner.result(), Some(Err(if radio_failure { Failure::Radio } else { Failure::Transport })));
    }
}
#[test]
fn write_backpressure_is_bounded_by_absolute_version_deadline() {
    let (ours, mut remote) = UnixStream::pair().unwrap();
    ours.set_nonblocking(true).unwrap(); remote.set_nonblocking(true).unwrap();
    let mut filler = &ours;
    let block = [0u8; 4096];
    while filler.write(&block).is_ok() {}
    let mut exchange = Exchange::new();
    assert_eq!(exchange.poll(&mut { &ours }.try_clone().unwrap()).unwrap(), None);
    assert_eq!(exchange.written, 0);
    let (mut runner, state) = setup(); ready(&state);
    state.borrow_mut().events.push_back(Event::Connected {
        generation: GENERATION, stream: ours.into(), peer: PEER, local: PEER,
    });
    runner.step(Duration::ZERO, false); assert_eq!(runner.exchange.written, 0);
    runner.step(VERSION, false); runner.step(VERSION, false);
    assert_eq!(runner.result(), Some(Err(Failure::Timeout)));
    let mut captured = Vec::new(); let _ = remote.read_to_end(&mut captured);
    assert!(captured.iter().all(|b| *b == 0));
}

struct ShortIo {
    writes: Vec<u8>,
    calls: usize,
}
impl Read for ShortIo {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::ErrorKind::WouldBlock.into())
    }
}
impl Write for ShortIo {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.calls += 1;
        if self.calls == 1 { return Err(io::ErrorKind::Interrupted.into()); }
        if self.calls == 2 { return Err(io::ErrorKind::WouldBlock.into()); }
        let size = bytes.len().min(2);
        self.writes.extend_from_slice(&bytes[..size]);
        Ok(size)
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}
#[test]
fn short_writes_and_interruptions_never_duplicate_the_single_request() {
    let mut exchange = Exchange::new();
    let mut stream = ShortIo { writes: Vec::new(), calls: 0 };
    for _ in 0..30 { exchange.poll(&mut stream).unwrap(); }
    assert_eq!(stream.writes, version_request());
    assert_eq!(stream.calls, 6);
}
#[test]
fn event_flood_cannot_starve_absolute_cancellation_or_deadline() {
    let (mut runner, state) = setup();
    for _ in 0..200 { state.borrow_mut().events.push_back(Event::Availability(true)); }
    runner.step(Duration::ZERO, false);
    assert_eq!(state.borrow().connects, 1);
    assert_eq!(state.borrow().events.len(), 136);
    runner.step(OVERALL, false);
    assert_eq!(runner.result(), None);
    for _ in 0..3 { runner.step(OVERALL, false); }
    assert_eq!(runner.result(), Some(Err(Failure::Timeout)));
}
#[test]
fn stale_failure_and_pairing_events_cannot_end_the_current_attempt() {
    let (mut runner, state) = setup(); ready(&state);
    state.borrow_mut().events.extend([
        Event::Failure { generation: 2, error: Error::Transport },
        Event::Closed { generation: 2 },
        Event::Confirmation { generation: 2, code: ComparisonCode(123456) },
    ]);
    let mut remote = connected(&state, GENERATION, PEER);
    runner.step(Duration::ZERO, false); request(&mut remote);
    remote.write_all(&frame(5, &response().encode_to_vec())).unwrap(); drive(&mut runner);
    assert_eq!(runner.result(), Some(Ok(())));
}

#[test]
fn cancellation_after_validation_but_before_cleanup_wins() {
    let (mut runner, state) = setup(); ready(&state);
    let mut remote = connected(&state, GENERATION, PEER);
    runner.step(Duration::ZERO, false); request(&mut remote);
    state.borrow_mut().pending = true;
    remote.write_all(&frame(5, &response().encode_to_vec())).unwrap(); drive(&mut runner);
    assert_eq!(runner.outcome, Some(Ok(())));
    assert_eq!(runner.result(), None);
    state.borrow_mut().pending = false;
    runner.step(Duration::from_millis(2), true);
    assert_eq!(runner.result(), Some(Err(Failure::Cancelled)));
}

#[test]
fn successful_response_with_stuck_cleanup_is_not_a_pass() {
    let (mut runner, state) = setup(); ready(&state);
    let mut remote = connected(&state, GENERATION, PEER);
    runner.step(Duration::ZERO, false); request(&mut remote);
    state.borrow_mut().pending = true;
    remote.write_all(&frame(5, &response().encode_to_vec())).unwrap(); drive(&mut runner);
    assert_eq!(runner.outcome, Some(Ok(())));
    runner.step(CLEANUP + Duration::from_secs(1), false);
    assert_eq!(runner.result(), Some(Err(Failure::Cleanup)));
}

#[test]
fn synchronous_connection_categories_are_distinct_and_never_retry() {
    use DiagnosticCategory as C;
    use DiagnosticOperation as O;
    for (error, category, operation) in [
        (Error::Unavailable, C::OwnerUnavailable, O::ConnectPaired),
        (Error::Busy, C::Busy, O::ConnectPaired),
        (Error::InvalidDevice, C::DeviceNotFound, O::DeviceLookup),
        (Error::PairingRejected, C::UnpairedTarget, O::BondCheck),
    ] {
        let (mut runner, state) = setup();
        state.borrow_mut().connect_error = Some(error);
        ready(&state); drive(&mut runner);
        assert_eq!(runner.result(), Some(Err(Failure::Pairing)));
        assert_eq!(runner.diagnostic_failure(), Some(DiagnosticFailure {
            category, operation, raw_ior_return: None,
        }));
        ready(&state); drive(&mut runner);
        assert_eq!(state.borrow().connects, 1);
        assert!(runner.stream.is_none());
    }
}

#[test]
fn every_native_category_and_raw_error_survives_terminal_event_and_cleanup() {
    use DiagnosticCategory as C;
    use DiagnosticOperation as O;
    for (category, operation) in [
        (C::OwnerUnavailable, O::ConnectPaired), (C::Busy, O::ConnectPaired),
        (C::DeviceNotFound, O::DeviceLookup), (C::UnpairedTarget, O::BondCheck),
        (C::Sdp, O::SdpQuery), (C::Rfcomm, O::RfcommComplete),
        (C::Timeout, O::NativeDeadline), (C::Permission, O::PermissionCheck),
        (C::Unavailable, O::Availability), (C::Transport, O::ConnectPaired),
        (C::Registration, O::Registration),
    ] {
        let (mut runner, state) = setup(); ready(&state);
        let detail = DiagnosticFailure { category, operation, raw_ior_return: Some(0xE00002E2) };
        state.borrow_mut().detail = Some(detail);
        state.borrow_mut().events.push_back(Event::Failure { generation: GENERATION, error: Error::Transport });
        drive(&mut runner);
        assert_eq!(runner.result(), Some(Err(Failure::Radio)));
        assert_eq!(runner.diagnostic_failure(), Some(detail));
        assert_eq!(runner.stage(), Stage::Stopped);
        assert_eq!(state.borrow().connects, 1);
        assert!(state.borrow().stops > 0);
    }
}

#[test]
fn startup_permission_detail_is_captured_before_stop() {
    let detail = DiagnosticFailure { category: DiagnosticCategory::Permission,
        operation: DiagnosticOperation::Start, raw_ior_return: None };
    let state = Rc::new(RefCell::new(State {
        fail_start: true, detail: Some(detail), ..Default::default()
    }));
    let result = Runner::new(Fake(state.clone()), Options { peer: PEER, authorized: true }, Duration::ZERO);
    assert!(matches!(result, Err(Failure::Native(value)) if value == detail));
    assert_eq!(state.borrow().stops, 1);
    assert_eq!(state.borrow().connects, 0);
}

#[test]
fn sdp_detail_survives_overall_timeout_without_changing_wait_behavior() {
    let (mut runner, state) = setup(); ready(&state);
    let detail = DiagnosticFailure { category: DiagnosticCategory::Sdp,
        operation: DiagnosticOperation::SdpRecord, raw_ior_return: None };
    state.borrow_mut().detail = Some(detail);
    runner.step(Duration::ZERO, false);
    assert_eq!(runner.result(), None);
    runner.step(OVERALL, false); runner.step(OVERALL, false);
    assert_eq!(runner.result(), Some(Err(Failure::Timeout)));
    assert_eq!(runner.diagnostic_failure(), Some(detail));
    assert_eq!(state.borrow().connects, 1);
}

#[test]
fn overall_and_version_deadlines_have_distinct_operations() {
    for version in [false, true] {
        let (mut runner, state) = setup();
        let _remote = if version { ready(&state); Some(connected(&state, GENERATION, PEER)) } else { None };
        runner.step(Duration::ZERO, false);
        let deadline = if version { VERSION } else { OVERALL };
        runner.step(deadline, false); runner.step(deadline, false);
        assert_eq!(runner.result(), Some(Err(Failure::Timeout)));
        assert_eq!(runner.diagnostic_failure(), Some(DiagnosticFailure {
            category: DiagnosticCategory::Timeout,
            operation: if version { DiagnosticOperation::VersionDeadline } else { DiagnosticOperation::OverallDeadline },
            raw_ior_return: None,
        }));
    }
}

#[test]
fn cleanup_failure_does_not_erase_native_error_or_claim_cleanup() {
    let (mut runner, state) = setup(); ready(&state);
    let detail = DiagnosticFailure { category: DiagnosticCategory::Rfcomm,
        operation: DiagnosticOperation::RfcommOpen, raw_ior_return: Some(0xE00002CD) };
    state.borrow_mut().detail = Some(detail);
    state.borrow_mut().pending = true;
    state.borrow_mut().events.push_back(Event::Failure { generation: GENERATION, error: Error::Transport });
    runner.step(Duration::ZERO, false); runner.step(CLEANUP, false);
    assert_eq!(runner.result(), Some(Err(Failure::Cleanup)));
    assert_eq!(runner.diagnostic_failure(), Some(detail));
    assert_eq!(state.borrow().connects, 1);
}

#[test]
fn captured_native_failure_never_overrides_cancellation() {
    let (mut runner, state) = setup(); ready(&state);
    state.borrow_mut().detail = Some(DiagnosticFailure {
        category: DiagnosticCategory::Sdp, operation: DiagnosticOperation::SdpQuery,
        raw_ior_return: Some(0xE00002BC),
    });
    runner.step(Duration::ZERO, true);
    assert_eq!(runner.result(), Some(Err(Failure::Cancelled)));
    assert_eq!(state.borrow().connects, 0);
}

#[test]
fn diagnostic_formatter_contains_only_fixed_tokens_and_numeric_error() {
    let detail = DiagnosticFailure { category: DiagnosticCategory::Rfcomm,
        operation: DiagnosticOperation::RfcommComplete, raw_ior_return: Some(0xE00002CD) };
    assert_eq!(detail.to_string(), "category=Rfcomm operation=RfcommComplete IOReturn=0xE00002CD");
    let detail = DiagnosticFailure { raw_ior_return: None, ..detail };
    assert_eq!(detail.to_string(), "category=Rfcomm operation=RfcommComplete IOReturn=unavailable");
    let source = include_str!("main.rs");
    for forbidden in ["options.peer", "args:?"] { assert!(!source.contains(forbidden)); }
}

fn lifecycle(kind: DiagnosticLifecycle) -> DiagnosticEvent {
    DiagnosticEvent { kind, raw_ior_return: None }
}

#[test]
fn lifecycle_late_callback_is_collected_before_final_cleanup_result() {
    let (mut runner, state) = setup();
    state.borrow_mut().pending = true;
    state.borrow_mut().lifecycle.push_back(lifecycle(DiagnosticLifecycle::QueryInitiated));
    runner.step(Duration::ZERO, true);
    assert_eq!(runner.result(), None);
    state.borrow_mut().lifecycle.extend([
        lifecycle(DiagnosticLifecycle::CallbackEntered),
        lifecycle(DiagnosticLifecycle::CallbackLate),
    ]);
    state.borrow_mut().pending = false;
    runner.step(Duration::from_millis(1), false);
    assert_eq!(runner.result(), Some(Err(Failure::Cancelled)));
    assert_eq!(runner.diagnostic_transcript().iter().map(|e| e.kind).collect::<Vec<_>>(),
        [DiagnosticLifecycle::QueryInitiated, DiagnosticLifecycle::CallbackEntered,
         DiagnosticLifecycle::CallbackLate]);
    assert_eq!(state.borrow().connects, 0);
}

#[test]
fn lifecycle_flood_is_bounded_and_cannot_starve_cancellation_or_cleanup_deadline() {
    for cancel in [false, true] {
        let (mut runner, state) = setup();
        state.borrow_mut().pending = true;
        state.borrow_mut().lifecycle.extend(
            std::iter::repeat_n(lifecycle(DiagnosticLifecycle::CallbackEntered), 1000));
        runner.step(OVERALL, cancel);
        assert_eq!(state.borrow().lifecycle.len(), 936);
        runner.step(OVERALL + CLEANUP, false);
        assert_eq!(runner.result(), Some(Err(Failure::Cleanup)));
        assert_eq!(runner.outcome, Some(Err(if cancel { Failure::Cancelled } else { Failure::Timeout })));
        assert_eq!(runner.diagnostic_transcript().len(), 64);
        assert_eq!(runner.diagnostic_transcript()[63].kind, DiagnosticLifecycle::Overflow);
        assert_eq!(state.borrow().connects, 0);
    }
}

#[test]
fn lifecycle_native_overflow_is_sticky_and_not_an_outcome() {
    let (mut runner, state) = setup();
    state.borrow_mut().lifecycle.extend([
        lifecycle(DiagnosticLifecycle::QueryAccepted),
        lifecycle(DiagnosticLifecycle::Overflow),
        lifecycle(DiagnosticLifecycle::ServiceRecordMissing),
    ]);
    runner.step(Duration::ZERO, false);
    assert_eq!(runner.result(), None);
    assert_eq!(runner.diagnostic_transcript().len(), 2);
    runner.step(OVERALL, false);
    assert_eq!(runner.result(), Some(Err(Failure::Timeout)));
    assert_eq!(runner.diagnostic_transcript().len(), 2);
}

#[test]
fn lifecycle_collection_preserves_version_deadline_and_raw_status() {
    let (mut runner, state) = setup(); ready(&state);
    let _remote = connected(&state, GENERATION, PEER);
    let event = DiagnosticEvent { kind: DiagnosticLifecycle::CallbackAccepted,
        raw_ior_return: Some(0xE00002CD) };
    state.borrow_mut().lifecycle.push_back(event);
    runner.step(Duration::ZERO, false);
    runner.step(VERSION, false);
    assert_eq!(runner.result(), Some(Err(Failure::Timeout)));
    assert_eq!(runner.diagnostic_transcript(), [event]);
    assert_eq!(state.borrow().connects, 1);
}

#[test]
fn cleanup_timeout_captures_exact_snapshot_once_even_after_overflow() {
    let (mut runner, state) = setup();
    let snapshot = CleanupSnapshot::from_bits(1).unwrap();
    state.borrow_mut().pending = true;
    state.borrow_mut().snapshot = Some(snapshot);
    state.borrow_mut().lifecycle.push_back(lifecycle(DiagnosticLifecycle::Overflow));
    runner.step(Duration::ZERO, true);
    assert_eq!(state.borrow().snapshot_reads, 0);
    runner.step(CLEANUP, false);
    assert_eq!(runner.result(), Some(Err(Failure::Cleanup)));
    assert_eq!(runner.outcome, Some(Err(Failure::Cancelled)));
    let report = runner.cleanup_report().unwrap();
    assert_eq!(report.native, Some(snapshot));
    assert!(!report.runner_stream_owned);
    assert!(report.to_string().contains("pending_sdp_callback=true"));
    assert!(report.to_string().contains("pending_open_callback=false"));
    state.borrow_mut().snapshot = CleanupSnapshot::from_bits(0);
    state.borrow_mut().pending = false;
    runner.step(CLEANUP + Duration::from_secs(1), false);
    assert_eq!(runner.cleanup_report(), Some(report));
    assert_eq!(state.borrow().snapshot_reads, 1);
    assert_eq!(state.borrow().connects, 0);
}

#[test]
fn cleanup_timeout_reports_unavailable_not_all_clear() {
    let (mut runner, state) = setup();
    state.borrow_mut().pending = true;
    runner.step(OVERALL, false);
    runner.step(OVERALL + CLEANUP, false);
    let report = runner.cleanup_report().unwrap();
    assert_eq!(report.native, None);
    assert!(report.to_string().ends_with("native_snapshot=unavailable"));
    assert_eq!(runner.result(), Some(Err(Failure::Cleanup)));
    assert_eq!(runner.outcome, Some(Err(Failure::Timeout)));
}

#[test]
fn cleanup_callback_release_before_deadline_does_not_capture_failure_snapshot() {
    let (mut runner, state) = setup();
    state.borrow_mut().pending = true;
    runner.step(Duration::ZERO, true);
    state.borrow_mut().pending = false;
    runner.step(Duration::from_millis(1), false);
    assert_eq!(runner.result(), Some(Err(Failure::Cancelled)));
    assert_eq!(runner.cleanup_report(), None);
    assert_eq!(state.borrow().snapshot_reads, 0);
}
