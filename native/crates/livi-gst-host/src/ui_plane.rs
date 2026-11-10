// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};

use livi_host_proto::{
    CameraOrder, Framer, UrlStatus, camera_body, encode_reply, parse_url_status, plane_body,
    ui_planes, url_body,
};

use crate::{Plane, UrlPlay};

const QUEUE: usize = 1024;

fn connect(path: &str, id: u32, first: Vec<u8>) -> Option<(SyncSender<Vec<u8>>, UnixStream)> {
    let mut sock = UnixStream::connect(path)
        .inspect_err(|e| eprintln!("[gst-host] no UI for plane 0x{id:x} at {path}: {e}"))
        .ok()?;
    let back = sock.try_clone().ok()?;
    let (out, queued) = sync_channel::<Vec<u8>>(QUEUE);
    out.try_send(first).ok()?;
    std::thread::Builder::new()
        .name(format!("ui-plane-{id:x}"))
        .spawn(move || {
            for msg in queued {
                if sock.write_all(&msg).is_err() {
                    return;
                }
            }
        })
        .ok()?;
    Some((out, back))
}

pub struct UiPlane {
    id: u32,
    out: SyncSender<Vec<u8>>,
    lost: AtomicBool,
    native: Option<NativeUi>,
}

struct NativeUi {
    owner: livi_host_proto::native::Owner,
    sock: UnixStream,
    feedback: Arc<Mutex<std::collections::VecDeque<livi_host_proto::native::Feedback>>>,
    bytes: Arc<std::sync::atomic::AtomicUsize>,
}

fn native_failure(
    queue: &Mutex<std::collections::VecDeque<livi_host_proto::native::Feedback>>,
    owner: livi_host_proto::native::Owner,
    code: livi_host_proto::native::Failure,
) {
    let mut queue = queue.lock().unwrap_or_else(|e| e.into_inner());
    queue.clear();
    queue.push_back(livi_host_proto::native::Feedback::Failed { owner, code });
}

impl UiPlane {
    pub fn open(path: &str, id: u32, codec: &str, codec_data: &[u8]) -> Option<Self> {
        let create = encode_reply(ui_planes::CREATE, id, &plane_body(codec, codec_data));
        let (out, _) = connect(path, id, create)?;
        Some(Self {
            id,
            out,
            lost: AtomicBool::new(false),
            native: None,
        })
    }

