// SPDX-License-Identifier: GPL-3.0-or-later
//! Default-off isolated renderer. Only bounded Send messages cross threads.
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::Duration;

use livi_host_proto::native::{Capabilities, Create, Failure, Feedback, Owner, Packet, REQUIRED_CAPABILITIES, frame};
use livi_host_proto::ui_planes as op;

use crate::control::Region;
use crate::experimental_metal::Prototype;

const LIMIT: usize = 3;
static OUT: OnceLock<mpsc::SyncSender<Command>> = OnceLock::new();
static ENABLED: AtomicBool = AtomicBool::new(false);
static CONNECTION: AtomicU64 = AtomicU64::new(1);
static CONNECTIONS: AtomicUsize = AtomicUsize::new(0);

struct ConnectionGuard;
impl Drop for ConnectionGuard {
    fn drop(&mut self) { CONNECTIONS.fetch_sub(1, Ordering::AcqRel); }
}

struct Command {
    connection: u64,
    id: u32,
    packet: Packet,
    live: Arc<AtomicBool>,
    feedback: mpsc::SyncSender<Vec<u8>>,
}
struct Entry {
    id: u32,
    owner: Owner,
    player: Prototype,
    active: bool,
    visibility: u64,
    decoded: u64,
    live: Arc<AtomicBool>,
    feedback: mpsc::SyncSender<Vec<u8>>,
}
#[derive(Default)]
struct Registry {
    rx: Option<mpsc::Receiver<Command>>,
    window: usize,
    region: Region,
    shown: bool,
    entries: HashMap<u64, Entry>,
}
thread_local! { static REGISTRY: RefCell<Registry> = RefCell::new(Registry::default()); }

fn on_main() -> bool {
    unsafe extern "C" { fn pthread_main_np() -> i32; }
    unsafe { pthread_main_np() == 1 }
}

pub fn initialize() -> bool {
    if !on_main() || OUT.get().is_some() { return false; }
    let (tx, rx) = mpsc::sync_channel(LIMIT);
    if OUT.set(tx).is_err() { return false; }
    REGISTRY.with_borrow_mut(|r| { r.rx = Some(rx); r.shown = true; });
    ENABLED.store(true, Ordering::Release);
    true
}
pub fn enabled() -> bool { ENABLED.load(Ordering::Acquire) }
pub fn stop() {
    if !on_main() { return; }
    ENABLED.store(false, Ordering::Release);
    REGISTRY.with_borrow_mut(|r| {
        for entry in r.entries.values() { entry.live.store(false, Ordering::Release); }
        r.entries.clear();
        if let Some(rx) = &r.rx { while rx.try_recv().is_ok() {} }
    });
}
fn send(entry: &Entry, feedback: Feedback) {
    if let Ok(bytes) = frame(op::NATIVE_FEEDBACK, entry.id, &Packet::Feedback(feedback))
        && entry.feedback.try_send(bytes).is_ok() {
        return;
    }
    entry.live.store(false, Ordering::Release);
}
fn fail(entry: &Entry, code: Failure) {
    send(entry, Feedback::Failed { owner: entry.owner, code });
    entry.live.store(false, Ordering::Release);
}
fn make(r: &Registry, create: &Create) -> Option<Prototype> {
    if r.window == 0 { return None; }
    // The Slint render notifier owns this live view; no raw view crosses threads.
    let player = unsafe { Prototype::new(r.window, create.owner.session, &create.codec, &create.codec_data) }?;
    player.set_visible(false);
    player.set_region(r.region);
    player.start().then_some(player)
}
pub fn tick() {
    if !on_main() || !enabled() { return; }
    REGISTRY.with_borrow_mut(|r| {
        for _ in 0..LIMIT {
            let Some(cmd) = r.rx.as_ref().and_then(|rx| rx.try_recv().ok()) else { break };
            if !cmd.live.load(Ordering::Acquire) { continue; }
            if let Packet::Create(create) = &cmd.packet {
                if r.entries.len() >= LIMIT || r.entries.contains_key(&cmd.connection) {
                    cmd.live.store(false, Ordering::Release);
                    continue;
                }
                match make(r, create) {
                    Some(player) => {
                        r.entries.insert(cmd.connection, Entry { id: cmd.id, owner: create.owner,
                            player, active: false, visibility: 0, decoded: 0,
                            live: cmd.live, feedback: cmd.feedback });
                    }
                    None => {
                        let packet = Packet::Feedback(Feedback::Failed { owner: create.owner, code: Failure::Unsupported });
                        if let Ok(bytes) = frame(op::NATIVE_FEEDBACK, cmd.id, &packet) { let _ = cmd.feedback.try_send(bytes); }
                        cmd.live.store(false, Ordering::Release);
                    }
                }
                continue;
            }
            let Some(entry) = r.entries.get_mut(&cmd.connection)
                .filter(|e| e.id == cmd.id && Some(e.owner) == cmd.packet.owner()) else {
                    cmd.live.store(false, Ordering::Release); continue;
                };
            match cmd.packet {
                Packet::Data { bytes, .. } => {
                    if entry.active && !entry.player.push(&bytes) { fail(entry, Failure::Backpressure); }
                }
                Packet::Flush(_) => {
                    entry.player.set_visible(false);
                    if !entry.player.flush() { fail(entry, Failure::Decode); }
                    entry.player.set_visible(entry.active && r.shown);
                }
                Packet::Active { active, .. } => {
                    entry.active = active;
                    entry.player.set_visible(active && r.shown);
                }
                Packet::Stop(_) => entry.live.store(false, Ordering::Release),
                _ => fail(entry, Failure::Unsupported),
            }
        }
        for entry in r.entries.values_mut() {
            if !entry.live.load(Ordering::Acquire) { continue; }
            if entry.player.failed() { fail(entry, Failure::Decode); continue; }
            let snapshot = entry.player.snapshot();
            if snapshot.visibility_epoch != entry.visibility {
                entry.visibility = snapshot.visibility_epoch;
                send(entry, Feedback::Invalidated { owner: entry.owner, visibility: entry.visibility });
            }
            for (frame, ready_time) in entry.player.take_decoded() {
                if frame > entry.decoded {
                    entry.decoded = frame;
                    send(entry, Feedback::Decoded { owner: entry.owner, frame, ready_time });
                }
            }
            for presentation in entry.player.take_presentations() {
                if !entry.active || !r.shown || presentation.identity.session_epoch != entry.owner.session { continue; }
                send(entry, Feedback::Presented { owner: entry.owner,
                    frame: presentation.identity.frame_id,
                    visibility: presentation.identity.visibility_epoch,
                    time: presentation.presented_time });
            }
        }
        r.entries.retain(|_, entry| entry.live.load(Ordering::Acquire));
    });
}
pub fn window(screen: &str, window: usize) {
    if screen != "main" || !on_main() { return; }
    REGISTRY.with_borrow_mut(|r| {
        if r.window != window {
            for entry in r.entries.values() { fail(entry, Failure::Disconnected); }
            r.entries.clear();
            r.window = window;
        }
    });
}
pub fn place(tag: &str, region: Region) {
    if tag != livi_host_proto::MAIN_TAG || !on_main() { return; }
    REGISTRY.with_borrow_mut(|r| {
        r.region = region;
        for entry in r.entries.values() { entry.player.set_region(region); }
    });
}
pub fn show(tag: &str, shown: bool) {
    if tag != livi_host_proto::MAIN_TAG || !on_main() { return; }
    REGISTRY.with_borrow_mut(|r| {
        r.shown = shown;
        for entry in r.entries.values() { entry.player.set_visible(shown && entry.active); }
    });
}

