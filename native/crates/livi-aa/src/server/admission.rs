// SPDX-License-Identifier: GPL-3.0-or-later

//! Admission is exposure reduction, NOT Bluetooth/TCP peer authentication.
//! Shared production dispatch uses one listener and disjoint destination paths.
//! Android Auto TLS does not bind a TCP peer to the selected Bluetooth phone.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::unix::fs::MetadataExt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::task::JoinSet;

const WPP_WINDOW: Duration = Duration::from_secs(30);
const TCP_WINDOW: Duration = Duration::from_secs(15);
const REJECTION_LIMIT: u8 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    Disabled,
    NativeNotSelected,
    NotReady,
    InvalidAddress,
    AddressChanged,
    WrongPort,
    BindConflict,
    BindFailed,
    AcceptFailed,
    Busy,
    Stale,
    WppFailed,
    WppTimeout,
    WaitingTcpTimeout,
    InvalidPeer,
    AdmissionLimit,
    SessionSocketFailed,
    Cancelled,
    ListenerStopped,
    UsbActive,
    IdentityUnverified,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "native TCP: {self:?}")
    }
}

impl std::error::Error for Failure {}

/// Must additionally be verified against an active Mac interface by the helper.
/// No guessed /24 mask, DNS lookup, route mutation, or phone identity claim.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Network {
    ip: Ipv4Addr,
    mask: u32,
}

impl Network {
    pub fn new(ip: Ipv4Addr, mask: Ipv4Addr) -> Result<Self, Failure> {
        let mask = u32::from(mask);
        let host = u32::from(ip) & !mask;
        if !ip.is_private()
            || !(8..=30).contains(&mask.leading_ones())
            || mask.leading_ones() + mask.trailing_zeros() != 32
            || host == 0
            || host == !mask
        {
            return Err(Failure::InvalidAddress);
        }
        Ok(Self { ip, mask })
    }

    pub fn ip(self) -> Ipv4Addr {
        self.ip
    }

    fn accepts(self, peer: SocketAddr, local: SocketAddr) -> bool {
        let (IpAddr::V4(peer), IpAddr::V4(local)) = (peer.ip(), local.ip()) else {
            return false;
        };
        // The zero-mask sentinel cannot be constructed by Network::new. It
        // exists only in this module's tests, never in production admission.
        #[cfg(any(test, feature = "offline-admission-test-support"))]
        if self.ip == Ipv4Addr::LOCALHOST && self.mask == 0 {
            return peer.is_loopback() && local == self.ip;
        }
        let peer = u32::from(peer);
        local == self.ip
            && peer != u32::from(self.ip)
            && peer & self.mask == u32::from(self.ip) & self.mask
            && peer & !self.mask != 0
            && peer & !self.mask != !self.mask
    }
}

#[derive(Clone, Copy)]
pub enum Selection {
    Legacy,
    Native(Network),
    Shared,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenerState {
    Disabled,
    Binding,
    Ready,
    Failed(Failure),
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Wpp,
    WaitingTcp,
    Active,
    Finished,
    Failed(Failure),
}

impl Phase {
    fn terminal(self) -> bool {
        matches!(self, Self::Finished | Self::Failed(_))
    }
}

struct Attempt {
    id: u64,
    generation: u64,
    phase: Mutex<Phase>,
    cancel: Arc<Notify>,
    handed_over: std::sync::atomic::AtomicBool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Context {
    pub runtime: u64,
    pub attempt: u64,
    pub generation: u64,
}

impl Context {
    pub fn valid(self) -> bool {
        self.runtime != 0 && self.attempt != 0 && self.generation != 0
    }
}

struct Slot {
    attempt: Arc<Attempt>,
    network: Network,
    deadline: Option<Instant>,
    dispatched: bool,
    rejected: u8,
}

struct State {
    runtime: u64,
    session_directory: Option<std::path::PathBuf>,
    dongle_destination: Option<Arc<dyn Fn() -> Option<Ipv4Addr> + Send + Sync>>,
    selection: Selection,
    listener: ListenerState,
    last_failure: Option<Failure>,
    next_id: u64,
    slot: Option<Slot>,
    #[cfg(feature = "offline-admission-test-support")]
    offline_loopback: bool,
}

#[derive(Clone)]
pub struct Control(Arc<Mutex<State>>);

impl Control {
    pub fn new(selection: Selection) -> Self {
        Self(Arc::new(Mutex::new(State {
            runtime: 0,
            session_directory: None,
            dongle_destination: None,
            selection,
            listener: match selection {
                Selection::Legacy => ListenerState::Disabled,
                Selection::Native(_) | Selection::Shared => ListenerState::Binding,
            },
            last_failure: None,
            next_id: 0,
            slot: None,
            #[cfg(feature = "offline-admission-test-support")]
            offline_loopback: false,
        })))
    }

