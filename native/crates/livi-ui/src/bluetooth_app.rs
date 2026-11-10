// SPDX-License-Identifier: GPL-3.0-or-later
//! Main Devices diagnostics and the preserved legacy opt-in Bluetooth-only view.
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use livi_bt_diagnostic::{Failure, Options, Radio, Runner, Stage};
use livi_native_bluetooth::{
    Address, Backend, CleanupSnapshot, DiagnosticEvent, DiagnosticFailure, Error, Event, MacBackend,
};
use slint::{ComponentHandle, Timer, TimerMode};
use crate::AppWindow;

const CONSENT_WINDOW: Duration = Duration::from_secs(60);
static CANCELLED: AtomicBool = AtomicBool::new(false);

fn request(args: &[String], root: bool, foreign: bool) -> Result<Options, Failure> {
    if root || foreign
        || args.iter().filter(|arg| arg.as_str() == "--bluetooth-only-test").count() != 1
    {
        return Err(Failure::Unauthorized);
    }
    let rest: Vec<_> = args.iter()
        .filter(|arg| arg.as_str() != "--bluetooth-only-test").cloned().collect();
    Options::parse(&rest)
}

/// Construction is inert; the factory is invoked only by an explicit click.
struct Session<R: Radio> {
    options: Option<Options>,
    configuring: bool,
    selection_error: bool,
    runner: Option<Runner<R>>,
    result: Option<Result<(), Failure>>,
    created: Duration,
    closing: bool,
    transport_observed: bool,
    evidence: String,
}
impl<R: Radio> Session<R> {
    fn new(options: Options, now: Duration) -> Self {
        Self { options: Some(options), configuring: false, selection_error: false, runner: None, result: None, created: now, closing: false, transport_observed: false, evidence: String::new() }
    }
    fn configure(now: Duration) -> Self {
        Self { options: None, configuring: true, selection_error: false, runner: None, result: None, created: now, closing: false, transport_observed: false, evidence: String::new() }
    }
    fn select(&mut self, address: &str, consent: bool) -> Result<(), Failure> {
        self.selection_error = true;
        if !self.configuring || !self.can_connect() || !consent {
            return Err(Failure::Unauthorized);
        }
        let options = Options::parse(&[
            "--authorize-bluetooth-hardware".into(), "--paired-phone-only".into(),
            "--phone-address".into(), address.into(),
        ])?;
        self.options = Some(options);
        self.configuring = false;
        self.selection_error = false;
        Ok(())
    }
    fn can_connect(&self) -> bool {
        (self.options.is_some() || self.configuring) && !self.closing && self.result.is_none()
    }
    fn connect(&mut self, now: Duration, factory: impl FnOnce() -> Result<R, Failure>) {
        if !self.can_connect() || self.configuring { return; }
        if now < self.created || now.saturating_sub(self.created) >= CONSENT_WINDOW {
            self.options.take();
            self.result = Some(Err(Failure::Unauthorized));
            return;
        }
        let options = self.options.take().unwrap();
        match factory().and_then(|radio| Runner::new(radio, options, now)) {
            Ok(runner) => self.runner = Some(runner),
            Err(error) => {
                if let Failure::Native(detail) = error {
                    self.evidence = format!("Native startup failure: {detail}. No selected-peer stream accepted.");
                }
                self.result = Some(Err(error));
            }
        }
    }
    fn cancel(&mut self, now: Duration, close: bool) {
        self.closing |= close;
        self.configuring = false;
        self.options.take();
        if let Some(runner) = &mut self.runner {
            runner.step(now, true);
            self.collect();
        } else if self.result.is_none() {
            self.result = Some(Err(Failure::Cancelled));
        }
    }
    fn tick(&mut self, now: Duration, signal: bool) {
        if signal {
            self.cancel(now, true);
        } else if let Some(runner) = &mut self.runner {
            runner.step(now, false);
            self.collect();
        } else if self.can_connect() && now.saturating_sub(self.created) >= CONSENT_WINDOW {
            self.options.take();
            self.result = Some(Err(Failure::Unauthorized));
        }
    }
    fn collect(&mut self) {
        if let Some(runner) = &self.runner {
            self.transport_observed = runner.transport_observed();
            let stages: Vec<_> = runner.diagnostic_transcript().iter()
                .map(|event| match event.raw_ior_return {
                    Some(raw) => format!("{:?}(0x{raw:08X})", event.kind),
                    None => format!("{:?}", event.kind),
                }).collect();
            self.evidence = format!(
                "Selected-peer stream observed (historical): {}. Native stages: {}.",
                self.transport_observed, stages.join(" → "),
            );
            if let Some(detail) = runner.diagnostic_failure() {
                self.evidence.push_str(&format!(" Failure: {detail}."));
            }
            if let Some(cleanup) = runner.cleanup_report() {
                self.evidence.push_str(&format!(" Local cleanup: {cleanup:?}."));
            }
        }
        if let Some(result) = self.runner.as_ref().and_then(Runner::result) {
            self.result = Some(result);
            self.runner.take();
        }
    }
    fn can_close(&self) -> bool { self.closing && self.runner.is_none() }
    fn status(&self) -> &'static str {
        if self.selection_error && self.can_connect() {
            return "Enter a valid Classic Bluetooth address and confirm the already-paired phone consent. No Bluetooth access occurred.";
        }
        match self.result {
            Some(Ok(())) => "PASS — RFCOMM and WPP 6.0 identity fields valid; disconnected; cleanup confirmed.",
            Some(Err(Failure::Cancelled)) => "Cancelled — owned cleanup confirmed; no retry.",
            Some(Err(Failure::Cleanup)) => "FAIL — cleanup unconfirmed. Do not retry or claim success.",
            Some(Err(Failure::Unauthorized)) => "Consent expired or unavailable. Close and launch a fresh test.",
            Some(Err(Failure::Pairing)) => "FAIL — existing bond unavailable or pairing requested. No pairing attempted.",
            Some(Err(Failure::Timeout)) if !self.transport_observed =>
                "FAIL — no selected-peer RFCOMM stream accepted before deadline; WPP not reached; owned cleanup confirmed. Phone activation trigger unknown.",
            Some(Err(Failure::Timeout)) => "FAIL — RFCOMM was accepted but WPP did not complete before deadline; owned cleanup confirmed.",
            Some(Err(Failure::Protocol)) => "FAIL — incompatible version/status/identity response; owned cleanup confirmed.",
            Some(Err(_)) => "FAIL — connection or startup refused. No fallback or readiness claim.",
            None => match self.runner.as_ref().map(Runner::stage) {
                None => "Ready — no Bluetooth access yet. Click Connect within 60 seconds.",
                Some(Stage::Starting) => "Starting authorized Bluetooth owner…",
                Some(Stage::Paired) => "Existing bond accepted — waiting for selected phone to open incoming RFCOMM. No outgoing SDP/open or pairing. Activation trigger unknown.",
                Some(Stage::Connected) => "RFCOMM connected — sending one version request…",
                Some(Stage::VersionSent) => "Version request sent — waiting for bounded response…",
                Some(Stage::Cleaning | Stage::IdentityValid | Stage::Stopped) =>
                    "Disconnecting and confirming owned cleanup…",
            },
        }
    }
}