pub fn serve(mut socket: UnixStream) {
    let Some(out) = OUT.get() else { return };
    if !enabled() { return; }
    if CONNECTIONS.try_update(Ordering::AcqRel, Ordering::Acquire,
        |count| count.checked_add(1).filter(|next| *next <= LIMIT)).is_err() { return; }
    let _guard = ConnectionGuard;
    let Ok(connection) = CONNECTION.try_update(Ordering::Relaxed, Ordering::Relaxed,
        |id| id.checked_add(1)) else { return };
    let Ok(mut writer) = socket.try_clone() else { return };
    let _ = writer.set_write_timeout(Some(Duration::from_secs(2)));
    let _ = socket.set_read_timeout(Some(Duration::from_millis(100)));
    let (back, rx) = mpsc::sync_channel::<Vec<u8>>(LIMIT);
    let live = Arc::new(AtomicBool::new(true));
    let alive = live.clone();
    let Ok(worker) = std::thread::Builder::new().name("native-view-feedback".into()).spawn(move || {
        while alive.load(Ordering::Acquire) {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(bytes) if writer.write_all(&bytes).is_err() => break,
                Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => break,
            }
        }
        alive.store(false, Ordering::Release);
        let _ = writer.shutdown(Shutdown::Both);
    }) else { return };
    let mut framer = livi_host_proto::native::Framer::new();
    let mut chunk = [0; 65536];
    let mut negotiated = false;
    let mut owner = None;
    let mut plane = None;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    'read: while enabled() && live.load(Ordering::Acquire) {
        if owner.is_none() && std::time::Instant::now() >= deadline { break; }
        let n = match socket.read(&mut chunk) {
            Ok(0) => break, Ok(n) => n,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
            Err(_) => break,
        };
        if framer.push(&chunk[..n]).is_err() { break; }
        while let Some(message) = framer.next_message() {
            let Some(packet) = op::parse_native(message.op, false, &message.rest) else { break 'read };
            if matches!(packet, Packet::Capabilities(_)) {
                if negotiated { break 'read; }
                negotiated = true;
                let Ok(bytes) = frame(op::NATIVE_CAPABILITIES_REPLY, message.id,
                    &Packet::Capabilities(Capabilities { bits: REQUIRED_CAPABILITIES })) else { break 'read };
                if back.try_send(bytes).is_err() { break 'read; }
                continue;
            }
            if !negotiated { break 'read; }
            if matches!(packet, Packet::Create(_)) && owner.is_none() {
                owner = packet.owner(); plane = Some(message.id);
            } else if owner.is_none() || packet.owner() != owner || plane != Some(message.id)
                || matches!(packet, Packet::Create(_)) { break 'read; }
            let cmd = Command { connection, id: message.id, packet, live: live.clone(), feedback: back.clone() };
            if out.try_send(cmd).is_err() { break 'read; }
        }
    }
    live.store(false, Ordering::Release);
    let _ = socket.shutdown(Shutdown::Both);
    let _ = worker.join();
}
