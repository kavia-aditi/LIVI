// SPDX-License-Identifier: GPL-3.0-or-later
//! Bluetooth-only diagnostic. No TCP, helper IPC, AP configuration or session runner.
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use livi_aa_proto::{WifiVersionRequest, WifiVersionResponse};
use livi_native_bluetooth::{Address, CleanupSnapshot, DiagnosticCategory, DiagnosticEvent, DiagnosticFailure, DiagnosticLifecycle, DiagnosticOperation, Error, Event};
use prost::Message;

pub const OVERALL: Duration = Duration::from_secs(60);
pub const VERSION: Duration = Duration::from_secs(10);
pub const CLEANUP: Duration = Duration::from_secs(2);
pub const MAX_BODY: usize = 4096;
const GENERATION: u64 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    Options,
    Unauthorized,
    Native(DiagnosticFailure),
    Radio,
    Peer,
    Pairing,
    Timeout,
    Protocol,
    Transport,
    Cancelled,
    Cleanup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Starting,
    Paired,
    Connected,
    VersionSent,
    IdentityValid,
    Cleaning,
    Stopped,
}

/// Only paired-only connection is available to the runner, never pairing/confirm/discovery.
pub trait Radio {
    fn start(&mut self) -> Result<(), Error>;
    fn connect_paired(&mut self, generation: u64, peer: Address) -> Result<(), Error>;
    fn tick(&mut self, now: Duration);
    fn next_event(&mut self) -> Option<Event>;
    fn cancel(&mut self, generation: u64);
    fn stop(&mut self);
    fn idle(&self) -> bool;
    fn diagnostic_failure(&self) -> Option<DiagnosticFailure> { None }
    fn next_diagnostic_event(&mut self) -> Option<DiagnosticEvent> { None }
    fn cleanup_snapshot(&self) -> Option<CleanupSnapshot> { None }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleanupReport {
    pub native: Option<CleanupSnapshot>,
    pub runner_stream_owned: bool,
}

impl std::fmt::Display for CleanupReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CleanupSnapshot: runner_stream_owned={} ", self.runner_stream_owned)?;
        match self.native {
            Some(snapshot) => write!(f, "native_snapshot=available {snapshot}"),
            None => f.write_str("native_snapshot=unavailable"),
        }
    }
}

pub struct Options {
    pub peer: Address,
    pub authorized: bool,
}

impl Options {
    /// Deliberately no AP, endpoint, projection, pairing or duration overrides.
    pub fn parse(args: &[String]) -> Result<Self, Failure> {
        let mut authorized = false;
        let mut paired = false;
        let mut peer = None;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--authorize-bluetooth-hardware" if !authorized => authorized = true,
                "--paired-phone-only" if !paired => paired = true,
                "--phone-address" if peer.is_none() => {
                    i += 1;
                    peer = Some(parse_address(args.get(i).ok_or(Failure::Options)?)?);
                }
                _ => return Err(Failure::Options),
            }
            i += 1;
        }
        if !authorized || !paired {
            return Err(Failure::Unauthorized);
        }
        Ok(Self { peer: peer.ok_or(Failure::Options)?, authorized })
    }
}

fn parse_address(text: &str) -> Result<Address, Failure> {
    if text.len() != 17 { return Err(Failure::Options); }
    let mut bytes = [0; 6];
    for (i, part) in text.split(':').enumerate() {
        if i >= 6 || part.len() != 2 || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Failure::Options);
        }
        bytes[i] = u8::from_str_radix(part, 16).map_err(|_| Failure::Options)?;
    }
    let address = Address(bytes);
    if !address.valid() { return Err(Failure::Options); }
    Ok(address)
}

/// Exactly one WPP message 4, version 6.0. No endpoint, AP or invented capability.
pub fn version_request() -> Vec<u8> {
    let body = WifiVersionRequest {
        major_version: Some(6),
        minor_version: Some(0),
        ..Default::default()
    }.encode_to_vec();
    let mut frame = Vec::with_capacity(body.len() + 4);
    frame.extend_from_slice(&(body.len() as u16).to_be_bytes());
    frame.extend_from_slice(&4u16.to_be_bytes());
    frame.extend_from_slice(&body);
    frame
}

fn valid_label(value: Option<&str>) -> bool {
    value.is_some_and(|s| (1..=128).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_graphic()))
}

/// Returns no identity material to logging or persistence.
pub fn validate_response(body: &[u8]) -> Result<(), Failure> {
    if body.len() > MAX_BODY { return Err(Failure::Protocol); }
    let response = WifiVersionResponse::decode(body).map_err(|_| Failure::Protocol)?;
    if response.major_version != Some(6) || response.minor_version != Some(0)
        || response.status != Some(0) || !valid_label(response.device_serial.as_deref())
        || !valid_label(response.mobile_device_identity.as_ref()
            .and_then(|id| id.mobile_device_id.as_deref()))
    {
        return Err(Failure::Protocol);
    }
    Ok(())
}

