use std::ffi::{CStr, CString, c_char, c_int};
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use livi_runtime::bluetooth::arrival::Arrival;
use livi_runtime::bluetooth::policy::{Action, Backend, Observation, Reconnect};
use livi_runtime::livi_sock::Broadcaster;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::{Notify, mpsc};

unsafe extern "C" {
    fn livi_native_run(target: *const c_char, scan_seconds: u32);
    #[cfg(test)]
    fn livi_native_ownership_test() -> c_int;
}

static ARRIVALS: OnceLock<mpsc::UnboundedSender<Arrival<OwnedFd>>> = OnceLock::new();
static POLICY: Mutex<Option<Reconnect>> = Mutex::new(None);
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

// Each fd is delivered exactly once by the native owner. No borrowed fd outlives this call.
#[unsafe(no_mangle)]
extern "C" fn livi_native_arrival(fd: c_int, peer: *const c_char, local: *const c_char) {
    let stream = unsafe { OwnedFd::from_raw_fd(fd) };
    if peer.is_null() || local.is_null() { return; }
    let peer = unsafe { CStr::from_ptr(peer) }.to_string_lossy().into_owned();
    let local = unsafe { CStr::from_ptr(local) }.to_string_lossy();
    let Ok(local) = crate::config::mac(&local) else { return };
    let bytes: Vec<_> = local.split(':').filter_map(|p| u8::from_str_radix(p, 16).ok()).collect();
    let Ok(local) = bytes.try_into() else { return };
    if let Some(tx) = ARRIVALS.get() {
        let _ = tx.send(Arrival { peer, local, stream });
    }
}

#[unsafe(no_mangle)]
extern "C" fn livi_native_should_page(enabled: bool, available: bool, busy: bool) -> bool {
    if !enabled || !available || busy || ACTIVE.load(Ordering::SeqCst) != 0 { return false; }
    let mut p = POLICY.lock().unwrap_or_else(|e| e.into_inner());
    let p = p.get_or_insert_with(Reconnect::default);
    p.decide(Backend::Dongle, "selected", Observation::default(), Instant::now())
        == Action::Page { track: true }
}

#[unsafe(no_mangle)]
extern "C" fn livi_native_page_finished(success: bool) {
    POLICY.lock().unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(Reconnect::default).completed("selected", success, Instant::now());
}

struct Active;
impl Active {
    fn new() -> Self { ACTIVE.fetch_add(1, Ordering::SeqCst); Self }
}
impl Drop for Active {
    fn drop(&mut self) { ACTIVE.fetch_sub(1, Ordering::SeqCst); }
}

struct PrivateDir(PathBuf);
impl PrivateDir {
    fn create() -> Result<Self, String> {
        let path = PathBuf::from(format!("/tmp/livi-bt-dev-{}", std::process::id()));
        // Never reuse/unlink someone else's directory or sockets.
        std::fs::DirBuilder::new().mode(0o700).create(&path).map_err(|e| e.to_string())?;
        Ok(Self(path))
    }
}
use std::os::unix::fs::DirBuilderExt;
impl Drop for PrivateDir {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
}

async fn events(
    listener: UnixListener, bcast: Broadcaster, wired: crate::aa::WiredPhones,
) {
    loop {
        let Ok((socket, _)) = listener.accept().await else { break };
        let (bcast, wired) = (bcast.clone(), wired.clone());
        tokio::spawn(async move {
            let (rd, mut wr) = socket.into_split();
            let mut line = String::new();
            let read = tokio::time::timeout(
                Duration::from_secs(3),
                BufReader::new(rd.take(16385)).read_line(&mut line),
            ).await;
            if !matches!(read, Ok(Ok(_))) || line.len() > 16384 || !line.ends_with('\n') {
                return;
            }
            if line.trim() == "subscribe" {
                let mut rx = bcast.subscribe();
                let _ = wr.write_all(b"{\"ok\":true}\n").await;
                while let Some(event) = rx.recv().await {
                    if wr.write_all(format!("{event}\n").as_bytes()).await.is_err() { break; }
                }
            } else if let Some(ids) = line.trim().strip_prefix("wired-phones ") {
                if let Ok(ids) = serde_json::from_str(ids) {
                    wired.set(ids);
                    let _ = wr.write_all(b"{\"ok\":true}\n").await;
                }
            } else {
                let _ = wr.write_all(b"{\"ok\":false,\"error\":\"USB/HFP operations unavailable in isolated development receiver\"}\n").await;
            }
        });
    }
}

