// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

use core::ffi::{c_char, c_int, c_void};
use std::cell::RefCell;
use std::ffi::CString;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Arc, Mutex};

use glib::IOCondition;

use crate::gst::Gst;
use crate::{Host, Wire};

const STATS_SECONDS: u32 = 5;
const VISUALIZER_INTERVAL_MS: u64 = 20;
const URL_STATUS_INTERVAL_MS: u64 = 500;
const CHUNK: usize = 65536;

struct SocketWire {
    sock: Arc<UnixStream>,
    write: Mutex<()>,
    native: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
}

impl Wire for SocketWire {
    fn reply(&self, op: u8, id: u32, rest: &[u8]) {
        if let Some(out) = &self.native {
            if rest.len() > 64 * 1024
                || out.try_send(livi_host_proto::encode_reply(op, id, rest)).is_err()
            {
                let _ = self.sock.shutdown(std::net::Shutdown::Both);
            }
            return;
        }
        let _guard = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let mut sock: &UnixStream = &self.sock;
        let _ = sock.write_all(&livi_host_proto::encode_reply(op, id, rest));
    }
}

pub fn run(sock_path: &str, crash_log: &str) {
    let isolated = std::env::var_os("LIVI_ISOLATED_NATIVE_MEDIA").as_deref()
        == Some(std::ffi::OsStr::new("1"));
    if isolated && !private_runtime(sock_path) { return; }
    let shared = cfg!(target_os = "macos")
        && std::env::var_os("LIVI_SHARED_NATIVE_MEDIA").as_deref()
            == Some(std::ffi::OsStr::new("1"))
        && std::path::Path::new(sock_path).file_name()
            == Some(std::ffi::OsStr::new("gst.native.sock"));
    let native_mode = isolated || shared;
    glib::set_prgname(Some("livi-video"));
    livi_video_player::ensure_init();
    arm_crash_handler(crash_log);

    let Ok(sock) = UnixStream::connect(sock_path) else {
        eprintln!("[gst-host] connect to {sock_path} failed");
        std::process::exit(1);
    };
    let sock = Arc::new(sock);
    let native = if native_mode {
        let (out, queued) = std::sync::mpsc::sync_channel::<Vec<u8>>(16);
        let mut writer = sock.try_clone().expect("owned host socket clone");
        let _ = writer.set_write_timeout(Some(std::time::Duration::from_secs(2)));
        if std::thread::Builder::new().name("native-feedback-write".into()).spawn(move || {
            for bytes in queued {
                if writer.write_all(&bytes).is_err() {
                    let _ = writer.shutdown(std::net::Shutdown::Both);
                    return;
                }
            }
        }).is_err() { return; }
        Some(out)
    } else { None };
    let wire = Arc::new(SocketWire {
        sock: sock.clone(),
        write: Mutex::new(()),
        native,
    });
    let host = Rc::new(RefCell::new(if native_mode {
        Host::new_native(Gst, wire)
    } else {
        Host::new(Gst, wire)
    }));

    let fd = sock.as_raw_fd();
    let reading = host.clone();
    glib_unix::unix_fd_add_local(
        fd,
        IOCondition::IN | IOCondition::HUP | IOCondition::ERR,
        move |_, cond| {
            if cond.contains(IOCondition::HUP) || cond.contains(IOCondition::ERR) {
                std::process::exit(0);
            }
            let mut chunk = [0u8; CHUNK];
            let mut from: &UnixStream = &sock;
            let read = match from.read(&mut chunk) {
                Ok(0) | Err(_) => std::process::exit(0),
                Ok(n) => n,
            };
            reading.borrow_mut().feed(&chunk[..read]);
            glib::ControlFlow::Continue
        },
    );

    let visualizer_host = host.clone();
    glib::timeout_add_local(
        std::time::Duration::from_millis(VISUALIZER_INTERVAL_MS),
        move || {
            visualizer_host.borrow_mut().pump_visualizer();
            visualizer_host.borrow().pump_native_feedback();
            glib::ControlFlow::Continue
        },
    );

    let status_host = host.clone();
    glib::timeout_add_local(
        std::time::Duration::from_millis(URL_STATUS_INTERVAL_MS),
        move || {
            status_host.borrow().pump_url_status();
            status_host.borrow().pump_audio_delays();
            glib::ControlFlow::Continue
        },
    );

    glib::timeout_add_seconds_local(STATS_SECONDS, move || {
        for line in host.borrow().take_stats() {
            eprintln!("{line}");
        }
        glib::ControlFlow::Continue
    });

    glib::MainLoop::new(None, false).run();
}