    pub fn is_legacy(&self) -> bool {
        matches!(self.0.lock().unwrap().selection, Selection::Legacy)
    }

    /// The helper supplies a passive, current lookup of its dongle-facing IP.
    /// This is routing/exposure control, never evidence of phone identity.
    pub fn shared(
        runtime: u64,
        dongle_destination: impl Fn() -> Option<Ipv4Addr> + Send + Sync + 'static,
    ) -> Result<Self, Failure> {
        if runtime == 0 {
            return Err(Failure::Stale);
        }
        let control = Self::new(Selection::Shared);
        {
            let mut state = control.0.lock().unwrap();
            state.runtime = runtime;
            state.dongle_destination = Some(Arc::new(dongle_destination));
        }
        Ok(control)
    }

    fn dongle_destination(&self) -> Option<Ipv4Addr> {
        let lookup = self.0.lock().unwrap().dongle_destination.clone();
        lookup.and_then(|lookup| lookup())
    }

    pub fn runtime(&self) -> u64 {
        self.0.lock().unwrap().runtime
    }

    pub fn native(runtime: u64, network: Network) -> Result<Self, Failure> {
        if runtime == 0 {
            return Err(Failure::Stale);
        }
        let control = Self::new(Selection::Native(network));
        control.0.lock().unwrap().runtime = runtime;
        Ok(control)
    }

    pub fn status(&self) -> (ListenerState, Option<Failure>) {
        let state = self.0.lock().unwrap();
        (state.listener, state.last_failure)
    }

    /// The caller additionally verifies UID and exact runtime authorization.
    pub fn native_isolated(
        runtime: u64,
        network: Network,
        directory: std::path::PathBuf,
    ) -> Result<Self, Failure> {
        let meta =
            std::fs::symlink_metadata(&directory).map_err(|_| Failure::SessionSocketFailed)?;
        if !meta.is_dir()
            || meta.mode() & 0o777 != 0o700
            || directory.parent() != Some(std::path::Path::new("/tmp"))
            || !directory
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("livi-bt-dev-"))
        {
            return Err(Failure::SessionSocketFailed);
        }
        let control = Self::native(runtime, network)?;
        control.0.lock().unwrap().session_directory = Some(directory);
        Ok(control)
    }

    fn bind_session(&self, attempt: u64) -> Option<(tokio::net::UnixListener, String)> {
        use std::os::unix::fs::PermissionsExt;
        let state = self.0.lock().unwrap();
        // Retain the original unqualified compatibility/test composition.
        if state.runtime == 0 || attempt == 0 {
            return super::bind_session_socket("aa-session");
        }
        let path = match &state.session_directory {
            Some(directory) => directory.join(format!("aa-{attempt}.sock")),
            None => {
                std::env::temp_dir().join(format!("aa-native-{}-{attempt}.sock", state.runtime))
            }
        };
        let text = path.to_str()?.to_owned();
        let listener = tokio::net::UnixListener::bind(&path).ok()?;
        // No unlink of pre-existing endpoints, including symlinks.
        if std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).is_err() {
            drop(listener);
            let _ = std::fs::remove_file(&path);
            return None;
        }
        Some((listener, text))
    }

    pub fn report(&self, failure: Failure) {
        self.0.lock().unwrap().last_failure = Some(failure);
    }

    /// Readiness is established only after the sole socket's bind succeeds.
    fn ready(&self, local: SocketAddr, port: u16) -> Result<(), Failure> {
        let mut state = self.0.lock().unwrap();
        let expected = match state.selection {
            Selection::Native(network) => network.ip,
            Selection::Shared => Ipv4Addr::UNSPECIFIED,
            Selection::Legacy => return Err(Failure::NativeNotSelected),
        };
        #[cfg(feature = "offline-admission-test-support")]
        let expected = if state.offline_loopback {
            Ipv4Addr::LOCALHOST
        } else {
            expected
        };
        if local.ip() != IpAddr::V4(expected) || local.port() != port {
            return Err(Failure::AddressChanged);
        }
        state.listener = ListenerState::Ready;
        Ok(())
    }

