// SPDX-License-Identifier: GPL-3.0-or-later

//! Windowless native Bluetooth ownership. This crate never owns an application
//! delegate or event loop. Call `poll` from the existing UI main-thread loop.
//! No helper IPC, WPP, TCP listener, credentials, or projection lives here.

use std::fmt;
use std::marker::PhantomData;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::time::Duration;

pub mod paired;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Address(pub [u8; 6]);

impl Address {
    pub fn valid(self) -> bool {
        self.0 != [0; 6] && self.0 != [255; 6] && self.0[0] & 1 == 0
    }
}

impl fmt::Debug for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[Bluetooth address redacted]")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ComparisonCode(pub u32);

impl fmt::Debug for ComparisonCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[pairing code redacted]")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Disabled,
    WrongThread,
    Unavailable,
    MetadataMissing,
    RegistrationConflict,
    Busy,
    InvalidDevice,
    PairingRejected,
    UnsupportedPairing,
    TimedOut,
    Transport,
    Backpressure,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "native Bluetooth: {self:?}")
    }
}

impl std::error::Error for Error {}

/// Fixed diagnostic vocabulary only: never contains device-supplied text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum DiagnosticCategory {
    OwnerUnavailable = 1,
    Busy = 2,
    DeviceNotFound = 3,
    UnpairedTarget = 4,
    Sdp = 5,
    Rfcomm = 6,
    Timeout = 7,
    Permission = 8,
    Unavailable = 9,
    Transport = 10,
    Registration = 11,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum DiagnosticOperation {
    Start = 1,
    ConnectPaired = 2,
    DeviceLookup = 3,
    BondCheck = 4,
    SdpQuery = 5,
    SdpComplete = 6,
    SdpRecord = 7,
    SdpChannel = 8,
    RfcommOpen = 9,
    RfcommComplete = 10,
    RfcommWrite = 11,
    RfcommWriteComplete = 12,
    PermissionCheck = 13,
    Availability = 14,
    NativeDeadline = 15,
    Registration = 16,
    PairingRequest = 17,
    OverallDeadline = 18,
    VersionDeadline = 19,
    RfcommClosed = 20,
    Bridge = 21,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiagnosticFailure {
    pub category: DiagnosticCategory,
    pub operation: DiagnosticOperation,
    /// Original IOReturn bit pattern, not an application-defined error code.
    pub raw_ior_return: Option<u32>,
}

impl DiagnosticFailure {
    pub fn connection(error: Error) -> Self {
        use DiagnosticCategory as C;
        use DiagnosticOperation as O;
        let (category, operation) = match error {
            Error::Unavailable | Error::Disabled | Error::WrongThread => (C::OwnerUnavailable, O::ConnectPaired),
            Error::Busy => (C::Busy, O::ConnectPaired),
            Error::InvalidDevice => (C::DeviceNotFound, O::DeviceLookup),
            Error::PairingRejected => (C::UnpairedTarget, O::BondCheck),
            Error::TimedOut => (C::Timeout, O::NativeDeadline),
            Error::MetadataMissing => (C::Permission, O::Start),
            Error::RegistrationConflict => (C::Registration, O::Registration),
            _ => (C::Transport, O::ConnectPaired),
        };
        Self { category, operation, raw_ior_return: None }
    }
}

impl fmt::Display for DiagnosticFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "category={:?} operation={:?} ", self.category, self.operation)?;
        match self.raw_ior_return {
            Some(raw) => write!(f, "IOReturn=0x{raw:08X}"),
            None => f.write_str("IOReturn=unavailable"),
        }
    }
}

/// Local ownership at one instant, not an OS-side disconnect/registration audit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleanupSnapshot {
    bits: u32,
}

impl CleanupSnapshot {
    pub fn from_bits(bits: u32) -> Option<Self> {
        (bits & !0x1FFF == 0).then_some(Self { bits })
    }

    pub fn bits(self) -> u32 { self.bits }
}

impl fmt::Display for CleanupSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, name) in [
            "pending_sdp_callback", "pending_open_callback", "rfcomm_channel_owned",
            "bridge_fd_owned", "service_record_owned", "listener_token_owned",
            "privacy_manager_owned", "inquiry_owned", "pairing_owned", "events_queued",
            "enabled", "available", "queued_descriptors_owned",
        ].iter().enumerate() {
            if index != 0 { f.write_str(" ")?; }
            write!(f, "{name}={}", self.bits & (1 << index) != 0)?;
        }
        Ok(())
    }
}

/// Fixed, identifier-free observations; these are not connection/admission events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum DiagnosticLifecycle {
    QueryInitiated = 1,
    QueryAccepted = 2,
    QueryRejected = 3,
    CallbackEntered = 4,
    CallbackAccepted = 5,
    CallbackRejected = 6,
    CallbackLate = 7,
    ServiceRecordFound = 8,
    ServiceRecordMissing = 9,
    RfcommChannelFound = 10,
    RfcommChannelMissing = 11,
    RfcommChannelNotChecked = 12,
    IncomingObserved = 13,
    IncomingNotObservedAtStop = 14,
    QueryDeadline = 15,
    OpenDeadline = 16,
    PairDeadline = 17,
    Overflow = 18,
    OwnedTransport = 19,
    LocalServiceReady = 20,
    PairedListenerArmed = 21,
    IncomingChannelOpen = 22,
    IncomingChannelNotOpen = 23,
    DelegateAttached = 24,
    DelegateRejected = 25,
    IncomingRejected = 26,
    OpenCallbackEntered = 27,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiagnosticEvent {
    pub kind: DiagnosticLifecycle,
    pub raw_ior_return: Option<u32>,
}

