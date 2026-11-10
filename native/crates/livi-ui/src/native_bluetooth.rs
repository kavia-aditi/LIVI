// SPDX-License-Identifier: GPL-3.0-or-later

//! Owns the dormant adapter and private bootstrap IPC lifecycle. No additional
//! window/delegate/event loop, TCP listener, or user controls live here.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use livi_core_client::Client;
use livi_native_bluetooth::{Adapter, Error, Gates, MacBackend};
use livi_native_handoff::router::Router;
use serde_json::Value;
use slint::{Timer, TimerMode};

fn configured(state: &Value) -> bool {
    state["config"]["nativeWirelessAa"]["enabled"]
        .as_bool()
        .unwrap_or(false)
}

fn production_gates(
    enabled: bool,
    readiness: Option<livi_native_handoff::ipc::Readiness>,
) -> Gates {
    Gates {
        enabled,
        listener_ready: readiness.is_some_and(|ready| ready.valid()),
        // Stage 4 never authorizes hardware. IPC UID, helper readiness,
        // configured enablement and the explicit request below cannot arm it.
        hardware_authorized: false,
    }
}

/// A one-shot nonvisual owner request. Construction must follow an explicit
/// user authorization action, never saved settings, IPC UID or phone identity.
/// Owns fresh validated credentials; it is neither Clone nor a state snapshot.
#[allow(dead_code)]
pub struct AuthorizedRequest {
    config: livi_native_handoff::config::Config,
    runtime: u64,
    created: Instant,
}

#[allow(dead_code)]
impl AuthorizedRequest {
    pub fn explicit(
        config: livi_native_handoff::config::Config,
        expected_runtime: u64,
    ) -> Result<Self, livi_native_handoff::Error> {
        if expected_runtime == 0 {
            return Err(livi_native_handoff::Error::Stale);
        }
        Ok(Self {
            config,
            runtime: expected_runtime,
            created: Instant::now(),
        })
    }

    fn consume(
        self,
        now: Instant,
    ) -> Result<(u64, livi_native_handoff::config::Config), livi_native_handoff::Error> {
        if now
            .checked_duration_since(self.created)
            .is_none_or(|age| age >= Duration::from_secs(5))
        {
            return Err(livi_native_handoff::Error::TimedOut);
        }
        Ok((self.runtime, self.config))
    }
}

pub struct Attachment {
    timer: Timer,
    owner: Rc<RefCell<Adapter<MacBackend>>>,
    handoff: Rc<RefCell<Router>>,
    origin: Instant,
}

impl Attachment {
    /// Dormant production ingress for a future explicitly authorized caller.
    /// No current config/env/UI path constructs this request. Even a valid
    /// request only starts private IPC; hardware_authorized remains false.
    #[allow(dead_code)]
    pub fn prepare_authorized(
        &self,
        generation: u64,
        request: AuthorizedRequest,
    ) -> Result<(), livi_native_handoff::Error> {
        let (runtime, config) = request.consume(Instant::now())?;
        self.handoff.borrow_mut().prepare_for_runtime(
            generation,
            runtime,
            config,
            self.origin.elapsed(),
        )
    }
}

pub fn attach(client: Client) -> Result<Attachment, Error> {
    // Allocation checks thread affinity but does not construct CBCentralManager.
    let owner = Rc::new(RefCell::new(Adapter::new(MacBackend::new()?)));
    let timer = Timer::default();
    let origin = Instant::now();
    let tick_owner = owner.clone();
    let handoff = Rc::new(RefCell::new(Router::default()));
    let tick_handoff = handoff.clone();
    timer.start(TimerMode::Repeated, Duration::from_millis(50), move || {
        let enabled = configured(&client.state());
        let mut adapter = tick_owner.borrow_mut();
        let now = origin.elapsed();
        let mut handoff = tick_handoff.borrow_mut();
        if !enabled {
            handoff.cancel();
        }
        // Drive readiness/EOF before exposing the gate to the existing owner.
        // Expiry/restart revokes only the exact attempt, never another service.
        if let Some(generation) = handoff.tick(now) {
            adapter.cancel(generation);
        }
        let _ = adapter.configure(production_gates(enabled, handoff.readiness(now)));
        for event in adapter.poll(now) {
            if let Err((generation, _)) = handoff.route(event, now) {
                adapter.cancel(generation);
            }
        }
    });
    Ok(Attachment {
        timer,
        owner,
        handoff,
        origin,
    })
}