    pub fn reserve(
        &self,
        generation: u64,
        network: Network,
        now: Instant,
    ) -> Result<Lease, Failure> {
        self.tick(now);
        let dongle = self.dongle_destination();
        let mut state = self.0.lock().unwrap();
        let error = match state.selection {
            Selection::Legacy => Some(Failure::NativeNotSelected),
            Selection::Native(selected) if selected != network => Some(Failure::AddressChanged),
            Selection::Shared if dongle == Some(network.ip) => Some(Failure::AddressChanged),
            _ if state.listener != ListenerState::Ready => Some(Failure::NotReady),
            _ if generation == 0 => Some(Failure::Stale),
            _ if state.slot.is_some() => Some(Failure::Busy),
            _ => None,
        };
        if let Some(error) = error {
            state.last_failure = Some(error);
            return Err(error);
        }
        let id = state.next_id.checked_add(1).ok_or(Failure::Busy)?;
        state.next_id = id;
        let attempt = Arc::new(Attempt {
            id,
            generation,
            phase: Mutex::new(Phase::Wpp),
            cancel: Arc::new(Notify::new()),
            handed_over: std::sync::atomic::AtomicBool::new(false),
        });
        state.slot = Some(Slot {
            attempt: attempt.clone(),
            network,
            deadline: Some(now + WPP_WINDOW),
            dispatched: false,
            rejected: 0,
        });
        Ok(Lease {
            control: self.clone(),
            attempt,
        })
    }

    pub fn tick(&self, now: Instant) {
        let mut state = self.0.lock().unwrap();
        if let Some(slot) = &state.slot {
            let mut phase = slot.attempt.phase.lock().unwrap();
            if slot.deadline.is_some_and(|deadline| now >= deadline) && !phase.terminal() {
                let error = if *phase == Phase::Wpp {
                    Failure::WppTimeout
                } else {
                    Failure::WaitingTcpTimeout
                };
                *phase = Phase::Failed(error);
                slot.attempt.cancel.notify_one();
            }
        }
        let ended = state.slot.as_ref().and_then(|slot| {
            let phase = *slot.attempt.phase.lock().unwrap();
            (phase.terminal() && !slot.dispatched).then_some(phase)
        });
        if let Some(phase) = ended {
            if let Phase::Failed(error) = phase {
                state.last_failure = Some(error);
            }
            state.slot = None;
        }
    }

    fn fail_listener(&self, error: Failure) {
        let mut state = self.0.lock().unwrap();
        state.listener = ListenerState::Failed(error);
        state.last_failure = Some(error);
        if let Some(slot) = &state.slot {
            *slot.attempt.phase.lock().unwrap() = Phase::Failed(error);
            slot.attempt.cancel.notify_one();
        }
    }

    fn admit(
        &self,
        peer: SocketAddr,
        local: SocketAddr,
        now: Instant,
    ) -> Result<Arc<Attempt>, Failure> {
        self.tick(now);
        let mut state = self.0.lock().unwrap();
        if state.listener != ListenerState::Ready {
            return Err(Failure::NotReady);
        }
        if matches!(state.selection, Selection::Legacy) {
            return Err(Failure::NativeNotSelected);
        }
        let slot = state.slot.as_mut().ok_or(Failure::NotReady)?;
        if *slot.attempt.phase.lock().unwrap() != Phase::WaitingTcp {
            return Err(Failure::NotReady);
        }
        if !slot.network.accepts(peer, local) {
            slot.rejected += 1;
            if slot.rejected >= REJECTION_LIMIT {
                *slot.attempt.phase.lock().unwrap() = Phase::Failed(Failure::AdmissionLimit);
                slot.attempt.cancel.notify_one();
                state.last_failure = Some(Failure::AdmissionLimit);
            }
            return Err(Failure::InvalidPeer);
        }
        slot.dispatched = true;
        slot.deadline = None;
        *slot.attempt.phase.lock().unwrap() = Phase::Active;
        Ok(slot.attempt.clone())
    }

    /// None means the explicit dongle route, not failed native admission.
    fn dispatch(
        &self,
        peer: SocketAddr,
        local: SocketAddr,
        now: Instant,
    ) -> Result<Option<Arc<Attempt>>, Failure> {
        // A busy dongle route must not postpone native WPP/TCP deadlines by
        // repeatedly winning the accept branch over the idle timer.
        self.tick(now);
        let dongle = self.dongle_destination();
        {
            let state = self.0.lock().unwrap();
            if matches!(state.selection, Selection::Shared)
                && dongle.is_some_and(|ip| local.ip() == IpAddr::V4(ip))
            {
                if state.listener != ListenerState::Ready
                    || !matches!(peer.ip(), IpAddr::V4(_))
                    || state
                        .slot
                        .as_ref()
                        .is_some_and(|slot| local.ip() == IpAddr::V4(slot.network.ip))
                {
                    return Err(Failure::InvalidPeer);
                }
                return Ok(None);
            }
        }
        self.admit(peer, local, now).map(Some)
    }