impl DiagnosticEvent {
    pub fn decode(raw: [u32; 3]) -> Option<Self> {
        use DiagnosticLifecycle::*;
        let kind = match raw[0] {
            1 => QueryInitiated, 2 => QueryAccepted, 3 => QueryRejected,
            4 => CallbackEntered, 5 => CallbackAccepted, 6 => CallbackRejected,
            7 => CallbackLate, 8 => ServiceRecordFound, 9 => ServiceRecordMissing,
            10 => RfcommChannelFound, 11 => RfcommChannelMissing,
            12 => RfcommChannelNotChecked, 13 => IncomingObserved,
            14 => IncomingNotObservedAtStop, 15 => QueryDeadline,
            16 => OpenDeadline, 17 => PairDeadline, 18 => Overflow,
            19 => OwnedTransport,
            20 => LocalServiceReady, 21 => PairedListenerArmed,
            22 => IncomingChannelOpen, 23 => IncomingChannelNotOpen,
            24 => DelegateAttached, 25 => DelegateRejected,
            26 => IncomingRejected, 27 => OpenCallbackEntered,
            _ => return None,
        };
        if raw[2] > 1 { return None; }
        Some(Self { kind, raw_ior_return: (raw[2] == 1).then_some(raw[1]) })
    }
}

impl fmt::Display for DiagnosticEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Lifecycle: {:?} ", self.kind)?;
        match self.raw_ior_return {
            Some(raw) => write!(f, "IOReturn=0x{raw:08X}"),
            None => f.write_str("IOReturn=unavailable"),
        }
    }
}

#[derive(Debug)]
pub enum Event {
    Availability(bool),
    Discovered(Address),
    Confirmation {
        generation: u64,
        code: ComparisonCode,
    },
    Passkey {
        generation: u64,
        code: ComparisonCode,
    },
    Connected {
        generation: u64,
        stream: OwnedFd,
        peer: Address,
        local: Address,
    },
    Failure {
        generation: u64,
        error: Error,
    },
    Closed {
        generation: u64,
    },
}

impl Event {
    fn generation(&self) -> Option<u64> {
        match self {
            Self::Availability(_) | Self::Discovered(_) => None,
            Self::Confirmation { generation, .. }
            | Self::Passkey { generation, .. }
            | Self::Connected { generation, .. }
            | Self::Failure { generation, .. }
            | Self::Closed { generation } => Some(*generation),
        }
    }
}

/// All three gates are needed. An enabled config alone never touches hardware.
/// Listener readiness and explicit hardware consent are supplied by future
/// integration stages, not inferred from environment variables or saved state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Gates {
    pub enabled: bool,
    pub listener_ready: bool,
    pub hardware_authorized: bool,
}

impl Gates {
    pub fn armed(self) -> bool {
        self.enabled && self.listener_ready && self.hardware_authorized
    }
}

/// Backend methods are serialized on the owner's thread. `stop` must withdraw
/// registration before closing owned streams, and be safe after partial start.
pub trait Backend {
    fn start(&mut self) -> Result<(), Error>;
    fn stop(&mut self);
    fn tick(&mut self, now: Duration);
    fn next_event(&mut self) -> Option<Event>;
    fn discover(&mut self) -> Result<(), Error>;
    fn connect(&mut self, generation: u64, peer: Address) -> Result<(), Error>;
    fn confirm(&mut self, generation: u64, accept: bool) -> Result<(), Error>;
    fn cancel(&mut self, generation: u64);
}

pub struct Adapter<B: Backend> {
    backend: B,
    active: bool,
    gates: Gates,
    next_generation: u64,
    attempt: Option<(u64, Duration)>,
    confirmation: Option<u64>,
    // Native ownership must never move to a worker, even with a Send backend.
    _main_thread: PhantomData<Rc<()>>,
}

