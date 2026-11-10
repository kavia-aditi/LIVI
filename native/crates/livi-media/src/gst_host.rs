// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use livi_host_proto::ops::*;
use livi_host_proto::{Framer, encode_reply as frame};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::{broadcast, mpsc, oneshot};

const ANSWER_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
    Vp9,
    Av1,
}

impl Codec {
    pub fn as_str(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::H265 => "h265",
            Codec::Vp9 => "vp9",
            Codec::Av1 => "av1",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum HostEvent {
    Config {
        plane: u32,
        codec: Codec,
        atom: Vec<u8>,
    },
    Started {
        plane: u32,
    },
    AudioStarted {
        stream: u32,
        first_sample: u32,
    },
    AudioDelay {
        stream: u32,
        ms: u32,
    },
    Spectrum {
        bands: Vec<f32>,
    },
    UrlStatus {
        plane: u32,
        status: livi_host_proto::UrlStatus,
    },
    Cameras {
        list: Vec<livi_host_proto::CameraSeen>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedTarget {
    Plane(u32),
    Audio(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    AacLc = 0,
    Opus = 1,
    Pcm = 2,
    PcmLe = 3,
    AacRaw = 4,
}

pub fn aac_raw_key(asc: &[u8]) -> [u8; 32] {
    let mut key = [0u8; 32];
    let len = asc.len().min(31);
    key[0] = len as u8;
    key[1..1 + len].copy_from_slice(&asc[..len]);
    key
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioOpts {
    pub codec: AudioCodec,
    pub payload_type: u8,
    pub clock_rate: u32,
    pub channels: u8,
    pub latency_ms: u32,
    pub realtime: bool,
    pub fed: bool,
    pub device: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicOpts {
    pub pcm: bool,
    pub payload_type: u8,
    pub sample_rate: u32,
    pub channels: u8,
    pub bitrate: u32,
    pub frame_ms: u32,
    pub port: u16,
    pub phone: String,
    pub device: String,
}

#[derive(Debug, Clone)]
pub struct Launch {
    pub bin: PathBuf,
    pub env: Vec<(String, String)>,
    pub log: PathBuf,
    pub crash: PathBuf,
}

#[derive(Clone)]
enum HostSender {
    Legacy(mpsc::UnboundedSender<Vec<u8>>),
    Native(mpsc::Sender<Vec<u8>>),
}

impl HostSender {
    fn send(&self, bytes: Vec<u8>) -> Result<(), ()> {
        match self {
            Self::Legacy(tx) => tx.send(bytes).map_err(|_| ()),
            Self::Native(tx) => tx.try_send(bytes).map_err(|_| ()),
        }
    }
}

enum HostReceiver {
    Legacy(mpsc::UnboundedReceiver<Vec<u8>>),
    Native(mpsc::Receiver<Vec<u8>>),
}

impl HostReceiver {
    async fn recv(&mut self) -> Option<Vec<u8>> {
        match self {
            Self::Legacy(rx) => rx.recv().await,
            Self::Native(rx) => rx.recv().await,
        }
    }
}

#[derive(Default)]
struct State {
    out: Option<(u64, HostSender)>,
    queue: Vec<Vec<u8>>,
    running: bool,
    launch_epoch: u64,
    unavailable: bool,
    port_waiters: HashMap<u32, oneshot::Sender<u16>>,
    audio_waiters: HashMap<u32, oneshot::Sender<(u16, u16)>>,
    feed_waiter: Option<oneshot::Sender<String>>,
    feed_path: Option<String>,
    accept_task: Option<tokio::task::JoinHandle<()>>,
    native_writer: Option<tokio::task::JoinHandle<()>>,
    native_reader: Option<tokio::task::JoinHandle<()>>,
    tap: bool,
    cameras: bool,
    native: bool,
    shared_native: bool,
    native_capabilities: bool,
    native_bytes: usize,
    native_messages: usize,
    native_owners: HashMap<u32, livi_host_proto::native::Owner>,
}

struct Inner {
    sock_path: PathBuf,
    feed_path: PathBuf,
    launch: Mutex<Option<Launch>>,
    native_host: Mutex<Option<GstHost>>,
    state: Mutex<State>,
    events: broadcast::Sender<HostEvent>,
    next_id: AtomicU32,
    next_conn: AtomicU64,
    feed_lock: tokio::sync::Mutex<()>,
    native_feedback: broadcast::Sender<livi_host_proto::native::Feedback>,
}

#[derive(Clone)]
pub struct GstHost {
    inner: Arc<Inner>,
}

impl GstHost {
    pub fn new(sock_path: PathBuf) -> Self {
        let feed_path = PathBuf::from(format!("{}.feed", sock_path.display()));
        let (events, _) = broadcast::channel(256);
        let native = isolated_media_runtime(&sock_path).is_some();
        let unavailable = cfg!(target_os = "macos")
            && std::env::var_os("LIVI_NATIVE_BT_DEV_DIR").is_some() && !native;
        Self {
            inner: Arc::new(Inner {
                sock_path,
                feed_path,
                launch: Mutex::new(None),
                native_host: Mutex::new(None),
                state: Mutex::new(State { native, unavailable, ..Default::default() }),
                events,
                next_id: AtomicU32::new(0x7b00_0000),
                next_conn: AtomicU64::new(0),
                feed_lock: tokio::sync::Mutex::new(()),
                native_feedback: broadcast::channel(16).0,
            }),
        }
    }

    pub fn set_launch(&self, launch: Launch) {
        if let Some(host) = self.native_host() {
            let mut native = launch.clone();
            native.log = launch.log.with_extension("native.log");
            native.crash = launch.crash.with_extension("native.crash");
            host.set_launch(native);
        }
        *self.inner.launch.lock().unwrap_or_else(|e| e.into_inner()) = Some(launch);
    }

    fn native_host(&self) -> Option<Self> {
        self.inner.native_host.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<HostEvent> {
        self.inner.events.subscribe()
    }

    /// Native video/control never share a socket, queue, feed, or process with
    /// legacy video/audio/HFP. This selects rendering, not radio authorization.
    pub fn select_shared_native(&self) -> bool {
        let mut st = self.state();
        if self.native_host().is_some() { return true; }
        if st.unavailable || st.running || st.out.is_some() || !st.queue.is_empty() {
            return false;
        }
        if st.native { return false; }
        let host = Self::new(self.inner.sock_path.with_file_name("gst.native.sock"));
        {
            let mut native = host.state();
            native.native = true;
            native.shared_native = true; // recoverable, but never a shared wire
        }
        *self.inner.native_host.lock().unwrap_or_else(|e| e.into_inner()) = Some(host);
        st.shared_native = true; // composition marker; legacy state.native stays false
        drop(st);
        let launch = self.inner.launch.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(launch) = launch { self.set_launch(launch); }
        true
    }

    /// Isolated composition must select this before any connection or launch.
    pub fn select_native(&self) -> bool {
        let mut st = self.state();
        if st.unavailable { return false; }
        if st.native { return !st.unavailable; }
        if st.running || st.out.is_some() || !st.queue.is_empty() { return false; }
        st.native = true;
        true
    }

    pub fn fail_native(&self, owner: livi_host_proto::native::Owner) {
        if let Some(host) = self.native_host() { host.fail_native(owner); return; }
        let _ = self.inner.native_feedback.send(livi_host_proto::native::Feedback::Failed {
            owner, code: livi_host_proto::native::Failure::Backpressure,
        });
    }

    fn reserve_native(st: &mut State, bytes: usize) -> bool {
        if st.native_messages >= 32 || st.native_bytes.saturating_add(bytes) > 8 * 1024 * 1024 {
            return false;
        }
        st.native_messages += 1;
        st.native_bytes += bytes;
        true
    }

    pub fn native_feedback(&self) -> broadcast::Receiver<livi_host_proto::native::Feedback> {
        if let Some(host) = self.native_host() { return host.native_feedback(); }
        self.inner.native_feedback.subscribe()
    }

    pub fn allocate_native_feed(&self) -> u32 {
        self.native_host().map_or_else(|| self.next_id(), |host| host.next_id())
    }

    pub async fn open_native_feed(&self) -> String {
        match self.native_host() {
            Some(host) => host.open_feed().await,
            None => self.open_feed().await,
        }
    }

    pub fn native_owner(
        &self, runtime: u64, attempt: u64, generation: u64, session: u64,
    ) -> Option<livi_host_proto::native::Owner> {
        if let Some(host) = self.native_host() {
            return host.native_owner(runtime, attempt, generation, session);
        }
        let st = self.state();
        if !st.native || !st.native_capabilities { return None; }
        let connection = st.out.as_ref()?.0.checked_add(1)?;
        let owner = livi_host_proto::native::Owner {
            runtime, attempt, generation, session, connection,
            player: u64::from(self.next_id()),
        };
        owner.valid().then_some(owner)
    }

    pub fn send_native(&self, feed: u32, packet: &livi_host_proto::native::Packet) -> bool {
        if let Some(host) = self.native_host() { return host.send_native(feed, packet); }
        use livi_host_proto::native::Packet;
        let op = match packet {
            Packet::Create(_) => OP_NATIVE_CREATE, Packet::Data { .. } => OP_NATIVE_DATA,
            Packet::Stop(_) => OP_NATIVE_STOP, Packet::Flush(_) => OP_NATIVE_FLUSH,
            Packet::Route { .. } => OP_NATIVE_ROUTE, Packet::Active { .. } => OP_NATIVE_ACTIVE,
            _ => return false,
        };
        let Ok(bytes) = livi_host_proto::native::frame(op, feed, packet) else { return false };
        let Some(owner) = packet.owner() else { return false };
        let mut st = self.state();
        let Some((connection, tx)) = st.out.clone() else { return false };
        if !st.native || !st.native_capabilities || Some(owner.connection) != connection.checked_add(1) {
            return false;
        }
        if matches!(packet, Packet::Create(_)) {
            if st.native_owners.contains_key(&feed) || st.native_owners.len() >= 3 { return false; }
            st.native_owners.insert(feed, owner);
        } else if st.native_owners.get(&feed) != Some(&owner) { return false; }
        if !Self::reserve_native(&mut st, bytes.len()) || tx.send(bytes).is_err() {
            st.unavailable = true;
            self.reset(&mut st);
            return false;
        }
        if matches!(packet, Packet::Stop(_)) { st.native_owners.remove(&feed); }
        true
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn next_id(&self) -> u32 {
        self.inner.next_id.try_update(Ordering::Relaxed, Ordering::Relaxed,
            |id| id.checked_add(1)).expect("media identity exhausted")
    }

    fn start(&self) {
        let launch = self
            .inner
            .launch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let epoch = {
            let mut st = self.state();
            if st.running || st.unavailable {
                return;
            }
            st.launch_epoch = st.launch_epoch.checked_add(1).expect("media launch identity exhausted");
            st.running = true;
            st.launch_epoch
        };
        let _ = std::fs::remove_file(&self.inner.sock_path);
        let listener = match UnixListener::bind(&self.inner.sock_path) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[core] gst-host socket: {e}");
                self.state().running = false;
                return;
            }
        };
        let accept = tokio::spawn(self.clone().accept(listener));
        self.state().accept_task = Some(accept);
        if let Some(launch) = launch {
            self.spawn_host(launch, epoch);
        }
    }

    fn spawn_host(&self, launch: Launch, epoch: u64) {
        let _ = std::fs::remove_file(&launch.crash);
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&launch.bin, std::fs::Permissions::from_mode(0o755));
        }
        let (stdout, stderr) = match log_file(&launch.log) {
            Some(f) => (
                f.try_clone()
                    .map(Stdio::from)
                    .unwrap_or_else(|_| Stdio::inherit()),
                Stdio::from(f),
            ),
            None => (Stdio::inherit(), Stdio::inherit()),
        };
        let mut command = tokio::process::Command::new(&launch.bin);
        let (isolated, shared) = {
            let state = self.state();
            (state.native && !state.shared_native, state.native && state.shared_native)
        };
        if isolated {
            command.env("LIVI_ISOLATED_NATIVE_MEDIA", "1");
        }
        if shared {
            command.env("LIVI_SHARED_NATIVE_MEDIA", "1");
        }
        let child = command
            .arg(&self.inner.sock_path)
            .arg(&launch.crash)
            .envs(launch.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .kill_on_drop(true)
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[core] cannot start gst-host, media has nowhere to go: {e}");
                let mut st = self.state();
                st.unavailable = true;
                self.reset(&mut st);
                return;
            }
        };
        let host = self.clone();
        tokio::spawn(async move {
            let status = child.wait().await;
            eprintln!("[core] gst-host exited: {status:?}");
            if let Ok(st) = &status {
                use std::os::unix::process::ExitStatusExt;
                if st.signal().is_some()
                    && let Ok(trace) = std::fs::read_to_string(&launch.crash)
                {
                    eprintln!(
                        "[core] gst-host crash backtrace ({}):\n{trace}",
                        launch.crash.display()
                    );
                }
            }
            host.reset_launch(epoch);
        });
    }

    fn reset_launch(&self, epoch: u64) {
        let mut st = self.state();
        if st.launch_epoch == epoch { self.reset(&mut st); }
    }

    fn reset(&self, st: &mut State) {
        st.running = false;
        st.out = None;
        if let Some(task) = st.native_writer.take() { task.abort(); }
        if let Some(task) = st.native_reader.take() { task.abort(); }
        st.native_capabilities = false;
        st.native_bytes = 0;
        st.native_messages = 0;
        for owner in st.native_owners.drain().map(|(_, owner)| owner) {
            let _ = self.inner.native_feedback.send(livi_host_proto::native::Feedback::Failed {
                owner, code: livi_host_proto::native::Failure::Disconnected,
            });
        }
        if st.native {
            st.unavailable = !st.shared_native;
            st.queue.clear();
        }
        st.feed_path = None;
        if let Some(waiter) = st.feed_waiter.take() {
            let _ = waiter.send(String::new());
        }
        if let Some(task) = st.accept_task.take() {
            task.abort();
        }
    }

    async fn accept(self, listener: UnixListener) {
        while let Ok((stream, _)) = listener.accept().await {
            let occupied = { let st = self.state(); st.native && st.out.is_some() };
            if occupied { drop(stream); continue; }
            let Ok(conn) = self.inner.next_conn.try_update(Ordering::Relaxed, Ordering::Relaxed,
                |id| id.checked_add(1)) else { drop(stream); break };
            let (tx, mut rx) = if self.state().native {
                let (tx, rx) = mpsc::channel(32);
                (HostSender::Native(tx), HostReceiver::Native(rx))
            } else {
                let (tx, rx) = mpsc::unbounded_channel();
                (HostSender::Legacy(tx), HostReceiver::Legacy(rx))
            };
            let (mut rd, mut wr) = stream.into_split();
            {
                let mut st = self.state();
                st.native_capabilities = false;
                st.native_bytes = 0;
                st.native_messages = 0;
                st.native_owners.clear();
                let mut initial = Vec::new();
                if st.native {
                    let query = livi_host_proto::native::Packet::Capabilities(
                        livi_host_proto::native::Capabilities { bits: 0 });
                    initial.push(livi_host_proto::native::frame(
                        OP_NATIVE_CAPABILITIES, 0, &query).unwrap());
                }
                if st.tap {
                    initial.push(frame(OP_VISUALIZER, 0, &[1]));
                }
                if st.cameras {
                    initial.push(frame(OP_CAMERAS_WATCH, 0, &[]));
                }
                initial.append(&mut st.queue);
                for queued in initial {
                    if st.native && !Self::reserve_native(&mut st, queued.len()) {
                        st.unavailable = true;
                        self.reset(&mut st);
                        return;
                    }
                    let _ = tx.send(queued);
                }
                st.out = Some((conn, tx));
            }
            let writer_host = self.clone();
            let writer = tokio::spawn(async move {
                while let Some(buf) = rx.recv().await {
                    let native = writer_host.state().native;
                    let written = if native {
                        tokio::time::timeout(Duration::from_secs(2), wr.write_all(&buf))
                            .await.is_ok_and(|result| result.is_ok())
                    } else {
                        wr.write_all(&buf).await.is_ok()
                    };
                    if !written {
                        if native {
                            let mut st = writer_host.state();
                            if st.out.as_ref().is_some_and(|(id, _)| *id == conn) {
                                writer_host.reset(&mut st);
                            }
                        }
                        break;
                    }
                    let mut st = writer_host.state();
                    if st.out.as_ref().is_some_and(|(id, _)| *id == conn) {
                        st.native_bytes = st.native_bytes.saturating_sub(buf.len());
                        st.native_messages = st.native_messages.saturating_sub(1);
                    }
                }
            });
            {
                let mut st = self.state();
                if st.native {
                    if st.out.as_ref().is_some_and(|(id, _)| *id == conn) {
                        st.native_writer = Some(writer);
                    } else { writer.abort(); }
                }
            }
            let host = self.clone();
            let reader = tokio::spawn(async move {
                let mut framer = Framer::new();
                let mut bounded = livi_host_proto::native::Framer::new();
                let native = host.state().native;
                let mut buf = vec![0u8; 64 * 1024];
                let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
                loop {
                    let read = if native {
                        {
                            let st = host.state();
                            if st.out.as_ref().is_none_or(|(id, _)| *id != conn)
                                || (!st.native_capabilities && tokio::time::Instant::now() >= deadline) {
                                break;
                            }
                        }
                        match tokio::time::timeout(Duration::from_millis(100), rd.read(&mut buf)).await {
                            Ok(result) => result,
                            Err(_) => continue,
                        }
                    } else { rd.read(&mut buf).await };
                    let Ok(n) = read else { break };
                    if n == 0 {
                        break;
                    }
                    if native {
                        if bounded.push(&buf[..n]).is_err() { break; }
                        while let Some(msg) = bounded.next_message() {
                            if host.state().out.as_ref().is_some_and(|(id, _)| *id == conn) {
                                host.on_reply(msg.op, msg.id, &msg.rest);
                            }
                        }
                        continue;
                    }
                    framer.push(&buf[..n]);
                    while let Some(msg) = framer.next_message() {
                        host.on_reply(msg.op, msg.id, &msg.rest);
                    }
                }
                let mut st = host.state();
                if st.out.as_ref().is_some_and(|(c, _)| *c == conn) {
                    if native {
                        host.reset(&mut st);
                    } else {
                        st.out = None;
                    }
                }
            });
            {
                let mut st = self.state();
                if st.native {
                    if st.out.as_ref().is_some_and(|(id, _)| *id == conn) {
                        st.native_reader = Some(reader);
                    } else { reader.abort(); }
                }
            }
        }
    }

    fn send(&self, buf: Vec<u8>) {
        self.start();
        let mut st = self.state();
        if st.unavailable {
            return;
        }
        if st.native && !Self::reserve_native(&mut st, buf.len()) {
            st.unavailable = true;
            self.reset(&mut st);
            return;
        }
        match &st.out {
            Some((_, tx)) => {
                if tx.send(buf).is_err() && st.native { self.reset(&mut st); }
            }
            None => st.queue.push(buf),
        }
    }

    fn on_reply(&self, op: u8, id: u32, rest: &[u8]) {
        let u16_at = |at: usize| {
            rest.get(at..at + 2)
                .map_or(0, |b| u16::from_le_bytes([b[0], b[1]]))
        };
        let u32_at = |at: usize| {
            rest.get(at..at + 4)
                .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        };
        match op {
            REPLY_NATIVE_CAPABILITIES => {
                if let Some(livi_host_proto::native::Packet::Capabilities(c)) = parse_native(op, true, rest) {
                    let mut st = self.state();
                    st.native_capabilities = st.native && c.supports_stage_five();
                }
            }
            REPLY_NATIVE_FEEDBACK => {
                if let Some(livi_host_proto::native::Packet::Feedback(feedback)) = parse_native(op, true, rest) {
                    let packet = livi_host_proto::native::Packet::Feedback(feedback.clone());
                    if self.state().native_owners.get(&id).copied() == packet.owner() {
                        let _ = self.inner.native_feedback.send(feedback);
                    }
                }
            }
            REPLY_PORT => {
                if let Some(w) = self.state().port_waiters.remove(&id) {
                    let _ = w.send(u16_at(0));
                }
            }
            REPLY_CONFIG => {
                let codec = if rest.first() == Some(&1) {
                    Codec::H265
                } else {
                    Codec::H264
                };
                let atom = rest.get(1..).unwrap_or_default().to_vec();
                let _ = self.inner.events.send(HostEvent::Config {
                    plane: id,
                    codec,
                    atom,
                });
            }
            REPLY_STARTED => {
                let _ = self.inner.events.send(HostEvent::Started { plane: id });
            }
            REPLY_AUDIO_PORTS => {
                if let Some(w) = self.state().audio_waiters.remove(&id) {
                    let ports = if rest.len() >= 4 {
                        (u16_at(0), u16_at(2))
                    } else {
                        (0, 0)
                    };
                    let _ = w.send(ports);
                }
            }
            REPLY_AUDIO_STARTED => {
                let _ = self.inner.events.send(HostEvent::AudioStarted {
                    stream: id,
                    first_sample: u32_at(0),
                });
            }
            REPLY_VISUALIZER => {
                let bands = rest
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| f32::from_le_bytes(*b))
                    .collect();
                let _ = self.inner.events.send(HostEvent::Spectrum { bands });
            }
            REPLY_FEED => {
                if let Some(w) = self.state().feed_waiter.take() {
                    let _ = w.send(String::from_utf8_lossy(rest).into_owned());
                }
            }
            REPLY_AUDIO_DELAY => {
                let _ = self.inner.events.send(HostEvent::AudioDelay {
                    stream: id,
                    ms: u32_at(0),
                });
            }
            REPLY_CAMERAS => {
                let list = livi_host_proto::parse_cameras_body(rest);
                let _ = self.inner.events.send(HostEvent::Cameras { list });
            }
            REPLY_URL_STATUS => {
                if let Some(status) = livi_host_proto::parse_url_status(rest) {
                    let _ = self
                        .inner
                        .events
                        .send(HostEvent::UrlStatus { plane: id, status });
                }
            }
            _ => {}
        }
    }

    pub async fn open_feed(&self) -> String {
        let _one_at_a_time = self.inner.feed_lock.lock().await;
        if let Some(path) = self.state().feed_path.clone() {
            return path;
        }
        let path = self.inner.feed_path.display().to_string();
        let (tx, rx) = oneshot::channel();
        self.state().feed_waiter = Some(tx);
        self.send(frame(OP_FEED_OPEN, 0, path.as_bytes()));
        let bound = tokio::time::timeout(ANSWER_TIMEOUT, rx)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        let mut st = self.state();
        st.feed_waiter = None;
        if !bound.is_empty() {
            st.feed_path = Some(bound.clone());
        }
        bound
    }

    pub async fn open_video_receiver(
        &self,
        plane: u32,
        key: &[u8; 32],
        cluster: bool,
    ) -> (u16, u32) {
        let receiver = self.next_id();
        let (tx, rx) = oneshot::channel();
        self.state().port_waiters.insert(receiver, tx);
        let mut rest = plane.to_le_bytes().to_vec();
        rest.push(u8::from(cluster));
        rest.extend_from_slice(key);
        self.send(frame(OP_LISTEN, receiver, &rest));
        let port = tokio::time::timeout(ANSWER_TIMEOUT, rx)
            .await
            .ok()
            .and_then(Result::ok);
        if port.is_none() {
            self.state().port_waiters.remove(&receiver);
        }
        (port.unwrap_or(0), receiver)
    }

    pub fn set_active_feeder(&self, receiver: u32, active: bool) {
        self.send(frame(OP_SET_ACTIVE, receiver, &[u8::from(active)]));
    }

    pub fn close_video_receiver(&self, receiver: u32) {
        self.send(frame(OP_TEARDOWN, receiver, &[]));
    }

    pub fn open_feed_route(&self, target: FeedTarget, active: bool) -> u32 {
        let id = self.next_id();
        let (kind, target) = match target {
            FeedTarget::Plane(plane) => (0u8, plane),
            FeedTarget::Audio(stream) => (1, stream),
        };
        let mut rest = vec![kind];
        rest.extend_from_slice(&target.to_le_bytes());
        rest.push(u8::from(active));
        self.send(frame(OP_FEED_ROUTE, id, &rest));
        id
    }

    pub fn close_feed_route(&self, id: u32) {
        self.send(frame(OP_FEED_CLOSE, id, &[]));
    }

    pub async fn open_audio(&self, key: &[u8; 32], o: &AudioOpts) -> (u32, u16, u16) {
        let stream = self.next_id();
        let (tx, rx) = oneshot::channel();
        self.state().audio_waiters.insert(stream, tx);
        let mut rest = vec![o.codec as u8, o.payload_type];
        rest.extend_from_slice(&o.clock_rate.to_le_bytes());
        rest.push(o.channels);
        rest.extend_from_slice(&o.latency_ms.to_le_bytes());
        rest.push(u8::from(o.realtime) | (u8::from(o.fed) << 1));
        rest.extend_from_slice(key);
        rest.extend_from_slice(o.device.as_bytes());
        self.send(frame(OP_AUDIO_OPEN, stream, &rest));
        let ports = tokio::time::timeout(ANSWER_TIMEOUT, rx)
            .await
            .ok()
            .and_then(Result::ok);
        if ports.is_none() {
            self.state().audio_waiters.remove(&stream);
        }
        let (data, control) = ports.unwrap_or((0, 0));
        (stream, data, control)
    }

    pub fn push_audio(&self, stream: u32, samples: &[u8]) {
        self.send(frame(OP_AUDIO_DATA, stream, samples));
    }

    pub fn set_visualizer_tap(&self, on: bool) {
        let connected = {
            let mut st = self.state();
            st.tap = on;
            st.out.is_some()
        };
        if connected {
            self.send(frame(OP_VISUALIZER, 0, &[u8::from(on)]));
        } else if on {
            self.start();
        }
    }

    pub fn watch_cameras(&self) {
        let connected = {
            let mut st = self.state();
            st.cameras = true;
            st.out.is_some()
        };
        if connected {
            self.send(frame(OP_CAMERAS_WATCH, 0, &[]));
        } else {
            self.start();
        }
    }

    pub fn open_camera(&self, plane: u32, order: &livi_host_proto::CameraOrder) {
        self.send(frame(
            OP_CAMERA_OPEN,
            plane,
            &livi_host_proto::camera_body(order),
        ));
    }

    pub fn set_audio_volume(&self, stream: u32, level: f64, ramp_ms: u32) {
        let mut rest = level.to_le_bytes().to_vec();
        rest.extend_from_slice(&ramp_ms.to_le_bytes());
        self.send(frame(OP_AUDIO_VOLUME, stream, &rest));
    }

    pub fn set_audio_output(&self, device: &str) {
        if self.state().running {
            self.send(frame(OP_AUDIO_OUTPUT, 0, device.as_bytes()));
        }
    }

    pub fn set_audio_active(&self, stream: u32, active: bool) {
        self.send(frame(OP_AUDIO_ACTIVE, stream, &[u8::from(active)]));
    }

    pub fn close_audio(&self, stream: u32) {
        self.state().audio_waiters.remove(&stream);
        self.send(frame(OP_AUDIO_STOP, stream, &[]));
    }

    pub fn open_mic(&self, key: &[u8; 32], o: &MicOpts) -> u32 {
        let id = self.next_id();
        let mut rest = vec![u8::from(o.pcm), o.payload_type];
        rest.extend_from_slice(&o.sample_rate.to_le_bytes());
        rest.push(o.channels);
        rest.extend_from_slice(&o.bitrate.to_le_bytes());
        rest.extend_from_slice(&o.frame_ms.to_le_bytes());
        rest.extend_from_slice(&o.port.to_le_bytes());
        rest.extend_from_slice(key);
        let phone = o.phone.as_bytes();
        rest.push(phone.len() as u8);
        rest.extend_from_slice(phone);
        rest.extend_from_slice(o.device.as_bytes());
        self.send(frame(OP_MIC_OPEN, id, &rest));
        id
    }

    pub fn close_mic(&self, id: u32) {
        self.send(frame(OP_MIC_STOP, id, &[]));
    }

    pub fn open_mic_tap(&self, path: &str, sample_rate: u32, channels: u8, device: &str) -> u32 {
        let id = self.next_id();
        let mut rest = sample_rate.to_le_bytes().to_vec();
        rest.push(channels);
        rest.push(device.len() as u8);
        rest.extend_from_slice(device.as_bytes());
        rest.extend_from_slice(path.as_bytes());
        self.send(frame(OP_TAP_OPEN, id, &rest));
        id
    }

    pub fn close_mic_tap(&self, id: u32) {
        self.send(frame(OP_TAP_STOP, id, &[]));
    }

    pub fn create_player(&self, plane: u32, codec: Codec, codec_data: &[u8]) {
        self.send(frame(
            OP_CREATE,
            plane,
            &livi_host_proto::plane_body(codec.as_str(), codec_data),
        ));
    }

    pub fn play_url(&self, plane: u32, url: &str, audio_device: &str) {
        self.send(frame(
            OP_PLAY_URL,
            plane,
            &livi_host_proto::url_body(url, audio_device),
        ));
    }

    pub fn set_play_rate(&self, plane: u32, rate: f64) {
        self.send(frame(OP_PLAY_RATE, plane, &rate.to_le_bytes()));
    }

    pub fn seek(&self, plane: u32, seconds: f64) {
        self.send(frame(OP_PLAY_SEEK, plane, &seconds.to_le_bytes()));
    }

    pub fn set_play_muted(&self, plane: u32, muted: bool) {
        self.send(frame(OP_PLAY_MUTE, plane, &[u8::from(muted)]));
    }

    pub fn stop(&self, plane: u32) {
        self.send(frame(OP_STOP, plane, &[]));
    }
}

#[cfg(target_os = "macos")]
fn isolated_media_runtime(socket: &std::path::Path) -> Option<u64> {
    unsafe extern "C" { fn geteuid() -> u32; }
    if socket.file_name()? != "gst.sock" { return None; }
    livi_host_proto::isolated_runtime::Layout::from_transport(
        socket.parent()?, unsafe { geteuid() }, 0x100,
        std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
        std::env::var_os("LIVI_NATIVE_BT_DEV_DIR").as_deref(), None,
    ).map(|layout| layout.runtime)
}

#[cfg(not(target_os = "macos"))]
fn isolated_media_runtime(_: &std::path::Path) -> Option<u64> { None }

fn log_file(path: &std::path::Path) -> Option<std::fs::File> {
    std::fs::create_dir_all(path.parent()?).ok()?;
    std::fs::File::create(path).ok()
}

pub async fn probe_codecs(launch: &Launch) -> Option<serde_json::Value> {
    let run = tokio::process::Command::new(&launch.bin)
        .arg("--probe")
        .envs(launch.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match tokio::time::timeout(Duration::from_secs(10), run).await {
        Ok(Ok(out)) => {
            serde_json::from_slice(String::from_utf8_lossy(&out.stdout).trim().as_bytes()).ok()
        }
        _ => {
            eprintln!("[core] gst-host codec probe failed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use livi_host_proto::encode_reply;
    use tokio::net::UnixStream;

    use super::*;
    use crate::test_dir::TempDir;

    struct FakeHost {
        stream: UnixStream,
        framer: Framer,
    }

    impl FakeHost {
        async fn connect(host: &GstHost) -> Self {
            host.start();
            let stream = UnixStream::connect(&host.inner.sock_path).await.unwrap();
            Self {
                stream,
                framer: Framer::new(),
            }
        }

        async fn next(&mut self) -> livi_host_proto::Message {
            let mut buf = [0u8; 4096];
            loop {
                if let Some(m) = self.framer.next_message() {
                    return m;
                }
                let n = tokio::time::timeout(Duration::from_secs(5), self.stream.read(&mut buf))
                    .await
                    .unwrap()
                    .unwrap();
                assert!(n > 0, "core closed the socket");
                self.framer.push(&buf[..n]);
            }
        }

        async fn reply(&mut self, op: u8, id: u32, rest: &[u8]) {
            self.stream
                .write_all(&encode_reply(op, id, rest))
                .await
                .unwrap();
        }
    }

    fn host(dir: &TempDir) -> GstHost {
        GstHost::new(dir.0.join("gst.sock"))
    }

    #[test]
    fn shared_native_launch_keeps_legacy_configuration_and_distinct_diagnostics() {
        let dir = TempDir::new();
        let gst = host(&dir);
        assert!(gst.select_shared_native());
        let launch = Launch {
            bin: dir.0.join("not-executed"),
            env: vec![("OFFLINE_ONLY".into(), "1".into())],
            log: dir.0.join("gst.log"),
            crash: dir.0.join("gst.crash"),
        };
        gst.set_launch(launch.clone());
        let native = gst.native_host().unwrap();
        let native_launch = native.inner.launch.lock().unwrap().clone().unwrap();
        assert_eq!(native_launch.bin, launch.bin);
        assert_eq!(native_launch.env, launch.env);
        assert_ne!(native_launch.log, launch.log);
        assert_ne!(native_launch.crash, launch.crash);
        assert_ne!(native_launch.log, native_launch.crash);
        assert_eq!(gst.inner.launch.lock().unwrap().as_ref().unwrap().log, launch.log);
        assert!(!gst.state().running && !native.state().running);
    }

    async fn native_ready(gst: &GstHost) -> (GstHost, FakeHost, livi_host_proto::native::Owner, u32) {
        use livi_host_proto::native::{Capabilities, Create, Packet, REQUIRED_CAPABILITIES};
        let native = gst.native_host().unwrap();
        let mut fake = FakeHost::connect(&native).await;
        assert_eq!(fake.next().await.op, OP_NATIVE_CAPABILITIES);
        fake.reply(REPLY_NATIVE_CAPABILITIES, 0,
            &Packet::Capabilities(Capabilities { bits: REQUIRED_CAPABILITIES }).encode().unwrap()).await;
        for _ in 0..100 {
            if gst.native_owner(1, 2, 3, 4).is_some() { break; }
            tokio::task::yield_now().await;
        }
        let owner = gst.native_owner(1, 2, 3, 4).unwrap();
        let feed = gst.allocate_native_feed();
        assert!(gst.send_native(feed, &Packet::Create(Create { owner, codec: "h264".into(), codec_data: vec![] })));
        assert_eq!(fake.next().await.op, OP_NATIVE_CREATE);
        (native, fake, owner, feed)
    }

    async fn legacy_continues(gst: &GstHost, legacy: &mut FakeHost, connection: u64) {
        assert!(!gst.state().native);
        assert!(gst.state().running);
        assert_eq!(gst.state().out.as_ref().unwrap().0, connection);
        gst.create_player(11, Codec::H264, &[1]);
        assert_eq!(legacy.next().await.op, OP_CREATE);
        gst.push_audio(55, &[2, 3]);
        let audio = legacy.next().await;
        assert_eq!((audio.op, audio.id, audio.rest), (OP_AUDIO_DATA, 55, vec![2, 3]));
        gst.set_audio_active(56, true);
        assert_eq!(legacy.next().await.op, OP_AUDIO_ACTIVE);
        gst.stop(12);
        assert_eq!(legacy.next().await.op, OP_STOP);
    }

    #[tokio::test]
    async fn shared_native_queue_exhaustion_cannot_reset_legacy_and_reconnects() {
        use livi_host_proto::native::Packet;
        let dir = TempDir::new();
        let gst = host(&dir);
        assert!(gst.select_shared_native());
        let mut legacy = FakeHost::connect(&gst).await;
        gst.stop(10);
        assert_eq!(legacy.next().await.op, OP_STOP);
        let connection = gst.state().out.as_ref().unwrap().0;
        let mut feedback = gst.native_feedback();
        let (native, mut fake, owner, feed) = native_ready(&gst).await;
        // No await: the bounded reservation cannot drain during this burst.
        let mut rejected = false;
        for _ in 0..33 {
            if !gst.send_native(feed, &Packet::Flush(owner)) { rejected = true; break; }
        }
        assert!(rejected);
        assert!(matches!(feedback.recv().await.unwrap(),
            livi_host_proto::native::Feedback::Failed { owner: failed, .. } if failed == owner));
        assert!(native.state().native_writer.is_none());
        assert!(native.state().native_reader.is_none());
        legacy_continues(&gst, &mut legacy, connection).await;
        assert!(native.state().native_owners.is_empty());
        assert!(!gst.send_native(feed, &Packet::Flush(owner)));
        // Drain/close the failed incarnation, then negotiate a fresh one.
        let _ = fake.stream.shutdown().await;
        drop(fake);
        let (_, mut again, current, current_feed) = native_ready(&gst).await;
        assert_ne!(owner.connection, current.connection);
        assert!(!gst.send_native(feed, &Packet::Stop(owner)));
        assert!(gst.send_native(current_feed, &Packet::Stop(current)));
        assert_eq!(again.next().await.op, OP_NATIVE_STOP);
        legacy_continues(&gst, &mut legacy, connection).await;
    }

    #[tokio::test]
    async fn shared_native_stalled_reader_write_timeout_leaves_legacy_healthy() {
        use livi_host_proto::native::Packet;
        let dir = TempDir::new();
        let gst = host(&dir);
        assert!(gst.select_shared_native());
        let mut legacy = FakeHost::connect(&gst).await;
        gst.stop(10);
        assert_eq!(legacy.next().await.op, OP_STOP);
        let connection = gst.state().out.as_ref().unwrap().0;
        let (native, _stalled, owner, feed) = native_ready(&gst).await;
        let mut feedback = gst.native_feedback();
        assert!(gst.send_native(feed, &Packet::Data {
            owner, bytes: vec![0; 1024 * 1024],
        }));
        tokio::time::timeout(Duration::from_secs(4), feedback.recv()).await.unwrap().unwrap();
        assert!(!native.state().running);
        legacy_continues(&gst, &mut legacy, connection).await;
    }

    #[tokio::test]
    async fn shared_native_missing_capabilities_and_eof_leave_legacy_healthy() {
        let dir = TempDir::new();
        let gst = host(&dir);
        assert!(gst.select_shared_native());
        let mut legacy = FakeHost::connect(&gst).await;
        gst.stop(10);
        assert_eq!(legacy.next().await.op, OP_STOP);
        let connection = gst.state().out.as_ref().unwrap().0;
        let native = gst.native_host().unwrap();
        let mut silent = FakeHost::connect(&native).await;
        assert_eq!(silent.next().await.op, OP_NATIVE_CAPABILITIES);
        tokio::time::timeout(Duration::from_secs(4), async {
            while native.state().running { tokio::time::sleep(Duration::from_millis(10)).await; }
        }).await.unwrap();
        assert!(gst.native_owner(1, 2, 3, 4).is_none());
        legacy_continues(&gst, &mut legacy, connection).await;
        drop(silent);
        let (_, fake, owner, _) = native_ready(&gst).await;
        let mut feedback = gst.native_feedback();
        drop(fake);
        let failed = tokio::time::timeout(Duration::from_secs(2), feedback.recv()).await.unwrap().unwrap();
        assert!(matches!(failed, livi_host_proto::native::Feedback::Failed { owner: failed, .. } if failed == owner));
        legacy_continues(&gst, &mut legacy, connection).await;
    }

    #[tokio::test]
    async fn shared_native_render_failure_and_stop_never_reach_legacy() {
        use livi_host_proto::native::{Failure, Feedback, Packet};
        let dir = TempDir::new();
        let gst = host(&dir);
        assert!(gst.select_shared_native());
        let mut legacy = FakeHost::connect(&gst).await;
        gst.stop(10);
        assert_eq!(legacy.next().await.op, OP_STOP);
        let connection = gst.state().out.as_ref().unwrap().0;
        let (_, mut fake, owner, feed) = native_ready(&gst).await;
        let mut feedback = gst.native_feedback();
        let failed = Feedback::Failed { owner, code: Failure::Render };
        fake.reply(REPLY_NATIVE_FEEDBACK, feed, &Packet::Feedback(failed.clone()).encode().unwrap()).await;
        assert_eq!(feedback.recv().await.unwrap(), failed);
        assert!(gst.send_native(feed, &Packet::Stop(owner)));
        assert_eq!(fake.next().await.op, OP_NATIVE_STOP);
        legacy_continues(&gst, &mut legacy, connection).await;
    }

    #[tokio::test]
    async fn stale_child_exit_cannot_reset_a_replacement_listener_before_accept() {
        let dir = TempDir::new();
        let gst = host(&dir);
        assert!(gst.select_shared_native());
        let native = gst.native_host().unwrap();
        native.start();
        let old = native.state().launch_epoch;
        native.reset_launch(old);
        native.start();
        assert_ne!(native.state().launch_epoch, old);
        native.reset_launch(old);
        assert!(native.state().running);
        let mut replacement = FakeHost::connect(&native).await;
        assert_eq!(replacement.next().await.op, OP_NATIVE_CAPABILITIES);
    }

    #[test]
    fn native_queue_limits_count_and_bytes_without_partial_reservations() {
        let mut state = State::default();
        assert!(!GstHost::reserve_native(&mut state, 8 * 1024 * 1024 + 1));
        assert_eq!((state.native_bytes, state.native_messages), (0, 0));
        for _ in 0..32 { assert!(GstHost::reserve_native(&mut state, 1)); }
        assert!(!GstHost::reserve_native(&mut state, 1));
        assert_eq!((state.native_bytes, state.native_messages), (32, 32));
    }

    #[tokio::test]
    async fn legacy_transport_cannot_switch_after_start() {
        let dir = TempDir::new();
        let gst = host(&dir);
        let _fake = FakeHost::connect(&gst).await;
        assert!(!gst.select_native());
    }

    #[tokio::test]
    async fn native_transport_requires_capabilities_and_current_connection_owner() {
        use livi_host_proto::native::{Capabilities, Create, Packet, REQUIRED_CAPABILITIES};
        let dir = TempDir::new();
        let gst = host(&dir);
        assert!(gst.select_native());
        assert!(gst.native_owner(1, 2, 3, 4).is_none());
        let mut fake = FakeHost::connect(&gst).await;
        assert_eq!(fake.next().await.op, OP_NATIVE_CAPABILITIES);
        fake.reply(REPLY_NATIVE_CAPABILITIES, 0,
            &Packet::Capabilities(Capabilities { bits: REQUIRED_CAPABILITIES }).encode().unwrap()).await;
        for _ in 0..100 {
            if gst.native_owner(1, 2, 3, 4).is_some() { break; }
            tokio::task::yield_now().await;
        }
        let owner = gst.native_owner(1, 2, 3, 4).unwrap();
        let feed = gst.allocate_native_feed();
        let create = Packet::Create(Create { owner, codec: "h264".into(), codec_data: vec![] });
        assert!(gst.send_native(feed, &create));
        assert_eq!(fake.next().await.op, OP_NATIVE_CREATE);
        assert!(!gst.send_native(feed, &create));
        let mut stale = owner;
        stale.connection += 1;
        assert!(!gst.send_native(feed, &Packet::Stop(stale)));
        assert!(gst.send_native(feed, &Packet::Stop(owner)));
        assert_eq!(fake.next().await.op, OP_NATIVE_STOP);
        assert!(!gst.send_native(feed, &Packet::Flush(owner)));
        assert!(gst.select_native(), "an already selected transport stays native");
    }

    #[tokio::test]
    async fn commands_wait_for_the_host_then_arrive_in_order() {
        let dir = TempDir::new();
        let gst = host(&dir);
        gst.set_visualizer_tap(true);
        gst.stop(0x7a00_0001);
        let mut fake = FakeHost::connect(&gst).await;
        let first = fake.next().await;
        assert_eq!(
            (first.op, first.rest.as_slice()),
            (OP_VISUALIZER, &[1u8][..])
        );
        assert_eq!(fake.next().await.op, OP_STOP);
        gst.create_player(0x7a00_0001, Codec::H265, &[9, 9]);
        let create = fake.next().await;
        assert_eq!((create.op, create.id), (OP_CREATE, 0x7a00_0001));
        assert_eq!(create.rest, [&[4u8][..], b"h265", &[9, 9]].concat());
    }

    #[tokio::test]
    async fn the_tap_follows_the_host_through_a_restart_and_brings_the_bands() {
        let dir = TempDir::new();
        let gst = host(&dir);
        let mut events = gst.subscribe();
        gst.set_visualizer_tap(true);
        let mut first = FakeHost::connect(&gst).await;
        assert_eq!(first.next().await.rest, [1]);
        let bands: Vec<u8> = [0.25f32, 1.0]
            .iter()
            .flat_map(|b| b.to_le_bytes())
            .collect();
        first.reply(REPLY_VISUALIZER, 0, &bands).await;
        assert_eq!(
            events.recv().await.unwrap(),
            HostEvent::Spectrum {
                bands: vec![0.25, 1.0]
            }
        );
        let status = livi_host_proto::UrlStatus {
            position: Some(2.0),
            ..Default::default()
        };
        first
            .reply(
                REPLY_URL_STATUS,
                0x7a00_0002,
                &livi_host_proto::url_status_body(&status),
            )
            .await;
        first.reply(REPLY_URL_STATUS, 0x7a00_0002, &[1]).await;
        assert_eq!(
            events.recv().await.unwrap(),
            HostEvent::UrlStatus {
                plane: 0x7a00_0002,
                status
            }
        );
        drop(first);

        let mut again = FakeHost::connect(&gst).await;
        assert_eq!(
            (again.next().await.op, gst.state().tap),
            (OP_VISUALIZER, true)
        );
        gst.set_visualizer_tap(false);
        assert_eq!(again.next().await.rest, [0]);
    }

    #[tokio::test]
    async fn a_new_output_reaches_a_running_host_and_starts_none() {
        let dir = TempDir::new();
        let gst = host(&dir);
        gst.set_audio_output("headphones");
        assert!(!gst.state().running);
        let mut fake = FakeHost::connect(&gst).await;
        gst.set_audio_output("headphones");
        let output = fake.next().await;
        assert_eq!(
            (output.op, output.rest.as_slice()),
            (OP_AUDIO_OUTPUT, &b"headphones"[..])
        );
    }

    #[tokio::test]
    async fn a_tap_switched_off_before_any_host_sends_nothing() {
        let dir = TempDir::new();
        let gst = host(&dir);
        gst.set_visualizer_tap(false);
        gst.stop(0x7a00_0001);
        let mut fake = FakeHost::connect(&gst).await;
        assert_eq!(fake.next().await.op, OP_STOP);
    }

    #[tokio::test]
    async fn a_feed_route_names_its_target_and_closes_again() {
        let dir = TempDir::new();
        let gst = host(&dir);
        let mut fake = FakeHost::connect(&gst).await;
        let video = gst.open_feed_route(FeedTarget::Plane(0x7a00_0001), true);
        let audio = gst.open_feed_route(FeedTarget::Audio(0x7b00_0009), false);
        assert_ne!(video, audio);
        let first = fake.next().await;
        assert_eq!((first.op, first.id), (OP_FEED_ROUTE, video));
        assert_eq!(first.rest, [0, 1, 0, 0, 0x7a, 1]);
        let second = fake.next().await;
        assert_eq!(
            (second.id, second.rest.as_slice()),
            (audio, &[1, 9, 0, 0, 0x7b, 0][..])
        );
        gst.close_feed_route(video);
        let close = fake.next().await;
        assert_eq!((close.op, close.id), (OP_FEED_CLOSE, video));
    }

    #[tokio::test]
    async fn a_screen_receiver_gets_its_port() {
        let dir = TempDir::new();
        let gst = host(&dir);
        let mut fake = FakeHost::connect(&gst).await;
        let open = tokio::spawn({
            let gst = gst.clone();
            async move { gst.open_video_receiver(0x7a00_0001, &[7; 32], false).await }
        });
        let listen = fake.next().await;
        assert_eq!(listen.op, OP_LISTEN);
        assert_eq!(&listen.rest[..5], &[1, 0, 0, 0x7a, 0]);
        assert_eq!(&listen.rest[5..], &[7; 32]);
        fake.reply(REPLY_PORT, listen.id, &7100u16.to_le_bytes())
            .await;
        assert_eq!(open.await.unwrap(), (7100, listen.id));
    }

    #[tokio::test]
    async fn audio_ports_and_events_come_back() {
        let dir = TempDir::new();
        let gst = host(&dir);
        let mut events = gst.subscribe();
        let mut fake = FakeHost::connect(&gst).await;
        let opts = AudioOpts {
            codec: AudioCodec::Opus,
            payload_type: 100,
            clock_rate: 48000,
            channels: 1,
            latency_ms: 1000,
            realtime: true,
            fed: false,
            device: "sink".into(),
        };
        let open = tokio::spawn({
            let gst = gst.clone();
            async move { gst.open_audio(&[1; 32], &opts).await }
        });
        let req = fake.next().await;
        assert_eq!(
            &req.rest[..12],
            &[1, 100, 0x80, 0xbb, 0, 0, 1, 0xe8, 3, 0, 0, 1]
        );
        assert_eq!(&req.rest[44..], b"sink");
        fake.reply(REPLY_AUDIO_PORTS, req.id, &[0x10, 0x27, 0x11, 0x27])
            .await;
        assert_eq!(open.await.unwrap(), (req.id, 10000, 10001));

        fake.reply(REPLY_CONFIG, 0x7a00_0001, &[1, 0xaa, 0xbb])
            .await;
        fake.reply(REPLY_AUDIO_STARTED, req.id, &5u32.to_le_bytes())
            .await;
        fake.reply(REPLY_AUDIO_DELAY, req.id, &120u32.to_le_bytes())
            .await;
        assert_eq!(
            events.recv().await.unwrap(),
            HostEvent::Config {
                plane: 0x7a00_0001,
                codec: Codec::H265,
                atom: vec![0xaa, 0xbb]
            }
        );
        assert_eq!(
            events.recv().await.unwrap(),
            HostEvent::AudioStarted {
                stream: req.id,
                first_sample: 5
            }
        );
        assert_eq!(
            events.recv().await.unwrap(),
            HostEvent::AudioDelay {
                stream: req.id,
                ms: 120
            }
        );
    }

    #[tokio::test]
    async fn an_unanswered_request_times_out_with_zeros() {
        let dir = TempDir::new();
        let gst = host(&dir);
        tokio::time::pause();
        let open = tokio::spawn({
            let gst = gst.clone();
            async move { gst.open_video_receiver(1, &[0; 32], true).await }
        });
        tokio::time::advance(ANSWER_TIMEOUT + Duration::from_millis(10)).await;
        assert_eq!(open.await.unwrap().0, 0);
        assert!(gst.state().port_waiters.is_empty());
    }

    #[tokio::test]
    async fn the_feed_is_opened_once() {
        let dir = TempDir::new();
        let gst = host(&dir);
        let mut fake = FakeHost::connect(&gst).await;
        let open = tokio::spawn({
            let gst = gst.clone();
            async move { gst.open_feed().await }
        });
        let req = fake.next().await;
        assert_eq!(req.op, OP_FEED_OPEN);
        fake.reply(REPLY_FEED, 0, &req.rest).await;
        let path = open.await.unwrap();
        assert!(path.ends_with("gst.sock.feed"));
        assert_eq!(gst.open_feed().await, path);
    }

    #[tokio::test]
    async fn a_missing_binary_makes_the_host_unavailable() {
        let dir = TempDir::new();
        let gst = host(&dir);
        gst.set_launch(Launch {
            bin: dir.0.join("no-such-host"),
            env: Vec::new(),
            log: dir.0.join("log/gst-host.log"),
            crash: dir.0.join("crash.log"),
        });
        assert_eq!(gst.open_feed().await, "");
        assert!(gst.state().unavailable);
    }
}