struct Exchange {
    request: Vec<u8>,
    written: usize,
    header: [u8; 4],
    header_read: usize,
    body: Vec<u8>,
    body_read: usize,
}

impl Exchange {
    fn new() -> Self {
        Self { request: version_request(), written: 0, header: [0; 4],
            header_read: 0, body: Vec::new(), body_read: 0 }
    }

    /// At most one bounded read/write per tick; nonblocking stream required.
    fn poll<S: Read + Write>(&mut self, stream: &mut S) -> Result<Option<Stage>, Failure> {
        if self.written < self.request.len() {
            match stream.write(&self.request[self.written..]) {
                Ok(0) => return Err(Failure::Transport),
                Ok(n) => self.written += n,
                Err(e) if transient(&e) => return Ok(None),
                Err(_) => return Err(Failure::Transport),
            }
            if self.written == self.request.len() { return Ok(Some(Stage::VersionSent)); }
            return Ok(None);
        }
        if self.header_read < 4 {
            match stream.read(&mut self.header[self.header_read..]) {
                Ok(0) => return Err(Failure::Transport),
                Ok(n) => self.header_read += n,
                Err(e) if transient(&e) => return Ok(None),
                Err(_) => return Err(Failure::Transport),
            }
            if self.header_read == 4 {
                let length = u16::from_be_bytes([self.header[0], self.header[1]]) as usize;
                if self.header[2..] != [0, 5] || length == 0 || length > MAX_BODY {
                    return Err(Failure::Protocol);
                }
                self.body.resize(length, 0);
            }
            return Ok(None);
        }
        match stream.read(&mut self.body[self.body_read..]) {
            Ok(0) => return Err(Failure::Transport),
            Ok(n) => self.body_read += n,
            Err(e) if transient(&e) => return Ok(None),
            Err(_) => return Err(Failure::Transport),
        }
        if self.body_read == self.body.len() {
            validate_response(&self.body)?;
            return Ok(Some(Stage::IdentityValid));
        }
        Ok(None)
    }
}

fn transient(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted)
}

/// Owns one attempt. No reuse, reconnect, session handoff or permission to admit TCP.
pub struct Runner<R: Radio> {
    radio: R,
    peer: Address,
    stage: Stage,
    started: Duration,
    version_started: Option<Duration>,
    cleaning: Option<Duration>,
    connected: bool,
    selected: bool,
    stream: Option<UnixStream>,
    exchange: Exchange,
    outcome: Option<Result<(), Failure>>,
    result: Option<Result<(), Failure>>,
    detail: Option<DiagnosticFailure>,
    transcript: Vec<DiagnosticEvent>,
    transcript_overflow: bool,
    cleanup_report: Option<CleanupReport>,
}

impl<R: Radio> Runner<R> {
    pub fn new(mut radio: R, options: Options, now: Duration) -> Result<Self, Failure> {
        if !options.authorized { return Err(Failure::Unauthorized); }
        if !options.peer.valid() { return Err(Failure::Options); }
        if radio.start().is_err() {
            let detail = radio.diagnostic_failure();
            radio.stop();
            return Err(detail.map_or(Failure::Radio, Failure::Native));
        }
        Ok(Self { radio, peer: options.peer, stage: Stage::Starting, started: now,
            version_started: None, cleaning: None, connected: false, selected: false,
            stream: None, exchange: Exchange::new(), outcome: None, result: None, detail: None,
            transcript: Vec::new(), transcript_overflow: false, cleanup_report: None })
    }

    pub fn stage(&self) -> Stage { self.stage }
    pub fn result(&self) -> Option<Result<(), Failure>> { self.result }
    /// Historical selected-peer fd acceptance, not a currently active channel,
    /// WPP validation, authenticated identity or projection readiness.
    pub fn transport_observed(&self) -> bool { self.connected }
    pub fn diagnostic_failure(&self) -> Option<DiagnosticFailure> { self.detail }
    pub fn diagnostic_transcript(&self) -> &[DiagnosticEvent] { &self.transcript }
    pub fn cleanup_report(&self) -> Option<CleanupReport> { self.cleanup_report }

    fn collect_diagnostics(&mut self) {
        for _ in 0..64 {
            let Some(event) = self.radio.next_diagnostic_event() else { break; };
            if self.transcript_overflow { continue; }
            if self.transcript.len() >= 63 {
                self.transcript.push(DiagnosticEvent {
                    kind: DiagnosticLifecycle::Overflow, raw_ior_return: None,
                });
                self.transcript_overflow = true;
            } else {
                self.transcript_overflow = event.kind == DiagnosticLifecycle::Overflow;
                self.transcript.push(event);
            }
        }
    }