impl<B: Backend> Adapter<B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            active: false,
            gates: Gates::default(),
            next_generation: 0,
            attempt: None,
            confirmation: None,
            _main_thread: PhantomData,
        }
    }

    pub fn configure(&mut self, gates: Gates) -> Result<(), Error> {
        let was_armed = self.gates.armed();
        self.gates = gates;
        if !gates.armed() {
            self.shutdown();
        } else if !was_armed {
            if let Err(error) = self.backend.start() {
                self.backend.stop();
                return Err(error);
            }
            self.active = true;
        }
        Ok(())
    }

    pub fn discover(&mut self) -> Result<(), Error> {
        self.require_active()?;
        self.backend.discover()
    }

    pub fn connect(&mut self, peer: Address, now: Duration) -> Result<u64, Error> {
        self.require_active()?;
        if !peer.valid() {
            return Err(Error::InvalidDevice);
        }
        if self.attempt.is_some() {
            return Err(Error::Busy);
        }
        let generation = self.next_generation.checked_add(1).ok_or(Error::Busy)?;
        self.next_generation = generation;
        self.backend.connect(generation, peer)?;
        self.attempt = Some((generation, now.saturating_add(Duration::from_secs(90))));
        Ok(generation)
    }

    /// Only an explicit response to the current comparison request can accept.
    /// Reject stale UI responses; no automatic acceptance or guessed PINs.
    pub fn confirm(&mut self, generation: u64, accept: bool) -> Result<(), Error> {
        self.require_active()?;
        if self.confirmation != Some(generation) || self.attempt.map(|a| a.0) != Some(generation) {
            return Err(Error::PairingRejected);
        }
        self.confirmation = None;
        self.backend.confirm(generation, accept)
    }

    pub fn cancel(&mut self, generation: u64) {
        if self.attempt.map(|a| a.0) == Some(generation) {
            self.attempt = None;
            self.confirmation = None;
            self.backend.cancel(generation);
        }
    }

    pub fn poll(&mut self, now: Duration) -> Vec<Event> {
        if !self.active {
            return Vec::new();
        }
        self.backend.tick(now);
        let mut out = Vec::new();
        if let Some((generation, deadline)) = self.attempt
            && deadline != Duration::MAX
            && now >= deadline
        {
            self.cancel(generation);
            out.push(Event::Failure {
                generation,
                error: Error::TimedOut,
            });
        }
        // Both the native and Rust boundary are bounded. Dropped events own fds.
        for _ in 0..64 {
            let Some(event) = self.backend.next_event() else {
                break;
            };
            if let Some(generation) = event.generation()
                && ((generation == 0 && !matches!(&event, Event::Failure { .. }))
                    || (generation != 0 && self.attempt.map(|a| a.0) != Some(generation)))
            {
                continue;
            }
            match &event {
                Event::Confirmation { generation, .. } => self.confirmation = Some(*generation),
                Event::Connected {
                    generation,
                    peer,
                    local,
                    ..
                } => {
                    if !peer.valid() || !local.valid() {
                        self.cancel(*generation);
                        continue;
                    }
                    // Transport success is not WPP/session/video success.
                    self.attempt = Some((*generation, Duration::MAX));
                }
                Event::Failure { generation, .. } | Event::Closed { generation } => {
                    if *generation == 0 {
                        if let Some((current, _)) = self.attempt {
                            self.cancel(current);
                        }
                    } else if self.attempt.map(|a| a.0) == Some(*generation) {
                        self.attempt = None;
                        self.confirmation = None;
                    }
                }
                Event::Availability(false) => {
                    if let Some((generation, _)) = self.attempt {
                        self.cancel(generation);
                    }
                }
                _ => {}
            }
            out.push(event);
        }
        out
    }

    pub fn shutdown(&mut self) {
        self.attempt = None;
        self.confirmation = None;
        if std::mem::replace(&mut self.active, false) {
            self.backend.stop();
        }
    }

    fn require_active(&self) -> Result<(), Error> {
        if self.active {
            Ok(())
        } else {
            Err(Error::Disabled)
        }
    }
}

impl<B: Backend> Drop for Adapter<B> {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    use std::ffi::{c_int, c_void};
    use std::os::fd::FromRawFd;

    #[repr(C)]
    struct RawEvent {
        kind: u32,
        value: u32,
        generation: u64,
        peer: [u8; 6],
        local: [u8; 6],
        fd: c_int,
    }

    unsafe extern "C" {
        fn livi_bt_create() -> *mut c_void;
        fn livi_bt_destroy(owner: *mut c_void);
        fn livi_bt_is_main_thread() -> bool;
        fn livi_bt_start(owner: *mut c_void) -> u32;
        fn livi_bt_stop(owner: *mut c_void);
        fn livi_bt_tick(owner: *mut c_void, now: f64);
        fn livi_bt_event(owner: *mut c_void, event: *mut RawEvent) -> bool;
        fn livi_bt_discover(owner: *mut c_void) -> u32;
        fn livi_bt_connect(owner: *mut c_void, generation: u64, peer: *const u8) -> u32;
        fn livi_bt_confirm(owner: *mut c_void, generation: u64, accept: bool) -> u32;
        fn livi_bt_cancel(owner: *mut c_void, generation: u64);
        #[cfg(feature = "paired-diagnostic")]
        fn livi_bt_connect_paired(owner: *mut c_void, generation: u64, peer: *const u8) -> u32;
        #[cfg(feature = "paired-diagnostic")]
        fn livi_bt_listen_paired(owner: *mut c_void, generation: u64, peer: *const u8) -> u32;
        #[cfg(feature = "paired-diagnostic")]
        fn livi_bt_diagnostic_idle(owner: *mut c_void) -> bool;
        #[cfg(feature = "paired-diagnostic")]
        fn livi_bt_diagnostic_cleanup_snapshot(owner: *mut c_void, out: *mut u32) -> bool;
        #[cfg(feature = "paired-diagnostic")]
        fn livi_bt_diagnostic_reporting(owner: *mut c_void);
        #[cfg(feature = "paired-diagnostic")]
        fn livi_bt_diagnostic_failure(owner: *mut c_void, out: *mut u32) -> bool;
        #[cfg(feature = "paired-diagnostic")]
        fn livi_bt_diagnostic_event(owner: *mut c_void, out: *mut u32) -> bool;
        #[cfg(test)]
        fn livi_bt_offline_test(scenario: u32) -> u32;
    }

    fn error(code: u32) -> Error {
        match code {
            1 => Error::Unavailable,
            2 => Error::MetadataMissing,
            3 => Error::RegistrationConflict,
            4 => Error::Busy,
            5 => Error::InvalidDevice,
            6 => Error::PairingRejected,
            7 => Error::UnsupportedPairing,
            8 => Error::TimedOut,
            10 => Error::Backpressure,
            _ => Error::Transport,
        }
    }

    fn result(code: u32) -> Result<(), Error> {
        if code == 0 { Ok(()) } else { Err(error(code)) }
    }

    /// Construction only allocates a dormant owner, never a Bluetooth manager.
    /// All calls and Drop remain on the process main thread.
    pub struct MacBackend {
        owner: *mut c_void,
        _main_thread: PhantomData<Rc<()>>,
    }

