// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

const FD_PROBE_CHILD: &str = "LIVI_R1_FD_PROBE_CHILD";
const FD_PROBE_FIXTURE: &str = "LIVI_R1_FD_PROBE_FIXTURE";
const FD_PROBE_LIMIT: Duration = Duration::from_secs(10);
const FD_PROBE_POLL: Duration = Duration::from_millis(10);
const FD_PROBE_CAPTURE_LIMIT: usize = 64 * 1024;

fn fd_selector(name: &str) -> String {
    // libtest names omit the crate prefix present in module_path!().
    let (_, module) = module_path!().split_once("::").expect("crate-qualified test module");
    format!("{module}::{name}")
}

fn fd_child_mode(selector: &str) -> Result<bool, &'static str> {
    match std::env::var_os(FD_PROBE_CHILD) {
        None => Ok(false),
        Some(child) if child == std::ffi::OsStr::new(selector) => Ok(true),
        Some(_) => Err("wrong FD probe child selector"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FdOutcome {
    Passed,
    Timeout,
    AssertionFailure,
    AbnormalExit,
    EmptySelection,
    InvalidSummary,
}

fn fd_outcome(timed_out: bool, success: bool, code: Option<i32>, stdout: &str) -> FdOutcome {
    if timed_out {
        FdOutcome::Timeout
    } else if !success {
        if code == Some(101) && stdout.contains("1 failed;") {
            FdOutcome::AssertionFailure
        } else {
            FdOutcome::AbnormalExit
        }
    } else if stdout.contains("running 0 tests") {
        FdOutcome::EmptySelection
    } else if stdout.contains("running 1 test") && stdout.contains("1 passed; 0 failed;") {
        FdOutcome::Passed
    } else {
        FdOutcome::InvalidSummary
    }
}

#[derive(Debug)]
struct FdReport {
    outcome: FdOutcome,
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
    cleanup_errors: Vec<String>,
}

// Own the exact Child from spawn through confirmed reaping. No assertion or
// fallible output handling may bypass cleanup after a successful spawn.
struct FdChild {
    child: std::process::Child,
    reaped: bool,
}

impl FdChild {
    fn terminate_and_reap(&mut self) -> (std::process::ExitStatus, Vec<String>) {
        let mut errors = Vec::new();
        if let Err(error) = self.child.kill() {
            // Exit can race kill. Still wait on this exact owned child.
            errors.push(format!("termination request: {error}"));
        }
        // The user-approved exception applies only after termination is
        // requested: this final OS cleanup can wait without a hard deadline.
        loop {
            match self.child.wait() {
                Ok(status) => {
                    self.reaped = true;
                    return (status, errors);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => {
                    if errors.len() < 8 {
                        errors.push(format!("reap: {error}"));
                    }
                    // Never drop a potentially live/unreaped child on an OS
                    // error. Retry exact-child termination and reaping.
                    let _ = self.child.kill();
                    std::thread::sleep(FD_PROBE_POLL);
                }
            }
        }
    }
}

impl Drop for FdChild {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.terminate_and_reap();
        }
    }
}

fn fd_nonblocking(pipe: &impl AsRawFd) -> std::io::Result<()> {
    let fd = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn fd_capture(pipe: &mut impl std::io::Read, output: &mut Vec<u8>) -> std::io::Result<()> {
    let mut bytes = [0; 4096];
    // Limit each polling turn even if the child floods output. Retain at most
    // 64 KiB per pipe, draining/discarding excess rather than blocking the child.
    for _ in 0..32 {
        match pipe.read(&mut bytes) {
            Ok(0) => break,
            Ok(count) => {
                let keep = count.min(FD_PROBE_CAPTURE_LIMIT.saturating_sub(output.len()));
                output.extend_from_slice(&bytes[..keep]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn run_fd_child(selector: &str, marker: &str, fixture: Option<&str>) -> std::io::Result<FdReport> {
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .arg("--exact")
        .arg(selector)
        .arg("--test-threads=1")
        .arg("--nocapture")
        .env(FD_PROBE_CHILD, marker)
        .env_remove(FD_PROBE_FIXTURE)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(fixture) = fixture {
        command.env(FD_PROBE_FIXTURE, fixture);
    }
    let mut owned = FdChild { child: command.spawn()?, reaped: false };
    // Spawn itself is an OS operation, not subject to a hard real-time bound.
    // The child execution deadline starts immediately after successful spawn.
    let started = Instant::now();
    fd_nonblocking(owned.child.stdout.as_ref().expect("piped stdout"))?;
    fd_nonblocking(owned.child.stderr.as_ref().expect("piped stderr"))?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let (status, timed_out, mut cleanup_errors) = loop {
        if started.elapsed() >= FD_PROBE_LIMIT {
            let (status, errors) = owned.terminate_and_reap();
            break (status, true, errors);
        }
        if let Some(status) = owned.child.try_wait()? {
            owned.reaped = true;
            break (status, false, Vec::new());
        }
        fd_capture(owned.child.stdout.as_mut().expect("piped stdout"), &mut stdout)?;
        fd_capture(owned.child.stderr.as_mut().expect("piped stderr"), &mut stderr)?;
        std::thread::sleep(FD_PROBE_POLL.min(FD_PROBE_LIMIT.saturating_sub(started.elapsed())));
    };
    // Pipes are nonblocking, and these children do not spawn descendants.
    // Diagnostic capture failure must not erase a known timeout/exit outcome.
    if let Err(error) = fd_capture(owned.child.stdout.as_mut().expect("piped stdout"), &mut stdout) {
        cleanup_errors.push(format!("final stdout capture: {error}"));
    }
    if let Err(error) = fd_capture(owned.child.stderr.as_mut().expect("piped stderr"), &mut stderr) {
        cleanup_errors.push(format!("final stderr capture: {error}"));
    }
    let stdout = String::from_utf8_lossy(&stdout).into_owned();
    let stderr = String::from_utf8_lossy(&stderr).into_owned();
    Ok(FdReport {
        outcome: fd_outcome(timed_out, status.success(), status.code(), &stdout),
        status,
        stdout,
        stderr,
        cleanup_errors,
    })
}

fn isolated_fd_probe(name: &str) -> bool {
    let selector = fd_selector(name);
    if fd_child_mode(&selector).expect("FD child marker validation") {
        return true;
    }
    // Concurrent parent tests have separate descriptor tables from each child.
    // The exact selected probe itself is synchronous and starts no background tasks.
    let report = run_fd_child(&selector, &selector, None)
        .expect("FD probe infrastructure failure (owned child cleaned up)");
    assert_eq!(
        report.outcome, FdOutcome::Passed,
        "isolated FD probe {selector}: status={:?}, cleanup={:?}\n{}\n{}",
        report.status, report.cleanup_errors, report.stdout, report.stderr,
    );
    false
}

#[test]
fn fd_result_validation_distinguishes_each_failure() {
    let passed = "running 1 test\n1 passed; 0 failed;";
    assert_eq!(fd_outcome(false, true, Some(0), passed), FdOutcome::Passed);
    assert_eq!(fd_outcome(true, false, None, ""), FdOutcome::Timeout);
    assert_eq!(fd_outcome(true, true, Some(0), passed), FdOutcome::Timeout);
    assert_eq!(
        fd_outcome(false, false, Some(101), "0 passed; 1 failed;"),
        FdOutcome::AssertionFailure,
    );
    assert_eq!(fd_outcome(false, false, Some(73), passed), FdOutcome::AbnormalExit);
    assert_eq!(fd_outcome(false, false, None, ""), FdOutcome::AbnormalExit);
    assert_eq!(fd_outcome(false, false, Some(101), ""), FdOutcome::AbnormalExit);
    assert_eq!(
        fd_outcome(false, true, Some(0), "running 0 tests\n0 passed; 0 failed;"),
        FdOutcome::EmptySelection,
    );
    assert_eq!(fd_outcome(false, true, Some(0), "1 passed; 0 failed;"), FdOutcome::InvalidSummary);
    assert_eq!(fd_outcome(false, true, Some(0), "running 1 test"), FdOutcome::InvalidSummary);
}

// Intentional failures activate only in an explicitly selected, marked child.
// Normal discovery/invocation returns without assertions, exits, or waiting.
#[test]
fn fd_probe_child_fixture() {
    let Some(mode) = std::env::var_os(FD_PROBE_FIXTURE) else { return };
    assert!(
        fd_child_mode(&fd_selector("fd_probe_child_fixture")).expect("fixture child marker"),
        "fixture requires child mode",
    );
    match mode.to_str().expect("fixture mode") {
        "assertion" => assert_eq!(0, 1, "intentional FD child assertion"),
        "abnormal" => std::process::exit(73),
        "timeout" => loop { std::thread::park(); },
        "success" => {}
        _ => panic!("unknown FD child fixture"),
    }
}

#[test]
fn fd_subprocess_negative_paths_reap_before_reporting() {
    let selector = fd_selector("fd_probe_child_fixture");
    for (fixture, expected) in [
        ("success", FdOutcome::Passed),
        ("assertion", FdOutcome::AssertionFailure),
        ("abnormal", FdOutcome::AbnormalExit),
        ("timeout", FdOutcome::Timeout),
    ] {
        let report = run_fd_child(&selector, &selector, Some(fixture)).unwrap();
        assert_eq!(report.outcome, expected, "{report:?}");
    }
    let missing = fd_selector("no_such_batch2_fd_probe");
    let report = run_fd_child(&missing, &missing, None).unwrap();
    assert_eq!(report.outcome, FdOutcome::EmptySelection, "{report:?}");
    let report = run_fd_child(&selector, "wrong-selector", Some("success")).unwrap();
    assert_eq!(report.outcome, FdOutcome::AssertionFailure, "{report:?}");
    assert!(report.stderr.contains("wrong FD probe child selector"), "{report:?}");
}

#[test]
fn concurrent_parent_probes_use_exact_isolated_children() {
    std::thread::scope(|scope| {
        let probes: Vec<_> = [
            "cancel_ack_eof_permutations_verify_known_alias_ownership",
            "receiver_eof_revokes_transport_and_releases_known_channel_owners",
        ].into_iter().map(|name| scope.spawn(move || {
            let selector = fd_selector(name);
            run_fd_child(&selector, &selector, None)
        })).collect();
        // Join every parent worker before asserting, even if one child failed.
        let results: Vec<_> = probes.into_iter().map(|probe| probe.join()).collect();
        for result in results {
            let report = result.expect("parent probe worker").expect("child infrastructure");
            assert_eq!(report.outcome, FdOutcome::Passed, "{report:?}");
        }
    });
}

// Raw-number closure probes are intentionally adjacent to synchronous teardown:
// no awaits or intervening descriptor allocation. Both entry points enforce a
// fresh exact-test child, not merely a request to serialize the parent runner.
// These checks cover known aliases, not process-wide/kernel-queue leak detection.
// They do not claim isolation from arbitrary externally injected child code.
fn descriptor_open(fd: libc::c_int) {
    assert!(unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0);
}

fn descriptor_closed(fd: libc::c_int) {
    let result = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    let error = std::io::Error::last_os_error();
    assert_eq!(result, -1, "known descriptor must be closed, not merely shut down");
    assert_eq!(error.raw_os_error(), Some(libc::EBADF));
}

fn drop_descriptor<T: AsRawFd>(owner: T) {
    let fd = owner.as_raw_fd();
    drop(owner);
    descriptor_closed(fd);
}

fn identity(fd: &OwnedFd) -> (libc::dev_t, libc::ino_t) {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    assert_eq!(unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) }, 0);
    let stat = unsafe { stat.assume_init() };
    (stat.st_dev, stat.st_ino)
}

fn live(stream: &mut UnixStream, phone: &mut UnixStream) {
    stream.write_all(b"a").unwrap();
    let mut byte = [0];
    phone.read_exact(&mut byte).unwrap();
    assert_eq!(byte, [b'a']);
    phone.write_all(b"b").unwrap();
    stream.read_exact(&mut byte).unwrap();
    assert_eq!(byte, [b'b']);
}

#[test]
fn cancel_ack_eof_permutations_verify_known_alias_ownership() {
    if !isolated_fd_probe("cancel_ack_eof_permutations_verify_known_alias_ownership") {
        return;
    }
    // Synthetic synchronous order, not an exhaustive scheduling matrix:
    // cancel before receive; cancel after receiver ACK; cancel after sender ACK;
    // sender EOF before ACK; sender EOF after ACK.
    for boundary in 0..5 {
        let (mut tx, mut rx) = super::super::tests::pair();
        let mut phone = arrival(&mut tx, 7);
        let pending_fd = tx.pending.as_ref().unwrap().stream.as_raw_fd();
        let original = identity(&tx.pending.as_ref().unwrap().stream);
        let (mut sentinel, mut sentinel_peer) = UnixStream::pair().unwrap();
        sentinel_peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        if boundary == 0 {
            tx.send(Message::Cancel { generation: 7 }).unwrap();
            descriptor_closed(pending_fd);
            assert!(tx.pending.is_none());
            let receiver_ipc = rx.stream.as_ref().unwrap().as_raw_fd();
            // Shutdown revokes queued aliases before getpeername validation.
            assert_eq!(rx.receive().err(), Some(Error::Descriptor));
            descriptor_closed(receiver_ipc);
            assert!(rx.poisoned);
            assert!(rx.rights.is_empty());
            assert!(rx.stream.is_none());
            closed(phone);
        } else {
            let received = adopted(&mut rx);
            assert_eq!(identity(&received), original);
            let received_fd = received.as_raw_fd();
            descriptor_open(pending_fd);
            descriptor_open(received_fd);
            assert_ne!(pending_fd, received_fd);
            assert!(tx.pending.is_some());
            if boundary == 1 {
                tx.send(Message::Cancel { generation: 7 }).unwrap();
                descriptor_closed(pending_fd);
                assert!(tx.pending.is_none());
                closed(phone);
                // EOF was shutdown, not closure of this still-owned alias.
                descriptor_open(received_fd);
                assert!(matches!(rx.receive().unwrap(), Some(Message::Cancel { generation: 7 })));
                drop_descriptor(received);
            } else if boundary == 3 {
                let mut bytes = [0; 40];
                tx.stream.as_mut().unwrap().read_exact(&mut bytes).unwrap();
                assert_eq!(bytes.to_vec(), ack(7, 1));
                let sender_ipc = tx.stream.as_ref().unwrap().as_raw_fd();
                drop(tx);
                descriptor_closed(pending_fd);
                descriptor_closed(sender_ipc);
                closed(phone);
                descriptor_open(received_fd);
                drop_descriptor(received);
                live(&mut sentinel, &mut sentinel_peer);
                drop_descriptor(sentinel);
                closed(sentinel_peer);
                continue;
            } else {
                assert!(tx.poll_adoption().unwrap());
                descriptor_closed(pending_fd);
                assert!(tx.pending.is_none());
                descriptor_open(received_fd);
                let mut received = UnixStream::from(received);
                if boundary == 2 {
                    tx.send(Message::Cancel { generation: 7 }).unwrap();
                    assert!(matches!(rx.receive().unwrap(), Some(Message::Cancel { generation: 7 })));
                    // After ACK the IPC layer cannot revoke the transferred fd;
                    // helper-worker cancellation is checked in the helper tests.
                    live(&mut received, &mut phone);
                }
                let sender_ipc = tx.stream.as_ref().unwrap().as_raw_fd();
                drop(tx);
                descriptor_closed(sender_ipc);
                descriptor_open(received_fd);
                live(&mut received, &mut phone);
                drop_descriptor(received);
                closed(phone);
                live(&mut sentinel, &mut sentinel_peer);
                drop_descriptor(sentinel);
                closed(sentinel_peer);
                continue;
            }
        }
        drop((tx, rx));
        live(&mut sentinel, &mut sentinel_peer);
        drop_descriptor(sentinel);
        closed(sentinel_peer);
    }
}

#[test]
fn receiver_eof_revokes_transport_and_releases_known_channel_owners() {
    if !isolated_fd_probe("receiver_eof_revokes_transport_and_releases_known_channel_owners") {
        return;
    }
    for consume_arrival in [false, true] {
        let (mut tx, mut rx) = super::super::tests::pair();
        let phone = arrival(&mut tx, 11);
        let pending_fd = tx.pending.as_ref().unwrap().stream.as_raw_fd();
        let received = consume_arrival.then(|| adopted(&mut rx));
        // No explicit Cancel in this test. Discard ACK when present to leave
        // sender ownership pending while receiver EOF is observed.
        if consume_arrival {
            let mut bytes = [0; 40];
            tx.stream.as_mut().unwrap().read_exact(&mut bytes).unwrap();
            assert_eq!(bytes.to_vec(), ack(11, 1));
        }
        let receiver_ipc = rx.stream.as_ref().unwrap().as_raw_fd();
        drop(rx);
        descriptor_closed(receiver_ipc);
        let sender_ipc = tx.stream.as_ref().unwrap().as_raw_fd();
        assert_eq!(tx.poll_adoption(), Err(Error::Disconnected));
        descriptor_closed(pending_fd);
        descriptor_closed(sender_ipc);
        assert!(tx.pending.is_none());
        assert!(tx.rights.is_empty());
        assert!(tx.stream.is_none());
        assert!(tx.poisoned);
        closed(phone);
        if let Some(received) = received {
            // Transport EOF did not close this caller-owned descriptor.
            descriptor_open(received.as_raw_fd());
            drop_descriptor(received);
        }
    }
}