async fn sessions(listener: TcpListener, dir: PathBuf, events: Broadcaster) {
    let mut next = 0u64;
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            result = listener.accept() => {
                let Ok((tcp, peer)) = result else { break };
                // Do not interrupt/replace an active projection with a second peer.
                if !tasks.is_empty() { continue; }
                next += 1;
                let path = dir.join(format!("session-{next}.sock"));
                let Ok(node) = UnixListener::bind(&path) else { continue };
                let _ = tcp.set_nodelay(true);
                events.push_json(serde_json::json!({
                    "event":"aa-session", "socket":path.to_string_lossy(),
                    "peer":peer.ip().to_string(), "transport":"wifi"
                }).to_string());
                let active = Active::new();
                tasks.spawn(async move {
                    let _active = active;
                    livi_aa::session::run(tcp, livi_aa::session::Peer {
                        label: "native development phone".into(), ip: peer.ip()
                    }, node, path.to_string_lossy().into_owned(), Arc::new(Notify::new())).await;
                });
            }
            _ = tasks.join_next(), if !tasks.is_empty() => {}
        }
    }
}

async fn bootstrap(
    mut arrivals: mpsc::UnboundedReceiver<Arrival<OwnedFd>>,
    cfg: crate::config::Config, events: Broadcaster, wired: crate::aa::WiredPhones,
) {
    while let Some(arrival) = arrivals.recv().await {
        let Ok(arrival) = arrival.into_stream() else { continue };
        let told = crate::aa::AaConfig {
            ssid: cfg.ssid.clone(), passphrase: cfg.password.clone(), channel: cfg.channel,
            ap_ip: cfg.ip.to_string(), port: cfg.port,
        };
        // One selected phone / one owned stream; EOF and timeout release it.
        if let Err(e) = crate::aa::handshake(
            arrival.stream, &arrival.peer,
            (told, livi_wifi::Security::Wpa2, cfg.bssid.clone()), &events, &wired,
        ).await {
            eprintln!("[native-bt-dev] RFCOMM bootstrap ended: {e}");
        }
    }
}

struct ProjectionChildren {
    ui: std::process::Child,
    core: std::process::Child,
}