    impl MacBackend {
        pub fn new() -> Result<Self, Error> {
            // SAFETY: this predicate has no side effects or preconditions.
            if !unsafe { livi_bt_is_main_thread() } {
                return Err(Error::WrongThread);
            }
            // SAFETY: called on the process main thread.
            let owner = unsafe { livi_bt_create() };
            if owner.is_null() {
                return Err(Error::Unavailable);
            }
            Ok(Self {
                owner,
                _main_thread: PhantomData,
            })
        }

        /// No pairing fallback. The native boundary rechecks the existing bond.
        #[cfg(feature = "paired-diagnostic")]
        pub fn connect_paired(&mut self, generation: u64, peer: Address) -> Result<(), Error> {
            if generation == 0 || !peer.valid() {
                return Err(Error::InvalidDevice);
            }
            result(unsafe { livi_bt_connect_paired(self.owner, generation, peer.0.as_ptr()) })
        }

        /// Select an existing bond for incoming AA RFCOMM only. No SDP query,
        /// outgoing open, discovery, pairing or automatic retry is performed.
        /// The caller must enforce a bounded window and pump the main run loop.
        #[cfg(feature = "paired-diagnostic")]
        pub fn listen_paired(&mut self, generation: u64, peer: Address) -> Result<(), Error> {
            if generation == 0 || !peer.valid() {
                return Err(Error::InvalidDevice);
            }
            result(unsafe { livi_bt_listen_paired(self.owner, generation, peer.0.as_ptr()) })
        }

        /// Includes pending SDP/open callbacks, not just descriptor closure.
        #[cfg(feature = "paired-diagnostic")]
        pub fn diagnostic_idle(&self) -> bool {
            unsafe { livi_bt_diagnostic_idle(self.owner) }
        }

        #[cfg(feature = "paired-diagnostic")]
        pub fn cleanup_snapshot(&self) -> Option<CleanupSnapshot> {
            let mut bits = 0;
            if !unsafe { livi_bt_diagnostic_cleanup_snapshot(self.owner, &mut bits) } {
                return None;
            }
            CleanupSnapshot::from_bits(bits)
        }

        /// Standalone diagnostic opt-in only; ordinary start/connection is unchanged.
        #[cfg(feature = "paired-diagnostic")]
        pub fn diagnostic_start(&mut self) -> Result<(), Error> {
            unsafe { livi_bt_diagnostic_reporting(self.owner) };
            self.start()
        }

        #[cfg(feature = "paired-diagnostic")]
        pub fn diagnostic_failure(&self) -> Option<DiagnosticFailure> {
            let mut raw = [0u32; 4];
            if !unsafe { livi_bt_diagnostic_failure(self.owner, raw.as_mut_ptr()) } {
                return None;
            }
            Self::decode_diagnostic(raw)
        }

        #[cfg(feature = "paired-diagnostic")]
        pub fn next_diagnostic_event(&mut self) -> Option<DiagnosticEvent> {
            let mut raw = [0u32; 3];
            if !unsafe { livi_bt_diagnostic_event(self.owner, raw.as_mut_ptr()) } {
                return None;
            }
            DiagnosticEvent::decode(raw)
        }

        #[cfg(feature = "paired-diagnostic")]
        fn decode_diagnostic(raw: [u32; 4]) -> Option<DiagnosticFailure> {
            use DiagnosticCategory as C;
            use DiagnosticOperation as O;
            let category = match raw[0] {
                1 => C::OwnerUnavailable, 2 => C::Busy, 3 => C::DeviceNotFound,
                4 => C::UnpairedTarget, 5 => C::Sdp, 6 => C::Rfcomm,
                7 => C::Timeout, 8 => C::Permission, 9 => C::Unavailable,
                10 => C::Transport, 11 => C::Registration, _ => return None,
            };
            let operation = match raw[1] {
                1 => O::Start, 2 => O::ConnectPaired, 3 => O::DeviceLookup,
                4 => O::BondCheck, 5 => O::SdpQuery, 6 => O::SdpComplete,
                7 => O::SdpRecord, 8 => O::SdpChannel, 9 => O::RfcommOpen,
                10 => O::RfcommComplete, 11 => O::RfcommWrite,
                12 => O::RfcommWriteComplete, 13 => O::PermissionCheck,
                14 => O::Availability, 15 => O::NativeDeadline,
                16 => O::Registration, 17 => O::PairingRequest,
                20 => O::RfcommClosed, 21 => O::Bridge, _ => return None,
            };
            Some(DiagnosticFailure { category, operation,
                raw_ior_return: (raw[3] != 0).then_some(raw[2]) })
        }
    }