pub(crate) struct Native(MacBackend);
impl Radio for Native {
    fn start(&mut self) -> Result<(), Error> { self.0.diagnostic_start() }
    fn connect_paired(&mut self, generation: u64, peer: Address) -> Result<(), Error> {
        self.0.listen_paired(generation, peer)
    }
    fn tick(&mut self, now: Duration) { self.0.tick(now); }
    fn next_event(&mut self) -> Option<Event> { self.0.next_event() }
    fn cancel(&mut self, generation: u64) { self.0.cancel(generation); }
    fn stop(&mut self) { self.0.stop(); }
    fn idle(&self) -> bool { self.0.diagnostic_idle() }
    fn diagnostic_failure(&self) -> Option<DiagnosticFailure> { self.0.diagnostic_failure() }
    fn next_diagnostic_event(&mut self) -> Option<DiagnosticEvent> { self.0.next_diagnostic_event() }
    fn cleanup_snapshot(&self) -> Option<CleanupSnapshot> { self.0.cleanup_snapshot() }
}
extern "C" fn cancel_signal(_: libc::c_int) {
    CANCELLED.store(true, Ordering::Relaxed);
}
type Handler = Option<unsafe extern "C" fn(libc::c_int)>;
struct Signals(Vec<(libc::c_int, Handler)>);
impl Signals {
    fn install() -> Result<Self, Box<dyn std::error::Error>> {
        CANCELLED.store(false, Ordering::Relaxed);
        let mut installed = Self(Vec::new());
        for number in [libc::SIGINT, libc::SIGTERM] {
            let previous = unsafe { libc::signal(number, Some(cancel_signal)) };
            if previous.is_some_and(|handler| handler as usize == libc::SIG_ERR) {
                return Err("cancellation setup failed".into());
            }
            installed.0.push((number, previous));
        }
        Ok(installed)
    }
}
impl Drop for Signals {
    fn drop(&mut self) {
        for (number, previous) in self.0.drain(..).rev() {
            unsafe { libc::signal(number, previous); }
        }
    }
}
/// UI-local owner: never publishes inventory, identity or authorization to core.
pub(crate) struct Devices<R: Radio> {
    session: Option<Session<R>>,
    consent: Option<(String, Duration)>,
    origin: Instant,
}

impl<R: Radio> Default for Devices<R> {
    fn default() -> Self {
        Self { session: None, consent: None, origin: Instant::now() }
    }
}