    pub fn open_native(path: &str, id: u32, create: &livi_host_proto::native::Create) -> Option<Self> {
        use livi_host_proto::native::{Capabilities, Failure, Packet, REQUIRED_CAPABILITIES, frame};
        let mut sock = UnixStream::connect(path).ok()?;
        sock.set_read_timeout(Some(std::time::Duration::from_secs(2))).ok()?;
        sock.set_write_timeout(Some(std::time::Duration::from_secs(2))).ok()?;
        sock.write_all(&frame(ui_planes::NATIVE_CAPABILITIES, id,
            &Packet::Capabilities(Capabilities { bits: 0 })).ok()?).ok()?;
        let mut framer = livi_host_proto::native::Framer::new();
        let mut chunk = [0; 512];
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let remaining = deadline.checked_duration_since(std::time::Instant::now())?;
            if remaining.is_zero() { return None; }
            sock.set_read_timeout(Some(remaining)).ok()?;
            let count = sock.read(&mut chunk).ok()?;
            if count == 0 { return None; }
            framer.push(&chunk[..count]).ok()?;
            if let Some(message) = framer.next_message() {
                let Packet::Capabilities(capabilities) =
                    ui_planes::parse_native(message.op, true, &message.rest)? else { return None };
                if message.id != id || capabilities.bits != REQUIRED_CAPABILITIES
                    || framer.next_message().is_some() { return None; }
                break;
            }
        }
        sock.write_all(&frame(ui_planes::NATIVE_CREATE, id, &Packet::Create(create.clone())).ok()?).ok()?;
        sock.set_read_timeout(None).ok()?;
        let mut reader = sock.try_clone().ok()?;
        let mut writer = sock.try_clone().ok()?;
        let feedback = Arc::new(Mutex::new(std::collections::VecDeque::new()));
        let bytes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (out, queued) = sync_channel::<Vec<u8>>(3);
        let (heard, owner, budget) = (feedback.clone(), create.owner, bytes.clone());
        std::thread::Builder::new().name("native-plane-write".into()).spawn(move || {
            for message in queued {
                let result = writer.write_all(&message);
                budget.fetch_sub(message.len(), Ordering::Relaxed);
                if result.is_err() {
                    native_failure(&heard, owner, Failure::Disconnected);
                    let _ = writer.shutdown(Shutdown::Both);
                    return;
                }
            }
        }).ok()?;
        let heard = feedback.clone();
        std::thread::Builder::new().name("native-plane-feedback".into()).spawn(move || {
            let mut framer = livi_host_proto::native::Framer::new();
            let mut chunk = [0; 1024];
            'read: while let Ok(count @ 1..) = reader.read(&mut chunk) {
                if framer.push(&chunk[..count]).is_err() { break; }
                while let Some(message) = framer.next_message() {
                    let Some(Packet::Feedback(value)) =
                        ui_planes::parse_native(message.op, true, &message.rest) else { break 'read };
                    if message.id != id || Packet::Feedback(value.clone()).owner() != Some(owner) {
                        break 'read;
                    }
                    let mut queue = heard.lock().unwrap_or_else(|e| e.into_inner());
                    if queue.len() >= 3 {
                        drop(queue);
                        native_failure(&heard, owner, Failure::Backpressure);
                        let _ = reader.shutdown(Shutdown::Both);
                        return;
                    }
                    queue.push_back(value);
                }
            }
            native_failure(&heard, owner, Failure::Disconnected);
            let _ = reader.shutdown(Shutdown::Both);
        }).ok()?;
        Some(Self { id, out, lost: AtomicBool::new(false),
            native: Some(NativeUi { owner: create.owner, sock, feedback, bytes }) })
    }

    fn send_native(&self, op: u8, packet: livi_host_proto::native::Packet) {
        use livi_host_proto::native::{Failure, frame};
        let Some(native) = &self.native else { return };
        if self.lost.load(Ordering::Relaxed) { return; }
        let bytes = frame(op, self.id, &packet);
        if let Ok(bytes) = bytes {
            let len = bytes.len();
            let reserved = native.bytes.try_update(Ordering::Relaxed, Ordering::Relaxed,
                |used| used.checked_add(len).filter(|next| *next <= 8 * 1024 * 1024)).is_ok();
            if reserved && self.out.try_send(bytes).is_ok() { return; }
            if reserved { native.bytes.fetch_sub(len, Ordering::Relaxed); }
        }
        self.lost.store(true, Ordering::Relaxed);
        native_failure(&native.feedback, native.owner, Failure::Backpressure);
        let _ = native.sock.shutdown(Shutdown::Both);
    }

    fn send(&self, op: u8, rest: &[u8]) {
        if let Err(TrySendError::Full(_)) = self.out.try_send(encode_reply(op, self.id, rest))
            && !self.lost.swap(true, Ordering::Relaxed)
        {
            eprintln!(
                "[gst-host] the UI stopped taking plane 0x{:x}, frames are lost",
                self.id
            );
        }
    }
}

impl Plane for UiPlane {
    fn start(&self) {}

    fn push(&self, nal: &[u8]) {
        if let Some(native) = &self.native {
            self.send_native(ui_planes::NATIVE_FRAME, livi_host_proto::native::Packet::Data {
                owner: native.owner, bytes: nal.to_vec(),
            });
            return;
        }
        self.send(ui_planes::FRAME, nal);
    }

    fn flush(&self) {
        if let Some(native) = &self.native {
            self.send_native(ui_planes::NATIVE_FLUSH, livi_host_proto::native::Packet::Flush(native.owner));
            return;
        }
        self.send(ui_planes::FLUSH, &[]);
    }

    fn set_gamma(&self, _gamma: f64, _contrast: f64, _r: f64, _g: f64, _b: f64) {}

    fn native_active(&self, active: bool) {
        if let Some(native) = &self.native {
            self.send_native(ui_planes::NATIVE_ACTIVE,
                livi_host_proto::native::Packet::Active { owner: native.owner, active });
        }
    }

    fn native_feedback(&self) -> Vec<livi_host_proto::native::Feedback> {
        self.native.as_ref().map(|native| native.feedback.lock()
            .unwrap_or_else(|e| e.into_inner()).drain(..).collect()).unwrap_or_default()
    }
}