    impl Backend for MacBackend {
        fn start(&mut self) -> Result<(), Error> {
            // SAFETY: owner is live and this !Send type is main-thread owned.
            result(unsafe { livi_bt_start(self.owner) })
        }
        fn stop(&mut self) {
            unsafe { livi_bt_stop(self.owner) };
        }
        fn tick(&mut self, now: Duration) {
            unsafe { livi_bt_tick(self.owner, now.as_secs_f64()) };
        }
        fn next_event(&mut self) -> Option<Event> {
            let mut event = RawEvent {
                kind: 0,
                value: 0,
                generation: 0,
                peer: [0; 6],
                local: [0; 6],
                fd: -1,
            };
            if !unsafe { livi_bt_event(self.owner, &mut event) } {
                return None;
            }
            // Ownership transfers once, even for unknown/stale/invalid events.
            let stream = if event.fd >= 0 {
                Some(unsafe { OwnedFd::from_raw_fd(event.fd) })
            } else {
                None
            };
            let generation = event.generation;
            Some(match event.kind {
                1 => Event::Availability(event.value != 0),
                2 => Event::Discovered(Address(event.peer)),
                3 => Event::Confirmation {
                    generation,
                    code: ComparisonCode(event.value),
                },
                4 => Event::Passkey {
                    generation,
                    code: ComparisonCode(event.value),
                },
                5 => Event::Connected {
                    generation,
                    stream: stream?,
                    peer: Address(event.peer),
                    local: Address(event.local),
                },
                6 => Event::Failure {
                    generation,
                    error: error(event.value),
                },
                7 => Event::Closed { generation },
                _ => return None,
            })
        }
        fn discover(&mut self) -> Result<(), Error> {
            result(unsafe { livi_bt_discover(self.owner) })
        }
        fn connect(&mut self, generation: u64, peer: Address) -> Result<(), Error> {
            result(unsafe { livi_bt_connect(self.owner, generation, peer.0.as_ptr()) })
        }
        fn confirm(&mut self, generation: u64, accept: bool) -> Result<(), Error> {
            result(unsafe { livi_bt_confirm(self.owner, generation, accept) })
        }
        fn cancel(&mut self, generation: u64) {
            unsafe { livi_bt_cancel(self.owner, generation) };
        }
    }