#[cfg(feature = "isolated-native-projection")]
pub struct IsolatedRequest {
    directory: std::path::PathBuf,
    config: livi_native_handoff::config::Config,
}

#[cfg(feature = "isolated-native-projection")]
fn private_bytes(path: &std::path::Path, max: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o077 != 0
        || meta.len() > max
    {
        return Err(std::io::Error::other("unsafe isolated input"));
    }
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(std::io::Error::other("oversized isolated input"));
    }
    Ok(bytes)
}

#[cfg(feature = "isolated-native-projection")]
pub fn isolated_request(runtime: &std::path::Path) -> std::io::Result<Option<IsolatedRequest>> {
    let args: Vec<_> = std::env::args().collect();
    isolated_request_with_args(runtime, &args)
}

#[cfg(feature = "isolated-native-projection")]
fn isolated_request_with_args(
    runtime: &std::path::Path,
    args: &[String],
) -> std::io::Result<Option<IsolatedRequest>> {
    if !args.iter().any(|arg| arg == "--isolated-native-projection") {
        return Ok(None);
    }
    let invalid = || std::io::Error::other("isolated native authorization or runtime unavailable");
    let one = |flag: &str| -> std::io::Result<&str> {
        let positions: Vec<_> = args
            .iter()
            .enumerate()
            .filter(|(_, arg)| arg.as_str() == flag)
            .collect();
        if positions.len() != 1 {
            return Err(invalid());
        }
        args.get(positions[0].0 + 1)
            .map(String::as_str)
            .ok_or_else(invalid)
    };
    if args
        .iter()
        .filter(|arg| arg.as_str() == "--isolated-native-projection")
        .count()
        != 1
        || std::env::var(livi_core_proto::LIFELINE_ENV).ok().as_deref() != Some("1")
        || args
            .iter()
            .filter(|arg| arg.as_str() == "--authorize-native-hardware")
            .count()
            != 1
        || std::env::var_os("LIVI_UI_STATE").is_some()
    {
        return Err(invalid());
    }
    let runtime_id = one("--native-runtime-id")?
        .parse::<u64>()
        .map_err(|_| invalid())?;
    let layout = livi_video_view::isolated_runtime::Layout::from_transport(
        runtime,
        unsafe { libc::geteuid() },
        libc::O_NOFOLLOW,
        std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
        std::env::var_os("LIVI_NATIVE_BT_DEV_DIR").as_deref(),
        Some(runtime_id),
    )
    .ok_or_else(invalid)?;
    let bytes = private_bytes(std::path::Path::new(one("--native-ap-config")?), 2048)?;
    let config = livi_native_handoff::config::Config::from_json(&bytes).map_err(|_| invalid())?;
    Ok(Some(IsolatedRequest {
        directory: layout.root,
        config,
    }))
}

#[cfg(feature = "isolated-native-projection")]
pub fn attach_isolated(request: IsolatedRequest) -> Result<Attachment, Error> {
    let owner = Rc::new(RefCell::new(Adapter::new(MacBackend::new()?)));
    let handoff = Rc::new(RefCell::new(Router::isolated(request.directory)));
    owner.borrow_mut().configure(Gates {
        enabled: true,
        listener_ready: true,
        hardware_authorized: true,
    })?;
    let timer = Timer::default();
    let tick_owner = owner.clone();
    let tick_handoff = handoff.clone();
    let origin = Instant::now();
    let mut config = Some(request.config);
    timer.start(TimerMode::Repeated, Duration::from_millis(50), move || {
        let now = origin.elapsed();
        let mut adapter = tick_owner.borrow_mut();
        let mut router = tick_handoff.borrow_mut();
        for event in adapter.poll(now) {
            if matches!(&event, livi_native_bluetooth::Event::Availability(true))
                && let Some(config) = config.take()
            {
                match adapter.connect(config.phone(), now) {
                    Ok(generation) => {
                        if router.prepare(generation, config, now).is_err() {
                            adapter.cancel(generation);
                        }
                    }
                    Err(_) => {
                        router.cancel();
                    }
                }
            }
            // This path never confirms pairing automatically.
            if let livi_native_bluetooth::Event::Confirmation { generation, .. } = &event {
                adapter.cancel(*generation);
                router.cancel();
                continue;
            }
            if let Err((generation, _)) = router.route(event, now) {
                adapter.cancel(generation);
            }
        }
        if let Some(generation) = router.tick(now) {
            adapter.cancel(generation);
        }
    });
    Ok(Attachment {
        timer,
        owner,
        handoff,
        origin,
    })
}