impl Drop for UiPlane {
    fn drop(&mut self) {
        if let Some(native) = &self.native { let _ = native.sock.shutdown(Shutdown::Both); }
    }
}

pub struct UiUrlPlayer {
    id: u32,
    out: SyncSender<Vec<u8>>,
    sock: UnixStream,
    status: Arc<Mutex<UrlStatus>>,
}

impl UiUrlPlayer {
    pub fn open(path: &str, id: u32, url: &str, audio_device: &str) -> Option<Self> {
        let play = encode_reply(ui_planes::PLAY_URL, id, &url_body(url, audio_device));
        let (out, sock) = connect(path, id, play)?;
        let status = Arc::new(Mutex::new(UrlStatus::default()));
        let (mut from_ui, heard) = (sock.try_clone().ok()?, status.clone());
        std::thread::Builder::new()
            .name(format!("ui-url-{id:x}"))
            .spawn(move || {
                let mut framer = Framer::new();
                let mut chunk = [0u8; 256];
                while let Ok(n @ 1..) = from_ui.read(&mut chunk) {
                    framer.push(&chunk[..n]);
                    while let Some(m) = framer.next_message() {
                        match m.op {
                            ui_planes::STATUS => {
                                if let Some(s) = parse_url_status(&m.rest) {
                                    *heard.lock().unwrap_or_else(|e| e.into_inner()) = s;
                                }
                            }
                            ui_planes::LOG => {
                                let line = String::from_utf8_lossy(&m.rest);
                                eprintln!("{}", livi_video_player::stamped(&line));
                            }
                            _ => {}
                        }
                    }
                }
                heard.lock().unwrap_or_else(|e| e.into_inner()).failed = true;
            })
            .ok()?;
        Some(Self {
            id,
            out,
            sock,
            status,
        })
    }

    fn send(&self, op: u8, rest: &[u8]) {
        let _ = self.out.try_send(encode_reply(op, self.id, rest));
    }
}

impl UrlPlay for UiUrlPlayer {
    fn start(&self) {}

    fn set_rate(&self, rate: f64) {
        self.send(ui_planes::PLAY_RATE, &rate.to_le_bytes());
    }

    fn seek(&self, seconds: f64) {
        self.send(ui_planes::PLAY_SEEK, &seconds.to_le_bytes());
    }

    fn set_muted(&self, muted: bool) {
        self.send(ui_planes::PLAY_MUTE, &[u8::from(muted)]);
    }