    impl Drop for MacBackend {
        fn drop(&mut self) {
            unsafe { livi_bt_destroy(self.owner) };
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn construction_on_a_worker_is_rejected_without_hardware_access() {
            assert!(
                std::thread::spawn(|| matches!(MacBackend::new(), Err(Error::WrongThread)))
                    .join()
                    .unwrap()
            );
        }

        macro_rules! native_test {
            ($name:ident, $scenario:expr) => {
                #[test]
                fn $name() {
                    // Only fake NSObject channels/pairs/registrations and socketpair.
                    assert_eq!(unsafe { livi_bt_offline_test($scenario) }, 0);
                }
            };
        }
        #[test]
        #[cfg(feature = "paired-diagnostic")]
        fn diagnostic_wire_decoder_preserves_every_category_operation_and_raw_bits() {
            use DiagnosticCategory as C;
            use DiagnosticOperation as O;
            let categories = [C::OwnerUnavailable, C::Busy, C::DeviceNotFound,
                C::UnpairedTarget, C::Sdp, C::Rfcomm, C::Timeout, C::Permission,
                C::Unavailable, C::Transport, C::Registration];
            let operations = [O::Start, O::ConnectPaired, O::DeviceLookup, O::BondCheck,
                O::SdpQuery, O::SdpComplete, O::SdpRecord, O::SdpChannel, O::RfcommOpen,
                O::RfcommComplete, O::RfcommWrite, O::RfcommWriteComplete,
                O::PermissionCheck, O::Availability, O::NativeDeadline, O::Registration,
                O::PairingRequest, O::RfcommClosed, O::Bridge];
            for category in categories {
                for operation in operations {
                    assert_eq!(MacBackend::decode_diagnostic([
                        category as u32, operation as u32, 0xE00002CD, 1,
                    ]), Some(DiagnosticFailure {
                        category, operation, raw_ior_return: Some(0xE00002CD),
                    }));
                    assert_eq!(MacBackend::decode_diagnostic([
                        category as u32, operation as u32, 0xFFFFFFFF, 0,
                    ]).unwrap().raw_ior_return, None);
                }
            }
            assert_eq!(MacBackend::decode_diagnostic([0, 1, 0, 0]), None);
            assert_eq!(MacBackend::decode_diagnostic([1, 99, 0, 0]), None);
        }
        native_test!(native_partial_registration_cleanup, 0);
        native_test!(native_registration_conflict, 1);
        native_test!(native_pairing_requires_explicit_matching_consent, 2);
        native_test!(native_late_open_is_rejected_and_fd_is_closed, 3);
        native_test!(native_repeated_teardown_and_stale_close, 4);
        native_test!(native_timeout_and_late_pairing_callback, 5);
        native_test!(native_backpressure_closes_only_owned_stream, 6);
        native_test!(native_late_sdp_keeps_attempts_serialized, 7);
        native_test!(native_immediate_open_callback_cleans_up, 8);
        native_test!(native_cancelled_open_blocks_new_attempt_until_callback, 9);
        native_test!(native_destroyed_open_owner_survives_until_callback, 10);
        native_test!(native_diagnostic_unpaired_never_starts_pairing, 11);
        native_test!(native_diagnostic_paired_uses_existing_bond, 12);
        native_test!(native_diagnostic_bond_loss_blocks_adoption, 13);
        native_test!(native_diagnostic_bond_loss_blocks_sdp_open, 14);
        native_test!(native_diagnostic_cleanup_waits_for_pending_callbacks, 15);
        native_test!(native_diagnostic_pairing_confirmation_is_never_accepted, 16);
        native_test!(diagnostic_owner_unavailable, 17);
        native_test!(diagnostic_busy, 18);
        native_test!(diagnostic_device_not_found, 19);
        native_test!(diagnostic_unpaired_target, 20);
        native_test!(diagnostic_sdp_query_raw_error, 21);
        native_test!(diagnostic_sdp_completion_raw_error, 22);
        native_test!(diagnostic_sdp_missing_record_preserves_wait, 23);
        native_test!(diagnostic_sdp_missing_channel_preserves_wait, 24);
        native_test!(diagnostic_sdp_channel_raw_error, 25);
        native_test!(diagnostic_rfcomm_open_raw_error, 26);
        native_test!(diagnostic_rfcomm_completion_raw_error, 27);
        native_test!(diagnostic_native_timeout, 28);
        native_test!(diagnostic_permission_denied, 29);
        native_test!(diagnostic_unsupported_unavailability, 30);
        native_test!(diagnostic_availability_loss, 31);
        native_test!(diagnostic_reporting_disabled_for_ordinary_use, 32);
        native_test!(diagnostic_first_failure_survives_stop, 33);
        native_test!(diagnostic_rfcomm_write_completion_raw_error, 34);
        native_test!(diagnostic_rfcomm_write_raw_error, 35);
        native_test!(diagnostic_rfcomm_closed, 36);
        native_test!(diagnostic_initial_unavailability_snapshot, 37);
        native_test!(diagnostic_terminal_error_replaces_provisional_sdp, 38);
        native_test!(diagnostic_bridge_failure, 39);
        native_test!(diagnostic_bond_check_race_never_enters_pairing, 40);
        native_test!(lifecycle_query_accepted, 41);
        native_test!(lifecycle_query_rejected, 42);
        native_test!(lifecycle_wrong_device_callback, 43);
        native_test!(lifecycle_no_pending_callback, 44);
        native_test!(lifecycle_existing_channel_callback, 45);
        native_test!(lifecycle_cancelled_callback, 46);
        native_test!(lifecycle_stopped_callback, 47);
        native_test!(lifecycle_closing_callback, 48);
        native_test!(lifecycle_disabled_callback, 49);
        native_test!(lifecycle_unavailable_callback, 50);
        native_test!(lifecycle_bond_loss_callback, 51);
        native_test!(lifecycle_error_callback_no_record, 52);
        native_test!(lifecycle_missing_record, 53);
        native_test!(lifecycle_missing_channel, 54);
        native_test!(lifecycle_channel_raw_error, 55);
        native_test!(lifecycle_record_and_channel_found, 56);
        native_test!(lifecycle_error_callback_cached_record, 57);
        native_test!(lifecycle_query_timeout_without_callback, 58);
        native_test!(lifecycle_query_timeout_then_callback, 59);
        native_test!(lifecycle_query_timeout_then_cleanup_callback, 60);
        native_test!(lifecycle_open_timeout, 61);
        native_test!(lifecycle_pair_timeout, 62);
        native_test!(lifecycle_incoming_rejected_observed, 63);
        native_test!(lifecycle_incoming_adopt_path_observed, 64);
        native_test!(lifecycle_no_incoming_snapshot_once, 65);
        native_test!(lifecycle_sticky_bounded_overflow, 66);
        native_test!(lifecycle_ordinary_reporting_disabled, 67);
        native_test!(lifecycle_duplicate_callback_rejected, 68);
        native_test!(lifecycle_destroyed_pending_query_callback, 69);
        native_test!(lifecycle_reentrant_completion_before_query_success, 70);
        native_test!(lifecycle_reentrant_completion_before_query_error, 71);
        native_test!(lifecycle_incoming_after_stop_snapshot, 72);
        native_test!(sdp_callback_immediate_failure_releases_destroyed_owner, 89);
        native_test!(sdp_callback_last_strong_owner_survives_until_matching_completion, 90);
        native_test!(sdp_callback_deferred_default_run_loop_delivery, 91);
        native_test!(sdp_callback_deferred_after_timeout_and_final_external_release, 92);
        native_test!(sdp_callback_deferred_after_cancel_and_stop_is_inert, 93);
        native_test!(incoming_owned_bridge_survives_pending_sdp_deadline, 94);
        native_test!(incoming_owned_bridge_survives_late_sdp_error, 95);
        native_test!(incoming_cancel_closes_fd_and_late_query_cannot_reopen, 96);
        native_test!(incoming_closed_channel_cannot_emit_connected, 97);
        native_test!(incoming_zero_mtu_cannot_emit_connected, 98);
        native_test!(incoming_real_bridge_pumps_both_directions_offline, 99);
        native_test!(incoming_channel_close_ends_real_bridge_offline, 100);
        native_test!(incoming_channel_without_bridge_still_times_out, 101);
        native_test!(passive_listener_survives_59_seconds_without_query_or_open, 102);
        native_test!(passive_listener_unpaired_target_refused, 103);
        native_test!(passive_listener_bond_race_refused, 104);
        native_test!(passive_listener_incoming_owns_and_closes_real_bridge, 105);
        native_test!(passive_listener_delegate_failure_preserves_raw_error, 106);
        native_test!(passive_listener_wrong_peer_rejected, 107);
        native_test!(passive_listener_outgoing_channel_rejected, 108);
        native_test!(passive_listener_wrong_channel_rejected, 109);
        native_test!(passive_listener_closed_channel_rejected, 110);
        native_test!(passive_listener_uses_reassigned_service_channel, 111);
        native_test!(passive_listener_cancel_rejects_incoming, 112);
        native_test!(passive_listener_bond_loss_rejects_incoming, 113);
        native_test!(passive_listener_wrong_notification_rejected, 114);
        native_test!(passive_listener_availability_loss_rejects_incoming, 115);
        native_test!(passive_listener_duplicate_open_completion_preserves_owned_stream, 116);
        native_test!(passive_listener_failed_open_completion_closes_owned_stream, 117);
        native_test!(service_dictionary_public_sdk_encoding_is_exact_without_publication, 118);
        #[cfg(feature = "offline-native-tests")]
        native_test!(discovery_old_inquiry_cannot_publish_into_replacement, 119);
        #[cfg(feature = "offline-native-tests")]
        native_test!(discovery_old_completion_cannot_clear_current_inquiry, 120);
        #[cfg(feature = "offline-native-tests")]
        native_test!(discovery_callbacks_after_timeout_or_withdraw_are_inert, 121);
        #[test]
        fn cleanup_snapshot_reads_each_resource_without_mutation_or_hardware() {
            for scenario in 73..=88 {
                assert_eq!(unsafe { livi_bt_offline_test(scenario) }, 0, "scenario {scenario}");
            }
        }
    }
}

#[cfg(target_os = "macos")]
pub use mac::MacBackend;

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::io::Read;
    use std::os::unix::net::UnixStream;