impl<R: Radio> Devices<R> {
    fn busy(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.runner.is_some())
    }
    fn retry_safe(&self) -> bool {
        !self.busy() && !self.session.as_ref()
            .is_some_and(|s| s.result == Some(Err(Failure::Cleanup)))
    }
    pub(crate) fn revoke(&mut self) {
        self.consent = None;
        if let Some(session) = &mut self.session {
            session.cancel(self.origin.elapsed(), false);
        }
    }
    pub(crate) fn idle(&self) -> bool { !self.busy() }
    fn consent_valid(&self, selected: Option<&str>, now: Duration) -> bool {
        self.consent.as_ref().is_some_and(|(peer, at)| {
            selected == Some(peer.as_str()) && now >= *at && now - *at < CONSENT_WINDOW
        })
    }
    fn press_at(
        &mut self, action: livi_ui_model::settings::view::Local,
        selected: Option<&str>, allowed: bool, now: Duration,
        factory: impl FnOnce() -> Result<R, Failure>,
    ) {
        use livi_ui_model::settings::view::Local;
        if action == Local::BluetoothCancel {
            self.consent = None;
            if let Some(s) = &mut self.session { s.cancel(now, false); }
            return;
        }
        if !allowed || !self.retry_safe() { return; }
        let Some(peer) = selected else { return };
        match action {
            Local::BluetoothConsent => {
                // Validate syntactically, still no backend construction/access.
                let mut check = Session::<R>::configure(now);
                if check.select(peer, true).is_ok() {
                    self.consent = Some((peer.to_owned(), now));
                }
            }
            Local::BluetoothConnect if self.consent_valid(selected, now) => {
                let (_, created) = self.consent.take().unwrap();
                let mut s = Session::configure(created);
                if s.select(peer, true).is_ok() { s.connect(now, factory); }
                self.session = Some(s);
            }
            _ => {}
        }
    }
    pub(crate) fn rows(&self, selected: Option<&str>, allowed: bool)
        -> Vec<livi_ui_model::settings::view::Row>
    {
        use livi_ui_model::settings::view::{Local, Row, RowKind};
        let button = |label: &str, text: &str, enabled, local| Row {
            kind: RowKind::Button, label: label.into(), button: text.into(),
            enabled, local: Some(local), ..Row::default()
        };
        let info = |label: &str, value: String| Row {
            kind: RowKind::Info, label: label.into(), value, ..Row::default()
        };
        let consent = self.consent_valid(selected, self.origin.elapsed());
        let s = self.session.as_ref();
        let stream = s.is_some_and(|s| s.transport_observed);
        let stage = s.and_then(|s| s.runner.as_ref().map(Runner::stage));
        let valid = s.is_some_and(|s| s.result == Some(Ok(())));
        let mut rows = vec![
            info("Selected paired-phone diagnostic",
                "Development-only native backend. One passive RFCOMM attempt and bounded WPP version exchange. No pairing, remote discovery, Wi-Fi, TCP, TLS or projection startup.".into()),
            button("One-shot Bluetooth consent",
                if consent { "Authorization armed (60 seconds)" } else { "Authorize selected paired phone" },
                allowed && selected.is_some() && self.retry_safe(), Local::BluetoothConsent),
            button("Selected paired phone", "Connect",
                allowed && consent && self.retry_safe(), Local::BluetoothConnect),
            button("Current diagnostic attempt", "Cancel",
                self.busy(), Local::BluetoothCancel),
            info("Discovery · diagnostic",
                "Not performed — local paired inventory is not discovery or a connection.".into()),
            info("RFCOMM · diagnostic", if stream {
                "Selected-peer stream observed (historical); see result for cleanup/current state."
            } else if matches!(stage, Some(Stage::Starting | Stage::Paired)) {
                "Waiting for selected phone's incoming stream; activation trigger unknown."
            } else { "No selected-peer stream observed." }.into()),
            info("WPP · diagnostic", if valid {
                "Version 6.0/status/field-format validation completed; NOT authenticated identity."
            } else if stage == Some(Stage::VersionSent) {
                "Version request sent; waiting for bounded response."
            } else if stream { "Stream observed; WPP completion not confirmed." }
            else { "Not reached — requires a real selected-peer stream." }.into()),
            info("TCP · diagnostic",
                "Blocked / not attempted — Bluetooth diagnostics cannot authorize TCP. Production hardware gates and IdentityUnverified enforcement are unchanged.".into()),
        ];
        let status = if !allowed {
            "Diagnostic unavailable in root, fixture or conflicting isolated mode. No diagnostic hardware access."
        } else { s.map_or("Select an already-paired phone, authorize it, then click Connect. Navigation stays available.", Session::status) };
        let mut result = info("Diagnostic result", status.into());
        result.alert = s.is_some_and(|s| matches!(s.result, Some(Err(_))));
        rows.push(result);
        if let Some(s) = s.filter(|s| !s.evidence.is_empty()) {
            rows.push(info("Live native diagnostic log", s.evidence.clone()));
        }
        rows
    }
    fn tick_at(&mut self, now: Duration, cancel: bool) -> bool {
        let before = self.session.as_ref()
            .map(|s| (s.status(), s.evidence.clone(), s.runner.is_some()));
        let expired = self.consent.as_ref().is_some_and(|(_, at)|
            now < *at || now - *at >= CONSENT_WINDOW);
        if expired || cancel { self.consent = None; }
        if let Some(s) = &mut self.session { s.tick(now, cancel); }
        expired || before != self.session.as_ref()
            .map(|s| (s.status(), s.evidence.clone(), s.runner.is_some()))
    }
    pub(crate) fn tick(&mut self, cancel: bool) -> bool {
        self.tick_at(self.origin.elapsed(), cancel)
    }
}

pub(crate) fn devices_allowed() -> bool {
    (unsafe { libc::geteuid() != 0 })
        && ["LIVI_UI_STATE", "LIVI_NATIVE_BT_DEV_DIR"].iter()
            .all(|key| std::env::var_os(key).is_none())
        && !std::env::args().any(|arg| arg == "--isolated-native-projection")
}

impl Devices<Native> {
    pub(crate) fn press(&mut self, action: livi_ui_model::settings::view::Local,
        selected: Option<&str>, allowed: bool)
    {
        self.press_at(action, selected, allowed, self.origin.elapsed(), ||
            MacBackend::new().map(Native).map_err(|_| Failure::Radio));
    }
}

/// Attached to the existing main AppWindow/event loop, never creates a window.
pub(crate) struct DevicesAttachment {
    timer: Timer,
    _signals: Signals,
}
pub(crate) fn attach_devices(ui: &AppWindow, client: &livi_core_client::Client)
    -> Result<DevicesAttachment, Box<dyn std::error::Error>>
{
    let signals = Signals::install()?;
    let closing = Rc::new(std::cell::Cell::new(false));
    ui.window().on_close_requested({
        let closing = closing.clone();
        move || {
            closing.set(true);
            crate::settings_ui::diagnostic_cancel();
            slint::CloseRequestResponse::KeepWindowShown
        }
    });
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(20), {
        let weak = ui.as_weak();
        let client = client.clone();
        move || {
            let signal = CANCELLED.load(Ordering::Relaxed);
            if signal { closing.set(true); }
            if let Some(ui) = weak.upgrade() {
                let done = crate::settings_ui::diagnostic_tick(&ui, &client, signal);
                if closing.get() && done { let _ = slint::quit_event_loop(); }
            }
        }
    });
    Ok(DevicesAttachment { timer, _signals: signals })
}
impl Drop for DevicesAttachment {
    fn drop(&mut self) {
        self.timer.stop();
        crate::settings_ui::diagnostic_cancel();
    }
}