static CRASH_PATH: AtomicPtr<c_char> = AtomicPtr::new(core::ptr::null_mut());

fn private_runtime(socket: &str) -> bool {
    let socket = std::path::Path::new(socket);
    if socket.file_name() != Some(std::ffi::OsStr::new("gst.sock")) { return false; }
    let Some(transport) = socket.parent() else { return false };
    livi_host_proto::isolated_runtime::Layout::from_transport(
        transport, unsafe { libc::geteuid() }, libc::O_NOFOLLOW,
        std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
        std::env::var_os("LIVI_NATIVE_BT_DEV_DIR").as_deref(), None,
    ).is_some()
}

const SIG_DFL: usize = 0;

#[cfg(all(test, target_os = "macos"))]
mod isolated_layout_tests {
    use super::*;

    #[test]
    fn media_guard_uses_standard_core_layout_offline() {
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let root = std::path::PathBuf::from(format!(
            "/tmp/livi-bt-dev-integration-gst-{}", std::process::id()));
        std::fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        std::fs::DirBuilder::new().mode(0o700).create(root.join("livi")).unwrap();
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600)
            .open(root.join("native-runtime")).unwrap();
        writeln!(file, "731").unwrap();
        drop(file);
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "process::isolated_layout_tests::media_guard_layout_child", "--nocapture"])
            .env("LIVI_OFFLINE_LAYOUT_CHILD", "1").env("XDG_RUNTIME_DIR", &root)
            .env("LIVI_NATIVE_BT_DEV_DIR", &root).output().unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stdout));
    }

    #[test]
    fn media_guard_layout_child() {
        if std::env::var("LIVI_OFFLINE_LAYOUT_CHILD").as_deref() != Ok("1") { return; }
        let root = std::path::PathBuf::from(std::env::var_os("LIVI_NATIVE_BT_DEV_DIR").unwrap());
        assert!(private_runtime(root.join("livi/gst.sock").to_str().unwrap()));
        assert!(!private_runtime(root.join("gst.sock").to_str().unwrap()));
        assert!(!private_runtime(root.join("livi/other.sock").to_str().unwrap()));
        if std::env::var("LIVI_OFFLINE_SHARED_LAYOUT").as_deref() != Ok("1") {
            std::fs::remove_file(root.join("native-runtime")).unwrap();
            assert!(!private_runtime(root.join("livi/gst.sock").to_str().unwrap()));
        }
    }
}

unsafe extern "C" {
    fn backtrace(buffer: *mut *mut c_void, size: c_int) -> c_int;
    fn backtrace_symbols_fd(buffer: *const *mut c_void, size: c_int, fd: c_int);
    fn signal(sig: c_int, handler: usize) -> usize;
}

extern "C" fn on_crash(sig: c_int) {
    const HEADER: &[u8] = b"\n=== gst-host CRASH backtrace ===\n";
    let mut frames = [core::ptr::null_mut::<c_void>(); 64];
    let depth = unsafe { backtrace(frames.as_mut_ptr(), frames.len() as c_int) };

    let dump = |fd: c_int| unsafe {
        let _ = libc::write(fd, HEADER.as_ptr().cast(), HEADER.len());
        backtrace_symbols_fd(frames.as_ptr(), depth, fd);
    };
    dump(libc::STDERR_FILENO);

    let path = CRASH_PATH.load(Ordering::Relaxed);
    if !path.is_null() {
        let fd = unsafe {
            libc::open(
                path,
                libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
                0o644 as c_int,
            )
        };
        if fd >= 0 {
            dump(fd);
            unsafe { libc::close(fd) };
        }
    }

    unsafe {
        signal(sig, SIG_DFL);
        libc::raise(sig);
    }
}

fn arm_crash_handler(crash_log: &str) {
    if !crash_log.is_empty()
        && let Ok(path) = CString::new(crash_log)
    {
        CRASH_PATH.store(path.into_raw(), Ordering::Relaxed);
    }
    unsafe {
        signal(libc::SIGSEGV, on_crash as *const () as usize);
        signal(libc::SIGABRT, on_crash as *const () as usize);
    }
}