    fn status(&self) -> UrlStatus {
        *self.status.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for UiUrlPlayer {
    fn drop(&mut self) {
        let _ = self.sock.shutdown(Shutdown::Both);
    }
}

pub struct UiCamera {
    _out: SyncSender<Vec<u8>>,
    sock: UnixStream,
}

impl UiCamera {
    pub fn open(path: &str, id: u32, order: &CameraOrder) -> Option<Self> {
        let open = encode_reply(ui_planes::CAMERA, id, &camera_body(order));
        let (out, sock) = connect(path, id, open)?;
        let mut from_ui = sock.try_clone().ok()?;
        std::thread::Builder::new()
            .name(format!("ui-camera-{id:x}"))
            .spawn(move || {
                let mut framer = Framer::new();
                let mut chunk = [0u8; 256];
                while let Ok(n @ 1..) = from_ui.read(&mut chunk) {
                    framer.push(&chunk[..n]);
                    while let Some(m) = framer.next_message() {
                        if m.op == ui_planes::LOG {
                            let line = String::from_utf8_lossy(&m.rest);
                            eprintln!("{}", livi_video_player::stamped(&line));
                        }
                    }
                }
            })
            .ok()?;
        Some(Self { _out: out, sock })
    }
}

impl Drop for UiCamera {
    fn drop(&mut self) {
        let _ = self.sock.shutdown(Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::os::unix::net::UnixListener;
    use std::time::Duration;

    use livi_host_proto::{Framer, Message};

    use super::*;

    fn socket_path(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("ui-plane-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn read_all(mut conn: UnixStream) -> Vec<Message> {
        let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
        let mut bytes = Vec::new();
        conn.read_to_end(&mut bytes).unwrap();
        let mut framer = Framer::new();
        framer.push(&bytes);
        std::iter::from_fn(|| framer.next_message()).collect()
    }

    #[test]
    fn the_ui_gets_the_create_then_the_frames_until_the_plane_goes() {
        let path = socket_path("frames");
        let listener = UnixListener::bind(&path).unwrap();
        let plane = UiPlane::open(path.to_str().unwrap(), 0x7a00_0001, "h264", &[1, 2]).unwrap();
        plane.push(&[5, 5]);
        plane.flush();
        drop(plane);

        let (conn, _) = listener.accept().unwrap();
        let got = read_all(conn);
        let _ = std::fs::remove_file(&path);
        let ops: Vec<(u8, u32)> = got.iter().map(|m| (m.op, m.id)).collect();
        assert_eq!(
            ops,
            [
                (ui_planes::CREATE, 0x7a00_0001),
                (ui_planes::FRAME, 0x7a00_0001),
                (ui_planes::FLUSH, 0x7a00_0001)
            ]
        );
        assert_eq!(got[0].rest, plane_body("h264", &[1, 2]));
        assert_eq!(got[1].rest, [5, 5]);
    }

    #[test]
    fn without_a_ui_there_is_no_plane() {
        let path = socket_path("nobody");
        assert!(UiPlane::open(path.to_str().unwrap(), 1, "h264", &[]).is_none());
        assert!(UiUrlPlayer::open(path.to_str().unwrap(), 1, "http://h/", "").is_none());
    }

    fn next(conn: &mut UnixStream, framer: &mut Framer) -> Message {
        let mut chunk = [0u8; 256];
        loop {
            if let Some(m) = framer.next_message() {
                return m;
            }
            let n = conn.read(&mut chunk).unwrap();
            assert!(n > 0, "the player hung up early");
            framer.push(&chunk[..n]);
        }
    }

    fn wait_for(player: &UiUrlPlayer, what: impl Fn(&UrlStatus) -> bool) {
        let start = std::time::Instant::now();
        while !what(&player.status()) {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "status never came"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn the_ui_plays_the_url_takes_the_controls_and_reports_back() {
        let path = socket_path("url");
        let listener = UnixListener::bind(&path).unwrap();
        let player =
            UiUrlPlayer::open(path.to_str().unwrap(), 9, "http://h/a.m3u8", "car").unwrap();
        let (mut ui, _) = listener.accept().unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = ui.set_read_timeout(Some(Duration::from_secs(5)));
        let mut framer = Framer::new();

        let play = next(&mut ui, &mut framer);
        assert_eq!((play.op, play.id), (ui_planes::PLAY_URL, 9));
        assert_eq!(play.rest, url_body("http://h/a.m3u8", "car"));

        player.start();
        player.set_rate(0.0);
        player.seek(12.5);
        player.set_muted(true);
        let got: Vec<(u8, Vec<u8>)> = (0..3)
            .map(|_| next(&mut ui, &mut framer))
            .map(|m| (m.op, m.rest))
            .collect();
        assert_eq!(
            got,
            [
                (ui_planes::PLAY_RATE, 0.0f64.to_le_bytes().to_vec()),
                (ui_planes::PLAY_SEEK, 12.5f64.to_le_bytes().to_vec()),
                (ui_planes::PLAY_MUTE, vec![1]),
            ]
        );

        let playing = UrlStatus {
            position: Some(3.0),
            ready: true,
            playing: true,
            ..UrlStatus::default()
        };
        let status = encode_reply(
            ui_planes::STATUS,
            9,
            &livi_host_proto::url_status_body(&playing),
        );
        ui.write_all(&encode_reply(
            ui_planes::LOG,
            9,
            b"[gst_video] a line from the UI",
        ))
        .unwrap();
        ui.write_all(&status).unwrap();
        wait_for(&player, |s| *s == playing);

        drop(ui);
        wait_for(&player, |s| s.failed);
    }

    #[test]
    fn a_dropped_player_lets_the_ui_go() {
        let path = socket_path("drop");
        let listener = UnixListener::bind(&path).unwrap();
        let player = UiUrlPlayer::open(path.to_str().unwrap(), 9, "http://h/", "").unwrap();
        let (mut ui, _) = listener.accept().unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = ui.set_read_timeout(Some(Duration::from_secs(5)));
        assert_eq!(next(&mut ui, &mut Framer::new()).op, ui_planes::PLAY_URL);
        drop(player);
        assert_eq!(ui.read(&mut [0u8; 16]).unwrap(), 0);
    }
}