fn stop_owned(child: &mut std::process::Child) {
    child.stdin.take(); // Close only our core lifeline, if present.
    match child.try_wait() {
        Ok(None) => {}
        Ok(Some(_)) | Err(_) => return,
    }
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM); }
    let until = Instant::now() + Duration::from_secs(10);
    while Instant::now() < until {
        match child.try_wait() {
            Ok(None) => {}
            Ok(Some(_)) | Err(_) => return,
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // A hung development child must not outlive its private runtime.
    let _ = child.kill();
    let _ = child.wait();
}

impl Drop for ProjectionChildren {
    fn drop(&mut self) {
        stop_owned(&mut self.ui);
        stop_owned(&mut self.core);
    }
}

fn start_projection(dir: &Path) -> Result<ProjectionChildren, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let bin = exe.parent().ok_or("no executable directory")?
        .join("../Resources/native-dev/LIVI");
    let core = bin.with_file_name("livi-core");
    let gst = bin.with_file_name("livi-gst-host");
    if !bin.is_file() || !core.is_file() || !gst.is_file() {
        return Err("development app must contain LIVI, livi-core and livi-gst-host".into());
    }
    let command = |path: &Path| {
        let mut cmd = std::process::Command::new(path);
        cmd.env("LIVI_NATIVE_BT_DEV_DIR", dir)
            .env("XDG_RUNTIME_DIR", dir)
            .env("LIVI_CORE_SOCKET", dir.join("livi/core.sock"))
            .env("LIVI_UI_PLANES", dir.join("livi/ui-planes.sock"))
            .env("LIVI_GST_HOST_BIN", &gst)
            .env("LIVI_ROOT", Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.."))
            .env("LIVI_NO_UI", "1")
            .env("LIVI_LIFELINE", "1")
            .env_remove("LIVI_RESOURCES")
            .env_remove("LIVI_UI_STATE");
        cmd
    };
    let mut core = command(&core).stdin(std::process::Stdio::piped())
        .spawn().map_err(|e| e.to_string())?;
    match command(&bin).spawn() {
        Ok(ui) => Ok(ProjectionChildren { ui, core }),
        Err(error) => {
            stop_owned(&mut core);
            Err(error.to_string())
        }
    }
}

pub fn run() -> Result<(), String> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--help"] {
        println!("LIVI Native Bluetooth Development\n--scan-seconds N (discovery only, 1–30s)\n--config PATH [--projection]\nNo production helper, USB watcher, radio reset or AP provisioning.");
        return Ok(());
    }
    let scan = args.windows(2).find(|a| a[0] == "--scan-seconds")
        .map(|a| a[1].parse::<u32>().map_err(|_| "invalid scan duration"))
        .transpose()?.unwrap_or(0);
    if scan > 30 { return Err("scan duration must be 1–30 seconds".into()); }
    if scan > 0 {
        let empty = CString::new("").unwrap();
        unsafe { livi_native_run(empty.as_ptr(), scan) };
        return Ok(());
    }
    let file = args.windows(2).find(|a| a[0] == "--config")
        .ok_or("supply --config PATH with your actual AP settings, or --scan-seconds 12")?;
    let cfg = crate::config::Config::parse(
        &std::fs::read_to_string(&file[1]).map_err(|e| e.to_string())?
    )?;
    let target = CString::new(cfg.phone.clone()).map_err(|e| e.to_string())?;
    let dir = PrivateDir::create()?;
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    let _entered = rt.enter();
    // Bind the explicit LAN address: proves it belongs to this host. Fail, never steal a port.
    let tcp = std::net::TcpListener::bind((cfg.ip, cfg.port)).map_err(|e| format!("AP LAN listener: {e}"))?;
    tcp.set_nonblocking(true).map_err(|e| e.to_string())?;
    let tcp = TcpListener::from_std(tcp).map_err(|e| e.to_string())?;
    let listener = UnixListener::bind(dir.0.join("aa.sock")).map_err(|e| e.to_string())?;
    let events_bus = Broadcaster::default();
    let wired = crate::aa::WiredPhones::default();
    wired.set(cfg.wired_ids.clone());
    let (tx, rx) = mpsc::unbounded_channel();
    ARRIVALS.set(tx).map_err(|_| "receiver already running")?;
    rt.spawn(events(listener, events_bus.clone(), wired.clone()));
    rt.spawn(sessions(tcp, dir.0.clone(), events_bus.clone()));
    rt.spawn(bootstrap(rx, cfg, events_bus, wired));
    println!("[native-bt-dev] private runtime {}", dir.0.display());
    let ui = if args.iter().any(|a| a == "--projection") {
        Some(start_projection(&dir.0)?)
    } else { None };
    // AppKit and IOBluetooth callbacks remain on the process main thread.
    unsafe { livi_native_run(target.as_ptr(), 0) };
    drop(ui);
    drop(_entered);
    rt.shutdown_timeout(Duration::from_secs(2));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_late_callbacks_and_withdrawal_are_owned_and_idempotent() {
        assert_eq!(unsafe { livi_native_ownership_test() }, 0);
    }

    #[test]
    fn owned_development_child_is_reaped() {
        let mut child = std::process::Command::new("/bin/sleep").arg("30").spawn().unwrap();
        stop_owned(&mut child);
        assert!(child.try_wait().unwrap().is_some());
        stop_owned(&mut child);
    }

    #[test]
    fn unavailable_disabled_busy_and_projection_never_page() {
        assert!(!livi_native_should_page(false, true, false));
        assert!(!livi_native_should_page(true, false, false));
        assert!(!livi_native_should_page(true, true, true));
        let active = Active::new();
        assert!(!livi_native_should_page(true, true, false));
        drop(active);
    }
    #[tokio::test]
    async fn channel_one_config_bootstraps_existing_wpa2_ap_without_bluetooth() {
        use livi_aa::wpp;
        use tokio::net::UnixStream;

        async fn packet(stream: &mut UnixStream) -> Vec<u8> {
            tokio::time::timeout(Duration::from_secs(2), async {
                let mut header = [0; 4];
                stream.read_exact(&mut header).await.unwrap();
                let len = u16::from_be_bytes([header[0], header[1]]) as usize;
                let mut body = vec![0; len];
                stream.read_exact(&mut body).await.unwrap();
                [header.to_vec(), body].concat()
            }).await.unwrap()
        }

        // Synthetic fixtures only: no AP joins, native callbacks, TCP listener or UI.
        let cfg = crate::config::Config::parse(&serde_json::json!({
            "ssid": "test-only-2.4", "password": "not-a-real-secret",
            "security": "wpa2", "bssid": "02:AB:CD:12:34:56", "channel": 1,
            "mac_ipv4": "192.168.10.2", "port": 5277,
            "phone_address": "02:AA:BB:CC:DD:EE", "phone_not_projecting_usb": true
        }).to_string()).unwrap();
        let (stream, remote) = std::os::unix::net::UnixStream::pair().unwrap();
        remote.set_nonblocking(true).unwrap();
        let mut phone = UnixStream::from_std(remote).unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        assert!(tx.send(Arrival {
            peer: cfg.phone.clone(), local: [2, 0, 0, 0, 0, 1],
            stream: OwnedFd::from(stream),
        }).is_ok());
        drop(tx);
        let task = tokio::spawn(bootstrap(
            rx, cfg, Broadcaster::default(), crate::aa::WiredPhones::default(),
        ));

        // Version 6.0 with channel 1's 2412 MHz, independently specified bytes.
        assert_eq!(packet(&mut phone).await, [
            0x00, 0x07, 0x00, 0x04, 0x08, 0x06, 0x10, 0x00, 0x20, 0xec, 0x12,
        ]);
        phone.write_all(&[
            wpp::frame(wpp::MSG_WIFI_VERSION_RESPONSE, &[]),
            wpp::frame(wpp::MSG_WIFI_INFO_REQUEST, &[]),
            wpp::frame(wpp::MSG_WIFI_PING_REQUEST, &[8, 1]),
        ].concat()).await.unwrap();
        let start = packet(&mut phone).await;
        assert_eq!(start, wpp::wifi_start_request("192.168.10.2", 5277));
        assert!(start.windows(12).any(|w| w == b"192.168.10.2"));
        let info = packet(&mut phone).await;
        assert_eq!(info, wpp::wifi_info_response(
            "test-only-2.4", "not-a-real-secret", "02:ab:cd:12:34:56",
            livi_wifi::Security::Wpa2,
        ));
        assert!(info.ends_with(&[0x20, 8, 0x28, 0]), "WPA2-Personal / static AP");
        assert_eq!(packet(&mut phone).await, wpp::pong(&[8, 1]));
        drop(phone);
        tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap();
    }
    #[tokio::test]
    async fn private_event_socket_rejects_usb_and_hfp_commands() {
        let dir = PrivateDir::create().unwrap();
        let path = dir.0.join("events.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let task = tokio::spawn(events(listener, Broadcaster::default(), crate::aa::WiredPhones::default()));
        for line in ["restart-usb", "sco-sink /tmp/no 1", "disconnect unrelated"] {
            let mut stream = tokio::net::UnixStream::connect(&path).await.unwrap();
            stream.write_all(format!("{line}\n").as_bytes()).await.unwrap();
            let mut reply = String::new();
            BufReader::new(stream).read_line(&mut reply).await.unwrap();
            assert_eq!(serde_json::from_str::<serde_json::Value>(&reply).unwrap()["ok"], false);
        }
        for bytes in [b"subscribe".to_vec(), vec![b'x'; 16385]] {
            let mut stream = tokio::net::UnixStream::connect(&path).await.unwrap();
            stream.write_all(&bytes).await.unwrap();
            stream.shutdown().await.unwrap();
            let mut reply = Vec::new();
            tokio::time::timeout(Duration::from_secs(4), stream.read_to_end(&mut reply))
                .await.unwrap().unwrap();
            assert!(reply.is_empty(), "malformed requests must not subscribe");
        }
        task.abort();
        let _ = task.await;
    }
}