fn render(ui: &AppWindow, state: &Session<Native>) {
    ui.set_bluetooth_test_status(format!("{}\n{}", state.status(), state.evidence).into());
    ui.set_bluetooth_test_can_connect(state.can_connect());
    ui.set_bluetooth_test_can_cancel(state.runner.is_some());
}

pub fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let foreign = ["LIVI_UI_STATE", "LIVI_NATIVE_BT_DEV_DIR"].iter()
        .any(|name| std::env::var_os(name).is_some())
        || std::env::var_os(livi_core_proto::LIFELINE_ENV).is_some();
    let root = unsafe { libc::geteuid() } == 0;
    let configure = args == ["--bluetooth-only-test"];
    let session = if configure && !root && !foreign {
        Session::<Native>::configure(Duration::ZERO)
    } else {
        Session::<Native>::new(request(args, root, foreign)
            .map_err(|_| "Bluetooth app test authorization/options refused")?, Duration::ZERO)
    };
    let _signals = Signals::install()?;
    let origin = Instant::now();
    let state = Rc::new(RefCell::new(session));
    let ui = AppWindow::new()?;
    ui.set_bluetooth_test_mode(true);
    ui.set_bluetooth_test_configure(configure);
    ui.window().set_size(slint::LogicalSize::new(1000.0, 800.0));
    render(&ui, &state.borrow());
    ui.on_bluetooth_test_connect({
        let state = state.clone();
        let weak = ui.as_weak();
        move |address, consent| {
            let mut state = state.borrow_mut();
            if CANCELLED.load(Ordering::Relaxed) {
                state.cancel(origin.elapsed(), true);
            } else {
                state.tick(origin.elapsed(), false);
                if state.configuring && state.can_connect()
                    && state.select(address.as_str(), consent).is_err()
                {
                    if let Some(ui) = weak.upgrade() {
                        render(&ui, &state);
                    }
                    return;
                }
                state.connect(origin.elapsed(), || MacBackend::new()
                    .map(Native).map_err(|_| Failure::Radio));
            }
            if let Some(ui) = weak.upgrade() { render(&ui, &state); }
        }
    });
    ui.on_bluetooth_test_cancel({
        let state = state.clone();
        let weak = ui.as_weak();
        move || {
            let mut state = state.borrow_mut();
            state.cancel(origin.elapsed(), false);
            if let Some(ui) = weak.upgrade() { render(&ui, &state); }
        }
    });
    ui.window().on_close_requested({
        let state = state.clone();
        move || {
            state.borrow_mut().cancel(origin.elapsed(), true);
            slint::CloseRequestResponse::KeepWindowShown
        }
    });
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(20), {
        let state = state.clone();
        let weak = ui.as_weak();
        move || {
            let mut state = state.borrow_mut();
            state.tick(origin.elapsed(), CANCELLED.load(Ordering::Relaxed));
            if let Some(ui) = weak.upgrade() { render(&ui, &state); }
            if state.can_close() { let _ = slint::quit_event_loop(); }
        }
    });
    ui.show()?;
    let event_loop = crate::run_app_event_loop();
    timer.stop();
    state.borrow_mut().cancel(origin.elapsed(), true);
    let result = state.borrow().result;
    let _ = ui.hide();
    event_loop?;
    match result {
        Some(Ok(())) => Ok(()),
        Some(Err(Failure::Cancelled)) => Err("Bluetooth app test cancelled; no PASS".into()),
        _ => Err("Bluetooth app test failed or cleanup unconfirmed; no readiness claim".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    #[derive(Default)]
    struct Fake {
        starts: usize, connects: usize, pending: bool, events: VecDeque<Event>,
        diagnostics: VecDeque<DiagnosticEvent>,
    }
    struct FakeRadio(Rc<RefCell<Fake>>);
    impl Radio for FakeRadio {
        fn start(&mut self) -> Result<(), Error> { self.0.borrow_mut().starts += 1; Ok(()) }
        fn connect_paired(&mut self, _: u64, _: Address) -> Result<(), Error> {
            self.0.borrow_mut().connects += 1; Ok(())
        }
        fn tick(&mut self, _: Duration) {}
        fn next_event(&mut self) -> Option<Event> { self.0.borrow_mut().events.pop_front() }
        fn cancel(&mut self, _: u64) {}
        fn stop(&mut self) {}
        fn next_diagnostic_event(&mut self) -> Option<DiagnosticEvent> {
            self.0.borrow_mut().diagnostics.pop_front()
        }
        fn idle(&self) -> bool {
            let state = self.0.borrow();
            !state.pending && state.events.is_empty()
        }
    }
    fn args() -> Vec<String> {
        ["--bluetooth-only-test", "--authorize-bluetooth-hardware",
         "--paired-phone-only", "--phone-address", "02:01:02:03:04:05"]
            .into_iter().map(String::from).collect()
    }
    fn setup() -> (Session<FakeRadio>, Rc<RefCell<Fake>>) {
        (Session::new(request(&args(), false, false).unwrap(), Duration::ZERO),
         Rc::new(RefCell::new(Fake::default())))
    }
    #[test]
    fn devices_connect_requires_fresh_peer_scoped_consent_and_never_autostarts() {
        use livi_ui_model::settings::view::Local::*;
        let mut d = Devices::<FakeRadio>::default();
        let peer = Some("02:01:02:03:04:05");
        d.tick_at(Duration::ZERO, false);
        d.press_at(BluetoothConnect, peer, true, Duration::ZERO, || panic!("no consent"));
        for (selection, allowed) in [(None, true), (peer, false), (Some("bad"), true)] {
            d.press_at(BluetoothConsent, selection, allowed, Duration::ZERO, || panic!("consent access"));
            assert!(d.consent.is_none());
        }
        d.press_at(BluetoothConsent, peer, true, Duration::ZERO, || panic!("consent access"));
        d.press_at(BluetoothConnect, Some("02:01:02:03:04:06"), true,
            Duration::ZERO, || panic!("wrong selected peer"));
        assert!(d.session.is_none());
        d.press_at(BluetoothConnect, peer, true, CONSENT_WINDOW, || panic!("expired consent"));
        assert!(d.session.is_none());
        d.press_at(BluetoothConsent, peer, true, CONSENT_WINDOW, || panic!("consent access"));
        let fake = Rc::new(RefCell::new(Fake::default()));
        d.press_at(BluetoothConnect, peer, true, CONSENT_WINDOW, || Ok(FakeRadio(fake.clone())));
        assert_eq!(fake.borrow().starts, 1);
        assert!(d.consent.is_none());
        d.press_at(BluetoothConnect, peer, true, CONSENT_WINDOW, || panic!("double connect"));
        d.press_at(BluetoothConsent, peer, true, CONSENT_WINDOW, || panic!("busy consent"));
        assert!(d.consent.is_none());
        d.press_at(BluetoothCancel, peer, true, CONSENT_WINDOW, || panic!("cancel factory"));
        d.tick_at(CONSENT_WINDOW, false);
        assert!(d.idle());
        assert_eq!(d.session.as_ref().unwrap().result, Some(Err(Failure::Cancelled)));
    }
    #[test]
    fn devices_navigation_revoke_cancels_owner_and_blocks_stale_callbacks() {
        use livi_ui_model::settings::view::Local::*;
        let mut d = Devices::<FakeRadio>::default();
        let peer = Some("02:01:02:03:04:05");
        let fake = Rc::new(RefCell::new(Fake::default()));
        d.press_at(BluetoothConsent, peer, true, Duration::ZERO, || panic!("consent factory"));
        d.revoke();
        d.press_at(BluetoothConnect, peer, true, Duration::ZERO, || panic!("revoked connect"));
        assert!(d.session.is_none());
        d.press_at(BluetoothConsent, peer, true, Duration::ZERO, || panic!("consent factory"));
        d.press_at(BluetoothConnect, peer, true, Duration::ZERO, || Ok(FakeRadio(fake.clone())));
        fake.borrow_mut().pending = true;
        d.revoke();
        assert!(!d.idle());
        d.tick_at(Duration::from_secs(3), false);
        assert!(d.idle());
        assert!(!d.retry_safe());
        d.press_at(BluetoothConsent, peer, true, Duration::from_secs(3), || panic!("unsafe retry"));
        assert!(d.consent.is_none());
        assert!(d.rows(peer, true).iter().any(|r| r.alert && r.value.contains("cleanup unconfirmed")));
    }
    #[test]
    fn devices_timeout_log_is_retained_and_tcp_is_never_claimed() {
        use livi_ui_model::settings::view::Local::*;
        use livi_native_bluetooth::DiagnosticLifecycle::{LocalServiceReady, PairedListenerArmed};
        let mut d = Devices::<FakeRadio>::default();
        let fake = Rc::new(RefCell::new(Fake::default()));
        let peer = Some("02:01:02:03:04:05");
        d.press_at(BluetoothConsent, peer, true, Duration::ZERO, || panic!("consent factory"));
        d.press_at(BluetoothConnect, peer, true, Duration::ZERO, || Ok(FakeRadio(fake.clone())));
        fake.borrow_mut().events.push_back(Event::Availability(true));
        fake.borrow_mut().diagnostics.extend([LocalServiceReady, PairedListenerArmed]
            .map(|kind| DiagnosticEvent { kind, raw_ior_return: None }));
        d.tick_at(Duration::ZERO, false);
        assert!(d.rows(peer, true).iter().any(|r| r.value.contains("Waiting for selected phone")));
        d.tick_at(Duration::from_secs(60), false);
        d.tick_at(Duration::from_secs(60), false);
        let rows = d.rows(peer, true);
        assert!(rows.iter().any(|r| r.label == "Live native diagnostic log"
            && r.value.contains("LocalServiceReady → PairedListenerArmed")));
        assert!(rows.iter().any(|r| r.label == "WPP · diagnostic" && r.value.contains("Not reached")));
        assert!(rows.iter().any(|r| r.label == "TCP · diagnostic" && r.value.contains("Blocked / not attempted")));
        assert!(rows.iter().all(|r| r.action.is_none() && r.change.is_none() && r.tools.is_empty()));
    }
    #[test]
    fn devices_attachment_uses_existing_window_and_normal_startup() {
        let source = include_str!("bluetooth_app.rs");
        let attachment = source.split("pub(crate) fn attach_devices(").nth(1).unwrap()
            .split("fn render(").next().unwrap();
        assert!(!attachment.contains("AppWindow::new"));
        assert!(!attachment.contains("set_bluetooth_test_mode"));
        assert!(!attachment.contains("run_app_event_loop"));
        let main = include_str!("main.rs");
        assert!(main.contains("bluetooth_app::attach_devices(&ui, &client)?"));
        assert!(main.contains("drop(devices_diagnostic)"));
        let settings = include_str!("settings_ui.rs");
        let local = settings.split("Some(local @ (Local::BluetoothConsent").nth(1).unwrap()
            .split("Some(Local::Browse").next().unwrap();
        assert!(local.contains("c.diagnostic.press("));
        assert!(!local.contains("client.act"));
        assert!(!local.contains("Change::Config"));
    }
    fn start(session: &mut Session<FakeRadio>, fake: &Rc<RefCell<Fake>>) -> UnixStream {
        session.connect(Duration::ZERO, || Ok(FakeRadio(fake.clone())));
        let (ours, peer) = UnixStream::pair().unwrap();
        fake.borrow_mut().events.extend([
            Event::Availability(true),
            Event::Connected { generation: 1, stream: ours.into(),
                peer: Address([2, 1, 2, 3, 4, 5]), local: Address([2, 0, 0, 0, 0, 1]) },
        ]);
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        session.tick(Duration::ZERO, false);
        peer
    }
    #[test]
    fn rejects_missing_consent_root_foreign_runtime_and_conflicting_modes() {
        assert!(request(&args(), true, false).is_err());
        assert!(request(&args(), false, true).is_err());
        for index in [0, 1, 2] {
            let mut input = args(); input.remove(index);
            assert!(request(&input, false, false).is_err());
        }
        for flag in ["--bluetooth-demo", "--isolated-native-projection", "--native-ap-config",
                     "--projection", "--bluetooth-only-test", "--bluetooth-only-test=true",
                     "--authorize-bluetooth-hardware=true"] {
            let mut input = args(); input.push(flag.into());
            assert!(request(&input, false, false).is_err());
        }
    }
    #[test]
    fn construction_and_polling_before_click_are_inert() {
        let (mut session, fake) = setup();
        for n in 0..20 { session.tick(Duration::from_millis(n), false); }
        assert!(session.can_connect());
        assert_eq!(fake.borrow().starts, 0);
        assert!(session.status().contains("no Bluetooth access"));
    }
    #[test]
    fn passive_listener_timeout_retains_evidence_and_never_claims_wpp() {
        use livi_native_bluetooth::DiagnosticLifecycle::{LocalServiceReady, PairedListenerArmed};
        let (mut session, fake) = setup();
        session.connect(Duration::ZERO, || Ok(FakeRadio(fake.clone())));
        fake.borrow_mut().events.push_back(Event::Availability(true));
        fake.borrow_mut().diagnostics.extend([LocalServiceReady, PairedListenerArmed]
            .map(|kind| DiagnosticEvent { kind, raw_ior_return: None }));
        session.tick(Duration::ZERO, false);
        assert!(session.status().contains("waiting for selected phone"));
        session.tick(Duration::from_secs(60), false);
        session.tick(Duration::from_secs(60), false);
        assert_eq!(session.result, Some(Err(Failure::Timeout)));
        assert!(session.runner.is_none());
        assert!(!session.transport_observed);
        assert!(session.status().contains("WPP not reached"));
        assert!(session.evidence.contains("LocalServiceReady → PairedListenerArmed"));
        assert!(!session.status().contains("PASS"));
    }
    #[test]
    fn accepted_stream_timeout_is_not_reported_as_missing_incoming() {
        let (mut session, fake) = setup();
        let _peer = start(&mut session, &fake);
        session.tick(Duration::from_secs(10), false);
        session.tick(Duration::from_secs(10), false);
        assert_eq!(session.result, Some(Err(Failure::Timeout)));
        assert!(session.transport_observed);
        assert!(session.evidence.contains("historical): true"));
        assert!(session.status().contains("WPP did not complete"));
        assert!(!session.status().contains("WPP not reached"));
    }
    #[test]
    fn app_adapter_uses_the_same_passive_diagnostic_path_as_standalone() {
        let source = include_str!("bluetooth_app.rs");
        let adapter = source.split("impl Radio for Native {").nth(1).unwrap()
            .split(r#"extern "C""#).next().unwrap();
        assert!(adapter.contains("self.0.diagnostic_start()"));
        assert!(adapter.contains("self.0.listen_paired(generation, peer)"));
        assert!(!adapter.contains("self.0.connect_paired("));
        for method in ["diagnostic_failure", "next_diagnostic_event", "cleanup_snapshot"] {
            assert!(adapter.contains(&format!("self.0.{method}()")));
        }
    }
    #[test]
    fn real_slint_pointer_events_reach_test_buttons_and_ordinary_navigation() {
        use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
        use slint::platform::{Platform, PlatformError, WindowAdapter, WindowEvent, PointerEventButton};
        struct Headless;
        impl Platform for Headless {
            fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
                Ok(MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer))
            }
        }
        fn click(ui: &AppWindow, x: f32, y: f32) {
            let position = slint::LogicalPosition::new(x, y);
            ui.window().dispatch_event(WindowEvent::PointerMoved { position });
            ui.window().dispatch_event(WindowEvent::PointerPressed {
                position, button: PointerEventButton::Left,
            });
            ui.window().dispatch_event(WindowEvent::PointerReleased {
                position, button: PointerEventButton::Left,
            });
        }
        fn find_click(ui: &AppWindow, hit: &Cell<usize>) {
            let before = hit.get();
            for y in (10..790).step_by(20) {
                for x in (10..990).step_by(20) {
                    click(ui, x as f32, y as f32);
                    if hit.get() > before { return; }
                }
            }
            panic!("real pointer events did not reach an enabled control");
        }
        slint::platform::set_platform(Box::new(Headless)).unwrap();
        let ui = AppWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(1000.0, 800.0));
        ui.set_bluetooth_test_mode(true);
        ui.set_bluetooth_test_status("Evidence ".repeat(200).into());
        let connects = Rc::new(Cell::new(0));
        let cancels = Rc::new(Cell::new(0));
        ui.on_bluetooth_test_connect({
            let count = connects.clone();
            move |_, _| count.set(count.get() + 1)
        });
        ui.on_bluetooth_test_cancel({
            let count = cancels.clone();
            move || count.set(count.get() + 1)
        });
        ui.show().unwrap(); // In-memory software adapter, never an OS window.
        // Disabled controls cannot invoke callbacks.
        for y in (10..790).step_by(20) { click(&ui, 100.0, y as f32); }
        assert_eq!(connects.get(), 0);
        assert_eq!(cancels.get(), 0);
        ui.set_bluetooth_test_can_connect(true);
        find_click(&ui, &connects);
        ui.set_bluetooth_test_can_connect(false);
        ui.set_bluetooth_test_can_cancel(true);
        find_click(&ui, &cancels);
        ui.set_bluetooth_test_mode(false);
        let pages = Rc::new(Cell::new(0));
        ui.on_page_chosen({
            let count = pages.clone();
            move |_| count.set(count.get() + 1)
        });
        ui.set_tabs(Rc::new(slint::VecModel::from(vec![crate::TabData {
            page: crate::Page::Settings, icon: Default::default(),
        }])).into());
        ui.invoke_wake_nav();
        find_click(&ui, &pages);
        assert_eq!(ui.get_page(), crate::Page::Settings);
        ui.hide().unwrap();
    }
    #[test]
    fn paired_detail_rows_have_constant_role_and_disabled_accessibility_guard() {
        let source = include_str!("../ui/settings/row.slint");
        let detail = source.split("export component DetailRow").nth(1).unwrap()
            .split("export component Chevron").next().unwrap();
        assert!(!detail.contains("accessible-role: root."));
        assert!(detail.contains("if root.data.kind == RowKind.pick: TouchArea"));
        assert!(detail.contains("accessible-role: button;"));
        assert!(detail.contains("enabled: root.data.enabled;"));
        assert!(detail.contains("accessible-action-default =>"));
        assert!(detail.contains("if (root.data.enabled) { root.press(); }"));
    }
    #[test]
    fn gui_configuration_requires_valid_selection_and_explicit_consent() {
        let mut session = Session::<FakeRadio>::configure(Duration::ZERO);
        session.connect(Duration::ZERO, || panic!("unconfigured radio"));
        for (address, consent) in [
            ("02:01:02:03:04:05", false), ("", true), ("phone name", true),
            ("00:00:00:00:00:00", true), ("03:01:02:03:04:05", true),
            ("02:01:02:03:04:05\n", true),
        ] {
            assert!(session.select(address, consent).is_err());
            session.tick(Duration::from_secs(1), false);
            assert!(session.status().contains("No Bluetooth access occurred"));
            session.connect(Duration::from_secs(1), || panic!("invalid selection radio"));
        }
        session.select("02:01:02:03:04:05", true).unwrap();
        assert!(session.select("02:01:02:03:04:06", true).is_err());
        let fake = Rc::new(RefCell::new(Fake::default()));
        session.connect(Duration::from_secs(2), || Ok(FakeRadio(fake.clone())));
        assert_eq!(fake.borrow().starts, 1);
        assert!(!session.can_connect());
        session.cancel(Duration::from_secs(2), false);
    }
    #[test]
    fn gui_expiry_and_close_forbid_late_selection_without_radio() {
        for close in [false, true] {
            let mut session = Session::<FakeRadio>::configure(Duration::ZERO);
            if close {
                session.cancel(Duration::ZERO, true);
            } else {
                session.tick(CONSENT_WINDOW, false);
            }
            assert!(!session.can_connect());
            assert!(session.select("02:01:02:03:04:05", true).is_err());
            session.connect(CONSENT_WINDOW, || panic!("expired or closed radio"));
        }
    }
    #[test]
    fn expired_consent_never_calls_factory_or_replays() {
        let (mut session, _) = setup();
        let called = Cell::new(false);
        session.connect(CONSENT_WINDOW, || { called.set(true); Err(Failure::Radio) });
        assert!(!called.get());
        assert_eq!(session.result, Some(Err(Failure::Unauthorized)));
        session.connect(Duration::ZERO, || { called.set(true); Err(Failure::Radio) });
        assert!(!called.get());
    }
    #[test]
    fn repeated_clicks_do_not_replace_owner_or_reconnect() {
        let (mut session, fake) = setup();
        session.connect(Duration::ZERO, || Ok(FakeRadio(fake.clone())));
        session.connect(Duration::ZERO, || panic!("duplicate factory"));
        assert_eq!(fake.borrow().starts, 1);
        session.cancel(Duration::ZERO, false);
        session.tick(Duration::ZERO, false);
        session.connect(Duration::ZERO, || panic!("reconnect"));
        assert!(!session.can_connect());
    }
    #[test]
    fn close_before_click_is_inert_and_blocks_late_click() {
        let (mut session, fake) = setup();
        session.cancel(Duration::ZERO, true);
        session.connect(Duration::ZERO, || panic!("closed window"));
        assert!(session.can_close());
        assert_eq!(fake.borrow().starts, 0);
    }

    #[test]
    fn failed_factory_cannot_report_pass_or_retry() {
        let (mut session, _) = setup();
        session.connect(Duration::ZERO, || Err(Failure::Radio));
        assert_eq!(session.result, Some(Err(Failure::Radio)));
        session.connect(Duration::ZERO, || panic!("startup fallback"));
        assert!(!session.status().contains("PASS"));
    }
    #[test]
    fn typed_native_startup_failure_remains_visible_without_a_runner() {
        use livi_native_bluetooth::{DiagnosticCategory, DiagnosticOperation};
        let (mut session, _) = setup();
        let detail = DiagnosticFailure {
            category: DiagnosticCategory::Permission,
            operation: DiagnosticOperation::Start,
            raw_ior_return: Some(0xE00002E2),
        };
        session.connect(Duration::ZERO, || Err(Failure::Native(detail)));
        assert!(session.runner.is_none());
        assert!(session.evidence.contains("category=Permission operation=Start"));
        assert!(session.evidence.contains("IOReturn=0xE00002E2"));
        assert!(!session.transport_observed);
        assert!(!session.status().contains("PASS"));
        assert!(!session.can_connect());
    }
    #[test]
    fn window_close_waits_for_cleanup() {
        let (mut session, fake) = setup();
        let mut peer = start(&mut session, &fake);
        let mut bytes = [0; 8]; peer.read_exact(&mut bytes).unwrap();
        fake.borrow_mut().pending = true;
        session.cancel(Duration::from_secs(1), true);
        assert!(!session.can_close());
        fake.borrow_mut().pending = false;
        session.tick(Duration::from_secs(1), false);
        assert!(session.can_close());
        assert_eq!(session.result, Some(Err(Failure::Cancelled)));
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }
    #[test]
    fn stuck_cleanup_never_passes() {
        let (mut session, fake) = setup();
        let _peer = start(&mut session, &fake);
        fake.borrow_mut().pending = true;
        session.cancel(Duration::from_secs(1), true);
        session.tick(Duration::from_secs(3), false);
        assert_eq!(session.result, Some(Err(Failure::Cleanup)));
        assert!(session.can_close());
        assert!(!session.status().contains("PASS"));
    }
    #[test]
    fn signals_before_or_during_attempt_cancel_without_retry() {
        for active in [false, true] {
            let (mut session, fake) = setup();
            if active { session.connect(Duration::ZERO, || Ok(FakeRadio(fake.clone()))); }
            session.tick(Duration::from_secs(1), true);
            session.tick(Duration::from_secs(1), true);
            assert!(session.can_close());
            assert!(!session.can_connect());
            assert_eq!(fake.borrow().starts, usize::from(active));
        }
    }
    #[test]
    fn real_runner_success_is_disconnected_not_projection() {
        let (mut session, fake) = setup();
        let mut peer = start(&mut session, &fake);
        let mut bytes = [0; 8]; peer.read_exact(&mut bytes).unwrap();
        assert_eq!(bytes, [0, 4, 0, 4, 8, 6, 16, 0]);
        // Synthetic 6.0/status 0, serial "serial", nested mobile ID "id".
        let body = [8, 6, 16, 0, 26, 6, b's', b'e', b'r', b'i', b'a', b'l',
                    32, 0, 50, 4, 10, 2, b'i', b'd'];
        let mut frame = vec![0, body.len() as u8, 0, 5];
        frame.extend_from_slice(&body);
        peer.write_all(&frame).unwrap();
        for _ in 0..5 { session.tick(Duration::from_millis(1), false); }
        assert_eq!(session.result, Some(Ok(())));
        assert!(session.status().contains("disconnected"));
        assert!(!session.can_connect());
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }
    #[test]
    fn dispatch_precedes_normal_startup_and_uses_real_appwindow() {
        let main = include_str!("main.rs");
        let body = &main[main.find("fn main()").unwrap()..];
        let dispatch = body.find("return bluetooth_app::run(&args)").unwrap();
        for marker in ["bundle::find()", "enable_native_projection()", "AppWindow::new()",
                       "connect(started_by_core", "Client::start"] {
            assert!(dispatch < body.find(marker).unwrap(), "{marker}");
        }
        let source = include_str!("bluetooth_app.rs");
        let runtime = source.split("#[cfg(test)]").next().unwrap();
        for denied in ["TcpListener", "TcpStream", "Command::", "Router::", "CoreProcess",
                       "wifi_start_request", "wifi_info_response", "enable_native_projection"] {
            assert!(!runtime.contains(denied), "{denied}");
        }
        assert!(runtime.contains("AppWindow::new()?"));
        assert!(runtime.contains("crate::run_app_event_loop()"));
        assert!(!runtime.contains("run_event_loop_until_quit"));
    }

    #[test]
    fn close_after_valid_response_before_cleanup_cancels_success() {
        let (mut session, fake) = setup();
        let mut peer = start(&mut session, &fake);
        let mut bytes = [0; 8]; peer.read_exact(&mut bytes).unwrap();
        let body = [8, 6, 16, 0, 26, 1, b's', 32, 0, 50, 3, 10, 1, b'i'];
        let mut frame = vec![0, body.len() as u8, 0, 5];
        frame.extend_from_slice(&body);
        fake.borrow_mut().pending = true;
        peer.write_all(&frame).unwrap();
        for _ in 0..5 { session.tick(Duration::from_millis(1), false); }
        assert!(session.result.is_none());
        assert!(session.status().contains("cleanup"));
        session.cancel(Duration::from_millis(2), true);
        fake.borrow_mut().pending = false;
        session.tick(Duration::from_millis(3), false);
        assert_eq!(session.result, Some(Err(Failure::Cancelled)));
        assert!(session.can_close());
    }
}