    fn session_ended(&self, id: u64) {
        let mut state = self.0.lock().unwrap();
        if let Some(slot) = &state.slot
            && slot.attempt.id == id
        {
            let mut phase = slot.attempt.phase.lock().unwrap();
            if !phase.terminal() {
                *phase = Phase::Finished;
            }
            drop(phase);
            state.slot = None;
        }
    }
}

/// Dropping one lease cancels only that reservation/session. IDs are helper-local
/// unique tokens, so reused Bluetooth generations cannot cancel another owner.
pub struct Lease {
    control: Control,
    attempt: Arc<Attempt>,
}

impl Lease {
    pub fn context(&self) -> Context {
        Context {
            runtime: self.control.0.lock().unwrap().runtime,
            attempt: self.attempt.id,
            generation: self.attempt.generation,
        }
    }

    /// The TCP task already owns its SessionOwner. Only Active may transfer.
    /// The returned guard remains responsible for explicit control-channel
    /// cancellation; dropping the bootstrap lease alone no longer tears down AA.
    pub fn handover(&self) -> Result<SessionLease, Failure> {
        let state = self.control.0.lock().unwrap();
        let slot = state.slot.as_ref().ok_or(Failure::Stale)?;
        if slot.attempt.id != self.attempt.id
            || !slot.dispatched
            || *self.attempt.phase.lock().unwrap() != Phase::Active
            || self
                .attempt
                .handed_over
                .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return Err(Failure::Stale);
        }
        Ok(SessionLease {
            control: self.control.clone(),
            attempt: self.attempt.clone(),
        })
    }

    pub fn generation(&self) -> u64 {
        self.attempt.generation
    }

    pub fn phase(&self, now: Instant) -> Phase {
        self.control.tick(now);
        *self.attempt.phase.lock().unwrap()
    }

    pub fn wpp_complete(&self, now: Instant) -> Result<(), Failure> {
        self.control.tick(now);
        let mut state = self.control.0.lock().unwrap();
        if state.listener != ListenerState::Ready {
            return Err(Failure::NotReady);
        }
        let slot = state.slot.as_mut().ok_or(Failure::Stale)?;
        if slot.attempt.id != self.attempt.id {
            return Err(Failure::Stale);
        }
        let mut phase = self.attempt.phase.lock().unwrap();
        if *phase != Phase::Wpp {
            return Err(Failure::Stale);
        }
        *phase = Phase::WaitingTcp;
        slot.deadline = Some(now + TCP_WINDOW);
        Ok(())
    }

    pub fn fail(&self, error: Failure) {
        let mut phase = self.attempt.phase.lock().unwrap();
        if !phase.terminal() {
            *phase = Phase::Failed(error);
            self.attempt.cancel.notify_one();
        }
        drop(phase);
        self.control.report(error);
        self.control.tick(Instant::now());
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if !self
            .attempt
            .handed_over
            .load(std::sync::atomic::Ordering::Acquire)
            && !self.phase(Instant::now()).terminal()
        {
            self.fail(Failure::Cancelled);
        }
    }
}

/// Scoped control ownership after the TCP task has adopted the session.
pub struct SessionLease {
    control: Control,
    attempt: Arc<Attempt>,
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        let lease = Lease {
            control: self.control.clone(),
            attempt: self.attempt.clone(),
        };
        if !lease.phase(Instant::now()).terminal() {
            lease.fail(Failure::Cancelled);
        }
    }
}

struct ListenerOwner(Control);

impl Drop for ListenerOwner {
    fn drop(&mut self) {
        if self.0.status().0 == ListenerState::Ready {
            self.0.fail_listener(Failure::ListenerStopped);
        }
    }
}

struct SessionOwner {
    control: Control,
    id: u64,
    path: String,
    file_id: Option<(u64, u64)>,
}

impl Drop for SessionOwner {
    fn drop(&mut self) {
        if let Ok(meta) = std::fs::symlink_metadata(&self.path)
            && self.file_id == Some((meta.dev(), meta.ino()))
        {
            let _ = std::fs::remove_file(&self.path);
        }
        self.control.session_ended(self.id);
    }
}