impl Drop for Attachment {
    fn drop(&mut self) {
        self.timer.stop();
        self.handoff.borrow_mut().cancel();
        self.owner.borrow_mut().shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use livi_native_bluetooth::{Address, Backend, Event};
    use std::cell::Cell;

    #[derive(Default)]
    struct NoHardware {
        calls: Rc<Cell<usize>>,
    }

    #[cfg(feature = "isolated-native-projection")]
    #[test]
    fn isolated_ui_layout_offline() {
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let root = std::path::PathBuf::from(format!(
            "/tmp/livi-bt-dev-integration-ui-{}",
            std::process::id()
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(root.join("livi"))
            .unwrap();
        for (name, bytes) in [
            ("native-runtime", &b"731\n"[..]),
            ("offline-config", &br#"{"ssid":"offline-fixture","passphrase":"not-a-real-password","security":"wpa2","channel":36,"bssid":"02:00:00:00:00:01","ap_ip":"192.168.50.1","port":5277,"phone_address":"02:00:00:00:00:02","instance_id":"offline-instance","serial":"offline-serial"}"#[..]),
        ] {
            let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600)
                .open(root.join(name)).unwrap();
            file.write_all(bytes).unwrap();
        }
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "native_bluetooth::tests::isolated_ui_layout_child",
                "--nocapture",
            ])
            .env("LIVI_OFFLINE_LAYOUT_CHILD", "1")
            .env("XDG_RUNTIME_DIR", &root)
            .env("LIVI_NATIVE_BT_DEV_DIR", &root)
            .env(livi_core_proto::LIFELINE_ENV, "1")
            .env_remove("LIVI_UI_STATE")
            .output()
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(
            result.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }

    #[cfg(feature = "isolated-native-projection")]
    #[test]
    fn isolated_ui_shared_layout_child() {
        if std::env::var("LIVI_OFFLINE_SHARED_LAYOUT").as_deref() != Ok("1") {
            return;
        }
        let root = std::path::PathBuf::from(std::env::var_os("LIVI_NATIVE_BT_DEV_DIR").unwrap());
        let transport =
            livi_core_proto::runtime::runtime_dir(std::env::var_os("XDG_RUNTIME_DIR"), unsafe {
                libc::geteuid()
            });
        let layout = livi_video_view::isolated_runtime::Layout::from_transport(
            &transport,
            unsafe { libc::geteuid() },
            libc::O_NOFOLLOW,
            std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
            std::env::var_os("LIVI_NATIVE_BT_DEV_DIR").as_deref(),
            Some(731),
        )
        .unwrap();
        assert_eq!(layout.root, root);
        assert_eq!(layout.runtime, 731);
        // A nonexistent config is rejected only after the shared runtime was accepted.
        let args = vec![
            "offline-test".into(),
            "--isolated-native-projection".into(),
            "--authorize-native-hardware".into(),
            "--native-runtime-id".into(),
            "731".into(),
            "--native-ap-config".into(),
            root.join("missing-config").to_str().unwrap().into(),
        ];
        assert_eq!(
            isolated_request_with_args(&transport, &args)
                .err()
                .unwrap()
                .kind(),
            std::io::ErrorKind::NotFound
        );
        assert!(isolated_request_with_args(&root, &args).is_err());
    }

    #[cfg(feature = "isolated-native-projection")]
    #[test]
    fn isolated_ui_layout_child() {
        if std::env::var("LIVI_OFFLINE_LAYOUT_CHILD").as_deref() != Ok("1") {
            return;
        }
        let root = std::path::PathBuf::from(std::env::var_os("LIVI_NATIVE_BT_DEV_DIR").unwrap());
        let transport =
            livi_core_proto::runtime::runtime_dir(std::env::var_os("XDG_RUNTIME_DIR"), unsafe {
                libc::geteuid()
            });
        assert_eq!(transport, root.join("livi"));
        let mut args = vec![
            "offline-test".into(),
            "--isolated-native-projection".into(),
            "--authorize-native-hardware".into(),
            "--native-runtime-id".into(),
            "731".into(),
            "--native-ap-config".into(),
            root.join("offline-config").to_str().unwrap().into(),
        ];
        assert_eq!(
            isolated_request_with_args(&transport, &args)
                .unwrap()
                .unwrap()
                .directory,
            root
        );
        args[4] = "732".into();
        assert!(isolated_request_with_args(&transport, &args).is_err());
        args[4] = "731".into();
        assert!(isolated_request_with_args(&root, &args).is_err());
        std::fs::remove_file(root.join("native-runtime")).unwrap();
        assert!(isolated_request_with_args(&transport, &args).is_err());
        let main = include_str!("main.rs");
        assert!(
            main.find("native_bluetooth::isolated_request(&runtime)?")
                .unwrap()
                < main
                    .find("native_bluetooth::attach_isolated(request)?")
                    .unwrap()
        );
        assert!(main.contains("NSBluetoothAlwaysUsageDescription"));
    }

    impl NoHardware {
        fn called(&self) {
            self.calls.set(self.calls.get() + 1);
        }
    }

    impl Backend for NoHardware {
        fn start(&mut self) -> Result<(), Error> {
            self.called();
            Ok(())
        }
        fn stop(&mut self) {
            self.called();
        }
        fn tick(&mut self, _: Duration) {
            self.called();
        }
        fn next_event(&mut self) -> Option<Event> {
            self.called();
            None
        }
        fn discover(&mut self) -> Result<(), Error> {
            self.called();
            Ok(())
        }
        fn connect(&mut self, _: u64, _: Address) -> Result<(), Error> {
            self.called();
            Ok(())
        }
        fn confirm(&mut self, _: u64, _: bool) -> Result<(), Error> {
            self.called();
            Ok(())
        }
        fn cancel(&mut self, _: u64) {
            self.called();
        }
    }

    #[test]
    fn explicit_request_is_fresh_runtime_bound_and_does_not_arm_hardware() {
        let config = || {
            livi_native_handoff::config::Config::from_json(br#"{"ssid":"offline-fixture","passphrase":"not-a-real-password","security":"wpa2","channel":36,"bssid":"02:00:00:00:00:01","ap_ip":"192.168.50.1","port":5277,"phone_address":"02:00:00:00:00:02","instance_id":"offline-instance","serial":"offline-serial"}"#).unwrap()
        };
        assert!(AuthorizedRequest::explicit(config(), 0).is_err());
        let request = AuthorizedRequest::explicit(config(), 73).unwrap();
        let created = request.created;
        let (runtime, owned) = request
            .consume(created + Duration::from_millis(4999))
            .unwrap();
        assert_eq!(runtime, 73);
        assert_eq!(owned.phone().0, [2, 0, 0, 0, 0, 2]);
        let request = AuthorizedRequest::explicit(config(), 73).unwrap();
        let created = request.created;
        assert!(matches!(
            request.consume(created + Duration::from_secs(5)),
            Err(livi_native_handoff::Error::TimedOut)
        ));
        assert!(!production_gates(true, None).hardware_authorized);
    }

    #[test]
    fn missing_or_malformed_config_is_disabled() {
        for state in [
            serde_json::json!({}),
            serde_json::json!({"config": {"nativeWirelessAa": {"enabled": "true"}}}),
            serde_json::json!({"config": {"wirelessAaEnabled": true}}),
        ] {
            assert!(!configured(&state));
        }
    }

    #[test]
    fn stage_two_polling_cannot_start_hardware_even_when_config_enabled() {
        let backend = NoHardware::default();
        let calls = backend.calls.clone();
        let mut adapter = Adapter::new(backend);
        for enabled in [false, true, true, false] {
            for readiness in [
                None,
                Some(livi_native_handoff::ipc::Readiness {
                    runtime: 73,
                    grant: 8,
                    lifetime_ms: 5000,
                }),
            ] {
                let gates = production_gates(enabled, readiness);
                assert!(!gates.hardware_authorized);
                assert_eq!(gates.listener_ready, readiness.is_some());
                adapter.configure(gates).unwrap();
                assert!(adapter.poll(Duration::from_secs(1)).is_empty());
                assert_eq!(adapter.discover(), Err(Error::Disabled));
            }
        }
        adapter.shutdown();
        drop(adapter);
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn existing_slint_loop_remains_the_only_application_loop() {
        let main = include_str!("main.rs");
        assert_eq!(
            main.matches("slint::run_event_loop_until_quit()").count(),
            1
        );
        assert!(main.contains("native_bluetooth::attach(client.clone())"));
        assert!(main.contains("drop(native_bluetooth)"));
        let forbidden = ["[NSApp", " run]"].concat();
        assert!(!include_str!("native_bluetooth.rs").contains(&forbidden));
    }
}