    fn finish(&mut self, now: Duration, outcome: Result<(), Failure>) {
        if self.cleaning.is_some() {
            if outcome == Err(Failure::Cancelled) {
                self.outcome = Some(outcome);
            }
            return;
        }
        // Read before stop clears native events. Cancellation remains authoritative.
        if outcome.is_err() {
            self.detail = self.radio.diagnostic_failure().or(self.detail);
        }
        self.stream.take();
        self.radio.cancel(GENERATION);
        self.radio.stop();
        self.cleaning = Some(now);
        self.outcome = Some(outcome);
        self.stage = Stage::Cleaning;
    }

    pub fn step(&mut self, now: Duration, cancelled: bool) {
        if self.result.is_some() { return; }
        self.step_inner(now, cancelled);
        self.collect_diagnostics();
    }

    fn step_inner(&mut self, now: Duration, cancelled: bool) {
        if self.result.is_some() { return; }
        if cancelled { self.finish(now, Err(Failure::Cancelled)); }
        if self.cleaning.is_none() && (now.saturating_sub(self.started) >= OVERALL
            || self.version_started.is_some_and(|start| now.saturating_sub(start) >= VERSION))
        {
            self.detail = Some(DiagnosticFailure {
                category: DiagnosticCategory::Timeout,
                operation: if self.version_started.is_some_and(|start| now.saturating_sub(start) >= VERSION) {
                    DiagnosticOperation::VersionDeadline
                } else { DiagnosticOperation::OverallDeadline },
                raw_ior_return: None,
            });
            self.finish(now, Err(Failure::Timeout));
        }
        if let Some(start) = self.cleaning {
            self.radio.tick(now);
            // Closing a stale Connected event also closes its owned descriptor.
            for _ in 0..64 {
                if self.radio.next_event().is_none() { break; }
            }
            if self.radio.idle() {
                self.result = self.outcome;
                self.stage = Stage::Stopped;
            } else if now.saturating_sub(start) >= CLEANUP {
                // Read local state once, without pumping callbacks or changing idle.
                self.cleanup_report = Some(CleanupReport {
                    native: self.radio.cleanup_snapshot(),
                    runner_stream_owned: self.stream.is_some(),
                });
                self.result = Some(Err(Failure::Cleanup));
                self.stage = Stage::Stopped;
            }
            return;
        }
        self.radio.tick(now);
        for _ in 0..64 {
            let Some(event) = self.radio.next_event() else { break; };
            let failure = match event {
                Event::Availability(true) if !self.selected => {
                    self.selected = true;
                    if let Err(error) = self.radio.connect_paired(GENERATION, self.peer) {
                        self.detail = Some(DiagnosticFailure::connection(error));
                        Some(Failure::Pairing)
                    } else {
                        self.stage = Stage::Paired;
                        None
                    }
                }
                Event::Connected { generation, stream, peer, local } => {
                    if generation != GENERATION { continue; }
                    if !self.selected || self.connected || peer != self.peer || !local.valid() {
                        Some(Failure::Peer)
                    } else {
                        let stream = UnixStream::from(stream);
                        if stream.set_nonblocking(true).is_err() {
                            Some(Failure::Transport)
                        } else {
                            self.stream = Some(stream);
                            self.connected = true;
                            self.version_started = Some(now);
                            self.stage = Stage::Connected;
                            None
                        }
                    }
                }
                Event::Confirmation { generation, .. } | Event::Passkey { generation, .. }
                    if generation == GENERATION => {
                        self.detail = Some(DiagnosticFailure {
                            category: DiagnosticCategory::UnpairedTarget,
                            operation: DiagnosticOperation::PairingRequest,
                            raw_ior_return: None,
                        });
                        Some(Failure::Pairing)
                    }
                Event::Failure { generation, error }
                    if generation == 0 || generation == GENERATION => {
                        self.detail = Some(DiagnosticFailure::connection(error));
                        Some(Failure::Radio)
                    }
                Event::Closed { generation }
                    if generation == 0 || generation == GENERATION => Some(Failure::Radio),
                Event::Availability(false) => {
                    self.detail = Some(DiagnosticFailure {
                        category: DiagnosticCategory::Unavailable,
                        operation: DiagnosticOperation::Availability,
                        raw_ior_return: None,
                    });
                    Some(Failure::Radio)
                }
                _ => None,
            };
            if let Some(error) = failure {
                self.finish(now, Err(error));
                return;
            }
        }
        if let Some(stream) = &mut self.stream {
            match self.exchange.poll(stream) {
                Ok(Some(Stage::IdentityValid)) => self.finish(now, Ok(())),
                Ok(Some(stage)) => self.stage = stage,
                Ok(None) => {}
                Err(error) => self.finish(now, Err(error)),
            }
        }
    }
}

impl<R: Radio> Drop for Runner<R> {
    fn drop(&mut self) {
        self.stream.take();
        self.radio.cancel(GENERATION);
        self.radio.stop();
    }
}

#[cfg(test)]
mod tests;