pub(super) async fn run(
    port: u16,
    on_session: impl Fn(&str, IpAddr) + Send + Sync + 'static,
    control: Control,
) {
    run_qualified(port, move |path, peer, _| on_session(path, peer), control).await;
}

pub(super) async fn run_qualified(
    port: u16,
    on_session: impl Fn(&str, IpAddr, Option<Context>) + Send + Sync + 'static,
    control: Control,
) {
    let selection = control.0.lock().unwrap().selection;
    let ip = match selection {
        Selection::Native(network) => network.ip,
        Selection::Shared => Ipv4Addr::UNSPECIFIED,
        Selection::Legacy => {
            control.fail_listener(Failure::NativeNotSelected);
            return;
        }
    };
    if port != crate::consts::TCP_PORT {
        control.fail_listener(Failure::WrongPort);
        return;
    }
    // Bind once, exclusively. No SO_REUSEPORT, retry, process signalling, fallback
    // to wildcard, or replacement of an occupied listener.
    let listener = match bind_once(ip, port).await {
        Ok(listener) => listener,
        Err(error) => {
            control.fail_listener(error);
            return;
        }
    };
    serve_qualified(listener, port, on_session, control).await;
}

async fn bind_once(ip: Ipv4Addr, port: u16) -> Result<TcpListener, Failure> {
    let bind = || -> std::io::Result<TcpListener> {
        let socket = tokio::net::TcpSocket::new_v4()?;
        socket.set_reuseaddr(false)?;
        socket.set_reuseport(false)?;
        socket.bind(SocketAddr::from((ip, port)))?;
        socket.listen(16)
    };
    bind().map_err(|error| {
        if error.kind() == std::io::ErrorKind::AddrInUse {
            Failure::BindConflict
        } else {
            Failure::BindFailed
        }
    })
}

#[cfg(test)]
async fn serve(
    listener: TcpListener,
    port: u16,
    on_session: impl Fn(&str, IpAddr) + Send + Sync + 'static,
    control: Control,
) {
    serve_qualified(
        listener,
        port,
        move |path, peer, _| on_session(path, peer),
        control,
    )
    .await;
}

async fn serve_qualified(
    listener: TcpListener,
    port: u16,
    on_session: impl Fn(&str, IpAddr, Option<Context>) + Send + Sync + 'static,
    control: Control,
) {
    let local = match listener.local_addr() {
        Ok(local) => local,
        Err(_) => {
            control.fail_listener(Failure::BindFailed);
            return;
        }
    };
    if let Err(error) = control.ready(local, port) {
        control.fail_listener(error);
        return;
    }
    let _owner = ListenerOwner(control.clone());
    let mut sessions = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            Some(_) = sessions.join_next(), if !sessions.is_empty() => {}
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                control.tick(Instant::now());
            }
            connection = listener.accept() => {
                let (tcp, peer) = match connection {
                    Ok(connection) => connection,
                    Err(_) => {
                        control.fail_listener(Failure::AcceptFailed);
                        return;
                    }
                };
                let Ok(local) = tcp.local_addr() else { continue };
                let Ok(attempt) = control.dispatch(peer, local, Instant::now()) else {
                    continue;
                };
                // Only native ownership is limited to one reservation. Do not
                // impose a new concurrency limit on the existing dongle route.
                if let Err(error) = super::tune(&tcp) {
                    if let Some(attempt) = &attempt {
                        *attempt.phase.lock().unwrap() = Phase::Failed(Failure::AcceptFailed);
                        control.report(Failure::AcceptFailed);
                        control.session_ended(attempt.id);
                        continue;
                    }
                    // Match the original listener: legacy socket options are
                    // best effort and must not suppress its session callback.
                    eprintln!("[aa-tcp] {peer}: socket options: {error}");
                }
                let id = attempt.as_ref().map_or(0, |attempt| attempt.id);
                let Some((node, path)) = control.bind_session(id) else {
                    if let Some(attempt) = &attempt {
                        *attempt.phase.lock().unwrap() = Phase::Failed(Failure::SessionSocketFailed);
                        control.report(Failure::SessionSocketFailed);
                        control.session_ended(attempt.id);
                    }
                    continue;
                };
                let file_id = std::fs::symlink_metadata(&path).ok().map(|m| (m.dev(), m.ino()));
                let owner = SessionOwner {
                    control: control.clone(),
                    id,
                    path: path.clone(),
                    file_id,
                };
                // Exactly the existing event callback and session runner. Neither
                // WPP nor TCP acceptance is a projection-success milestone.
                let context = attempt.as_ref().map(|attempt| Context {
                    runtime: control.runtime(),
                    attempt: attempt.id,
                    generation: attempt.generation,
                });
                on_session(&path, peer.ip(), context.filter(|context| context.valid()));
                sessions.spawn(async move {
                    let _owner = owner;
                    let cancel = attempt.as_ref()
                        .map_or_else(|| Arc::new(Notify::new()), |attempt| attempt.cancel.clone());
                    tokio::select! {
                        biased;
                        _ = cancel.notified() => {}
                        _ = crate::session::run(
                            tcp,
                            crate::session::Peer { label: peer.to_string(), ip: peer.ip() },
                            node,
                            path,
                            Arc::new(Notify::new()),
                        ) => {}
                    }
                });
            }
        }
    }
}