    #[derive(Default)]
    struct Fake {
        calls: Vec<&'static str>,
        events: VecDeque<Event>,
        start_error: Option<Error>,
        confirmations: Vec<(u64, bool)>,
    }
    struct FakeBackend(Rc<RefCell<Fake>>);
    impl Backend for FakeBackend {
        fn start(&mut self) -> Result<(), Error> {
            let mut fake = self.0.borrow_mut();
            fake.calls.push("start");
            fake.start_error.map_or(Ok(()), Err)
        }
        fn stop(&mut self) {
            self.0.borrow_mut().calls.push("stop");
        }
        fn tick(&mut self, _: Duration) {
            self.0.borrow_mut().calls.push("tick");
        }
        fn next_event(&mut self) -> Option<Event> {
            self.0.borrow_mut().events.pop_front()
        }
        fn discover(&mut self) -> Result<(), Error> {
            self.0.borrow_mut().calls.push("discover");
            Ok(())
        }
        fn connect(&mut self, _: u64, _: Address) -> Result<(), Error> {
            self.0.borrow_mut().calls.push("connect");
            Ok(())
        }
        fn confirm(&mut self, generation: u64, accept: bool) -> Result<(), Error> {
            self.0.borrow_mut().confirmations.push((generation, accept));
            Ok(())
        }
        fn cancel(&mut self, _: u64) {
            self.0.borrow_mut().calls.push("cancel");
        }
    }
    fn fixture() -> (Adapter<FakeBackend>, Rc<RefCell<Fake>>) {
        let fake = Rc::new(RefCell::new(Fake::default()));
        (Adapter::new(FakeBackend(fake.clone())), fake)
    }
    fn armed() -> Gates {
        Gates {
            enabled: true,
            listener_ready: true,
            hardware_authorized: true,
        }
    }
    const PEER: Address = Address([2, 1, 2, 3, 4, 5]);

    #[test]
    fn disabled_default_and_every_incomplete_gate_combination_are_inert() {
        for bits in 0..7 {
            let (mut adapter, fake) = fixture();
            adapter
                .configure(Gates {
                    enabled: bits & 1 != 0,
                    listener_ready: bits & 2 != 0,
                    hardware_authorized: bits & 4 != 0,
                })
                .unwrap();
            assert_eq!(adapter.connect(PEER, Duration::ZERO), Err(Error::Disabled));
            assert_eq!(adapter.discover(), Err(Error::Disabled));
            assert!(adapter.poll(Duration::ZERO).is_empty());
            drop(adapter);
            assert!(fake.borrow().calls.is_empty());
        }
    }

    #[test]
    fn start_failure_cleans_partial_initialization_without_automatic_retries() {
        let (mut adapter, fake) = fixture();
        fake.borrow_mut().start_error = Some(Error::RegistrationConflict);
        assert_eq!(adapter.configure(armed()), Err(Error::RegistrationConflict));
        adapter.configure(armed()).unwrap();
        assert!(adapter.poll(Duration::ZERO).is_empty());
        assert_eq!(fake.borrow().calls, ["start", "stop"]);
    }

    #[test]
    fn pairing_acceptance_requires_current_explicit_confirmation() {
        let (mut adapter, fake) = fixture();
        adapter.configure(armed()).unwrap();
        let generation = adapter.connect(PEER, Duration::ZERO).unwrap();
        assert_eq!(
            adapter.confirm(generation, true),
            Err(Error::PairingRejected)
        );
        fake.borrow_mut().events.push_back(Event::Confirmation {
            generation,
            code: ComparisonCode(123456),
        });
        adapter.poll(Duration::ZERO);
        assert_eq!(
            adapter.confirm(generation + 1, true),
            Err(Error::PairingRejected)
        );
        adapter.confirm(generation, false).unwrap();
        assert_eq!(fake.borrow().confirmations, [(generation, false)]);
        assert_eq!(
            adapter.confirm(generation, true),
            Err(Error::PairingRejected)
        );
    }