/// Opt-in cross-crate offline fixture, not helper authorization. Production
/// NativeTcp still rejects authorization independently. Never binds port 5277,
/// never accepts an interface/address parameter, and never accesses hardware.
#[cfg(feature = "offline-admission-test-support")]
pub mod offline_test_support {
    use super::*;

    /// `register` must synchronously retain join ownership; dropping its ticket
    /// must request cancellation without dropping the supervisor's join owner.
    /// `stop` must abort and await that owner. Readiness cancellation/panic drops
    /// the ticket; the external supervisor still drains the retained join.
    /// Cleanup is cooperative, not a hard-bounded teardown guarantee.
    pub async fn supervised_shared_listener<T, F>(
        runtime: u64,
        on_session: impl Fn(&str, IpAddr, Option<Context>) + Send + Sync + 'static,
        register: impl FnOnce(tokio::task::JoinHandle<()>) -> T,
        stop: impl FnOnce(T) -> F,
    ) -> Result<(Control, Network, SocketAddr, T), Failure>
    where
        F: std::future::Future<Output = ()>,
    {
        // All fallible setup precedes spawn. Nothing can await between spawn
        // and synchronous transfer to the supervisor.
        let listener = bind_once(Ipv4Addr::LOCALHOST, 0).await?;
        let address = listener.local_addr().map_err(|_| Failure::BindFailed)?;
        let control = Control::shared(runtime, || None)?;
        control.0.lock().unwrap().offline_loopback = true;
        let worker_control = control.clone();
        let worker = tokio::spawn(async move {
            serve_qualified(listener, address.port(), on_session, worker_control).await;
        });
        let ticket = register(worker);
        let ready = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match control.status().0 {
                    ListenerState::Ready => break Ok(()),
                    ListenerState::Failed(error) => break Err(error),
                    _ => tokio::time::sleep(Duration::from_millis(1)).await,
                }
            }
        })
        .await;
        if !matches!(ready, Ok(Ok(()))) {
            stop(ticket).await;
            return Err(Failure::NotReady);
        }
        Ok((
            control,
            Network { ip: Ipv4Addr::LOCALHOST, mask: 0 },
            address,
            ticket,
        ))
    }

    pub async fn shared_listener(
        runtime: u64,
        on_session: impl Fn(&str, IpAddr, Option<Context>) + Send + Sync + 'static,
    ) -> Result<(Control, Network, SocketAddr, tokio::task::JoinHandle<()>), Failure> {
        let listener = bind_once(Ipv4Addr::LOCALHOST, 0).await?;
        let address = listener.local_addr().map_err(|_| Failure::BindFailed)?;
        let control = Control::shared(runtime, || None)?;
        control.0.lock().unwrap().offline_loopback = true;
        let worker_control = control.clone();
        let worker = tokio::spawn(async move {
            serve_qualified(listener, address.port(), on_session, worker_control).await;
        });
        let ready = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match control.status().0 {
                    ListenerState::Ready => break Ok(()),
                    ListenerState::Failed(error) => break Err(error),
                    _ => tokio::time::sleep(Duration::from_millis(1)).await,
                }
            }
        })
        .await;
        if !matches!(ready, Ok(Ok(()))) {
            worker.abort();
            let _ = worker.await;
            return Err(Failure::NotReady);
        }
        Ok((
            control,
            Network {
                ip: Ipv4Addr::LOCALHOST,
                mask: 0,
            },
            address,
            worker,
        ))
    }
}

#[cfg(test)]
#[path = "admission_tests.rs"]
mod tests;