    #[test]
    fn stale_arrival_closes_its_descriptor_after_cancellation() {
        let (mut adapter, fake) = fixture();
        adapter.configure(armed()).unwrap();
        let generation = adapter.connect(PEER, Duration::ZERO).unwrap();
        adapter.cancel(generation);
        let (ours, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        fake.borrow_mut().events.push_back(Event::Connected {
            generation,
            stream: ours.into(),
            peer: PEER,
            local: PEER,
        });
        assert!(adapter.poll(Duration::ZERO).is_empty());
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
        let next = adapter.connect(PEER, Duration::ZERO).unwrap();
        assert!(next > generation);
    }

    #[test]
    fn timeout_cancels_and_rejects_late_consent() {
        let (mut adapter, fake) = fixture();
        adapter.configure(armed()).unwrap();
        let generation = adapter.connect(PEER, Duration::ZERO).unwrap();
        let events = adapter.poll(Duration::from_secs(90));
        assert!(matches!(
            events.as_slice(),
            [Event::Failure {
                error: Error::TimedOut,
                ..
            }]
        ));
        fake.borrow_mut().events.push_back(Event::Confirmation {
            generation,
            code: ComparisonCode(123456),
        });
        assert!(adapter.poll(Duration::from_secs(91)).is_empty());
        assert_eq!(
            adapter.confirm(generation, true),
            Err(Error::PairingRejected)
        );
    }

    #[test]
    fn repeated_shutdown_withdraws_once_and_drop_is_safe() {
        let (mut adapter, fake) = fixture();
        adapter.configure(armed()).unwrap();
        adapter.shutdown();
        adapter.shutdown();
        drop(adapter);
        assert_eq!(fake.borrow().calls, ["start", "stop"]);
    }

    #[test]
    fn availability_loss_cancels_only_the_current_attempt() {
        let (mut adapter, fake) = fixture();
        adapter.configure(armed()).unwrap();
        adapter.connect(PEER, Duration::ZERO).unwrap();
        fake.borrow_mut()
            .events
            .push_back(Event::Availability(false));
        assert!(matches!(
            adapter.poll(Duration::ZERO).as_slice(),
            [Event::Availability(false)]
        ));
        assert_eq!(fake.borrow().calls, ["start", "connect", "tick", "cancel"]);
    }

    #[test]
    fn generation_zero_attempt_events_are_dropped_and_close_descriptors() {
        let (mut adapter, fake) = fixture();
        adapter.configure(armed()).unwrap();
        let generation = adapter.connect(PEER, Duration::ZERO).unwrap();
        let (ours, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        fake.borrow_mut().events.extend([
            Event::Confirmation {
                generation: 0,
                code: ComparisonCode(123456),
            },
            Event::Passkey {
                generation: 0,
                code: ComparisonCode(123456),
            },
            Event::Connected {
                generation: 0,
                stream: ours.into(),
                peer: PEER,
                local: PEER,
            },
            Event::Closed { generation: 0 },
        ]);
        assert!(adapter.poll(Duration::ZERO).is_empty());
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
        assert_eq!(
            adapter.confirm(generation, true),
            Err(Error::PairingRejected)
        );
        assert_eq!(adapter.connect(PEER, Duration::ZERO), Err(Error::Busy));
    }

    #[test]
    fn global_failure_cancels_the_backend_attempt_before_clearing_state() {
        let (mut adapter, fake) = fixture();
        adapter.configure(armed()).unwrap();
        adapter.connect(PEER, Duration::ZERO).unwrap();
        fake.borrow_mut().events.push_back(Event::Failure {
            generation: 0,
            error: Error::Backpressure,
        });
        assert!(matches!(
            adapter.poll(Duration::ZERO).as_slice(),
            [Event::Failure {
                generation: 0,
                error: Error::Backpressure
            }]
        ));
        assert_eq!(fake.borrow().calls, ["start", "connect", "tick", "cancel"]);
        assert!(adapter.connect(PEER, Duration::ZERO).is_ok());
    }

    #[test]
    fn private_identifiers_and_comparison_codes_are_not_debug_logged() {
        assert!(!format!("{PEER:?}").contains("02:01"));
        assert!(!format!("{:?}", ComparisonCode(123456)).contains("123456"));
        assert!(!Address([0; 6]).valid());
        assert!(!Address([255; 6]).valid());
    }

    #[test]
    fn lifecycle_wire_and_display_are_fixed_and_preserve_raw_bits() {
        for kind in 1..=27 {
            let event = DiagnosticEvent::decode([kind, 0xE00002CD, 1]).unwrap();
            assert_eq!(event.kind as u32, kind);
            assert_eq!(event.raw_ior_return, Some(0xE00002CD));
            assert!(event.to_string().ends_with("IOReturn=0xE00002CD"));
            let absent = DiagnosticEvent::decode([kind, u32::MAX, 0]).unwrap();
            assert_eq!(absent.raw_ior_return, None);
            assert!(absent.to_string().ends_with("IOReturn=unavailable"));
        }
        assert_eq!(DiagnosticEvent::decode([0, 0, 0]), None);
        assert_eq!(DiagnosticEvent::decode([28, 0, 0]), None);
        assert_eq!(DiagnosticEvent::decode([1, 0, 2]), None);
    }

    #[test]
    fn cleanup_snapshot_decoder_and_formatter_are_fixed_and_bounded() {
        for bit in 0..13 {
            let snapshot = CleanupSnapshot::from_bits(1 << bit).unwrap();
            assert_eq!(snapshot.bits(), 1 << bit);
            assert_eq!(snapshot.to_string().matches("=true").count(), 1);
            assert_eq!(snapshot.to_string().matches("=false").count(), 12);
        }
        let clear = CleanupSnapshot::from_bits(0).unwrap();
        assert_eq!(clear.to_string().matches("=false").count(), 13);
        assert!(clear.to_string().contains("listener_token_owned=false"));
        assert!(CleanupSnapshot::from_bits(0x1FFF).is_some());
        assert!(CleanupSnapshot::from_bits(0x2000).is_none());
        assert!(CleanupSnapshot::from_bits(u32::MAX).is_none());
    }

    #[test]
    fn no_application_loop_delegate_window_or_logging_in_native_library() {
        let source = include_str!("adapter.m");
        for forbidden in [
            "NSApplication",
            "NSWindow",
            "NSAlert",
            "NSLog",
            "runModal",
            "NSApp ",
        ] {
            assert!(!source.contains(forbidden), "{forbidden}");
        }
    }
}
