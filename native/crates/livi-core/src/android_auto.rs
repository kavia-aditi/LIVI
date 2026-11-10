// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};

use livi_aa_stack::config::Codecs;
use livi_aa_stack::discovery::VideoCodec;
use livi_aa_stack::helper_sock::AaHelperSock;
use livi_aa_stack::manager::SessionId;
use livi_aa_stack::media::{AaMedia, AudioKind, AudioOutput};
use livi_core_proto::state::State;
use livi_host_proto::{PLANE_CLUSTER_RECV, PLANE_MAIN};
use livi_media::gst_host::{self, Codec, FeedTarget, GstHost};
use livi_media::planes::{ClusterPlanes, Crop, MainPlane, crop_for};
use tokio::sync::{broadcast, mpsc, watch};

use crate::devices;
use crate::hands_free::{Back, Io};

const SCO_MIC_SOCK: &str = "/tmp/aa-sco.mic";
const SCO_RATE: u32 = 8000;
const SCO_CHANNELS: u32 = 1;
const CALL_TAG: &str = "call";

pub fn codecs(probe: &serde_json::Value) -> Codecs {
    let hw = |codec: &str| probe[codec]["hw"].as_bool().unwrap_or(false);
    Codecs {
        hevc: crate::carplay::offers_hevc(probe),
        vp9: hw("vp9"),
        av1: hw("av1"),
    }
}

fn codec(c: VideoCodec) -> Codec {
    match c {
        VideoCodec::H264 => Codec::H264,
        VideoCodec::H265 => Codec::H265,
        VideoCodec::Vp9 => Codec::Vp9,
        VideoCodec::Av1 => Codec::Av1,
    }
}

pub fn to_stream(x: f64, y: f64, crop: Option<&Crop>) -> (f64, f64) {
    match crop {
        Some(c) if c.tier_w > 0.0 && c.tier_h > 0.0 => (
            (c.crop_l + x * c.vis_w) / c.tier_w,
            (c.crop_t + y * c.vis_h) / c.tier_h,
        ),
        _ => (x, y),
    }
}

type OutputKey = (AudioKind, u32, u32, String);

fn lock(audio: &Mutex<Audio>) -> MutexGuard<'_, Audio> {
    audio.lock().unwrap_or_else(|e| e.into_inner())
}

fn config_level(state: &State, kind: AudioKind) -> f64 {
    let cfg = &state.config;
    match kind {
        AudioKind::Media => cfg.audio_volume,
        AudioKind::Alert => cfg.nav_volume,
        AudioKind::Speech => cfg.voice_assistant_volume,
        AudioKind::Call => cfg.call_volume,
    }
}

#[derive(Default)]
struct Audio {
    outputs: Vec<AudioOutput>,
    opening: Vec<OutputKey>,
    keys: HashMap<u32, OutputKey>,
    levels: HashMap<AudioKind, f64>,
    call_active: bool,
}

#[derive(Default)]
struct Feeds {
    active: bool,
    video: BTreeMap<bool, u32>,
    codecs: BTreeMap<bool, Codec>,
    audio: BTreeMap<u32, u32>,
    native: Option<livi_aa_stack::manager::NativeContext>,
    geometry: Option<(u32, u32)>,
    native_owner: Option<livi_host_proto::native::Owner>,
}

#[derive(Default)]
struct Sessions {
    feeds: HashMap<SessionId, Feeds>,
    front: Option<SessionId>,
}

#[derive(Default)]
struct Call {
    up: bool,
    round: u64,
    tap: Option<u32>,
}

fn is_call(o: &AudioOutput) -> bool {
    o.tag.as_deref() == Some(CALL_TAG)
}

pub struct GstAaMedia {
    gst: GstHost,
    native_runtime: Option<u64>,
    native_selected: bool,
    plane: Arc<MainPlane>,
    clusters: Arc<ClusterPlanes>,
    state: watch::Receiver<State>,
    audio: Arc<Mutex<Audio>>,
    opened: broadcast::Sender<AudioOutput>,
    crop: watch::Sender<Option<Crop>>,
    call: Mutex<Call>,
    sessions: Mutex<Sessions>,
}

impl GstAaMedia {
    /// Shared ordinary-app renderer support, not Bluetooth authorization.
    pub fn new_shared(
        gst: GstHost,
        plane: Arc<MainPlane>,
        clusters: Arc<ClusterPlanes>,
        state: watch::Receiver<State>,
    ) -> Arc<Self> {
        let selected = gst.select_shared_native();
        let mut media = Self::new(gst, plane, clusters, state);
        let inner = Arc::get_mut(&mut media).expect("new media has one owner");
        inner.native_runtime = None;
        inner.native_selected = selected;
        media
    }

    pub fn new(
        gst: GstHost,
        plane: Arc<MainPlane>,
        clusters: Arc<ClusterPlanes>,
        state: watch::Receiver<State>,
    ) -> Arc<Self> {
        let native_runtime = isolated_runtime();
        let native_selected = native_runtime.is_some() && gst.select_native();
        Arc::new(Self {
            gst,
            native_runtime,
            native_selected,
            plane,
            clusters,
            state,
            audio: Arc::new(Mutex::new(Audio {
                call_active: true,
                ..Default::default()
            })),
            opened: broadcast::channel(16).0,
            crop: watch::channel(None).0,
            call: Mutex::new(Call::default()),
            sessions: Mutex::new(Sessions::default()),
        })
    }

    fn sessions(&self) -> MutexGuard<'_, Sessions> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn feed(&self, session: SessionId, target: FeedTarget) -> u32 {
        let mut sessions = self.sessions();
        let feeds = sessions.feeds.entry(session).or_default();
        let known = match target {
            FeedTarget::Plane(plane) => feeds.video.get(&(plane == PLANE_CLUSTER_RECV)),
            FeedTarget::Audio(stream) => feeds.audio.get(&stream),
        };
        if let Some(id) = known {
            return *id;
        }
        if feeds.native.is_some() && matches!(target, FeedTarget::Plane(_)) {
            if target != FeedTarget::Plane(PLANE_MAIN) { return 0; }
            let id = self.gst.allocate_native_feed();
            feeds.video.insert(false, id);
            return id;
        }
        let id = self.gst.open_feed_route(target, feeds.active);
        match target {
            FeedTarget::Plane(plane) => feeds.video.insert(plane == PLANE_CLUSTER_RECV, id),
            FeedTarget::Audio(stream) => feeds.audio.insert(stream, id),
        };
        id
    }

    fn set_call_active(&self, active: bool) {
        let streams: Vec<u32> = {
            let mut a = self.audio();
            a.call_active = active;
            a.outputs
                .iter()
                .filter(|o| is_call(o))
                .map(|o| o.stream)
                .collect()
        };
        for stream in streams {
            self.gst.set_audio_active(stream, active);
        }
    }

    pub fn crop(&self) -> watch::Receiver<Option<Crop>> {
        self.crop.subscribe()
    }

    fn call(&self) -> MutexGuard<'_, Call> {
        self.call.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn start_call(self: &Arc<Self>, helper: &AaHelperSock) {
        let round = {
            let mut call = self.call();
            if call.up {
                return;
            }
            call.up = true;
            call.round += 1;
            call.round
        };
        println!("[core] call audio up");
        let mut opened = self.opened.subscribe();
        let open = self
            .audio()
            .outputs
            .iter()
            .find(|o| is_call(o))
            .map(|o| o.stream);
        let (me, helper) = (self.clone(), helper.clone());
        tokio::spawn(async move {
            let stream = match open {
                Some(stream) => stream,
                None => loop {
                    match opened.recv().await {
                        Ok(o) if is_call(&o) => break o.stream,
                        Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => return,
                    }
                },
            };
            let feed = me.feed_path().await;
            if !me.call_is(round) {
                return;
            }
            if feed.is_empty() {
                eprintln!("[core] host has no media feed, the caller stays silent");
                return;
            }
            if let Err(e) = helper.set_sco_sink(Some((&feed, stream))).await {
                eprintln!("[core] helper did not take the call sink: {e}");
            }
        });
        if open.is_none() {
            self.prime_audio(AudioKind::Call, SCO_RATE, SCO_CHANNELS, CALL_TAG);
        }
        let device = self
            .state
            .borrow()
            .config
            .audio_input_device
            .clone()
            .unwrap_or_default();
        let tap = self.open_mic_tap(SCO_MIC_SOCK, SCO_RATE, SCO_CHANNELS, &device);
        if tap.is_none() {
            eprintln!("[core] no microphone tap, the caller hears nothing");
        }
        self.call().tap = tap;
    }

    pub fn stop_call(&self, helper: &AaHelperSock) {
        let tap = {
            let mut call = self.call();
            if !call.up {
                return;
            }
            call.up = false;
            call.tap.take()
        };
        println!("[core] call audio down");
        if let Some(tap) = tap {
            self.close_mic_tap(tap);
        }
        let helper = helper.clone();
        tokio::spawn(async move {
            let _ = helper.set_sco_sink(None).await;
        });
        let streams: Vec<u32> = {
            let mut a = self.audio();
            let gone: Vec<u32> = a
                .outputs
                .iter()
                .filter(|o| is_call(o))
                .map(|o| o.stream)
                .collect();
            a.outputs.retain(|o| !is_call(o));
            a.keys.retain(|stream, _| !gone.contains(stream));
            gone
        };
        for stream in streams {
            self.gst.close_audio(stream);
        }
    }

    fn call_is(&self, round: u64) -> bool {
        let call = self.call();
        call.up && call.round == round
    }

    fn audio(&self) -> MutexGuard<'_, Audio> {
        lock(&self.audio)
    }

    fn prepare(&self, cluster: bool, codec: Codec) {
        if cluster {
            self.clusters.prepare(codec, Vec::new());
        } else {
            self.plane.prepare(codec, Vec::new());
        }
    }

    fn prepare_native(&self, session: SessionId) {
        let route = {
            let mut sessions = self.sessions();
            let Some(feeds) = sessions.feeds.get_mut(&session).filter(|f| f.active) else { return };
            let Some(context) = feeds.native else { return };
            let Some(codec) = feeds.codecs.get(&false).copied() else { return };
            let Some(feed) = feeds.video.get(&false).copied() else { return };
            if feeds.native_owner.is_some() { return; }
            let Some(owner) = self.gst.native_owner(context.runtime, context.attempt, context.generation, session) else { return };
            feeds.native_owner = Some(owner);
            (owner, feed, codec, feeds.geometry)
        };
        self.plane.prepare_native(route.0, route.1, route.2);
        if let Some(tier) = route.3 { self.place_main(tier); }
    }

    fn place_main(&self, tier: (u32, u32)) {
        let cfg = self.state.borrow().config.clone();
        let crop = crop_for(tier, (cfg.projection_width, cfg.projection_height));
        self.plane.set_crop(crop);
        self.crop.send_replace(crop.or(Some(Crop {
            crop_l: 0.0,
            crop_t: 0.0,
            vis_w: f64::from(tier.0),
            vis_h: f64::from(tier.1),
            tier_w: f64::from(tier.0),
            tier_h: f64::from(tier.1),
        })));
    }
}

impl AaMedia for GstAaMedia {
    fn native_runtime(&self) -> Option<u64> { self.native_runtime }

    fn register_native(&self, session: SessionId, context: livi_aa_stack::manager::NativeContext) -> bool {
        if !self.native_selected || self.native_runtime.is_some_and(|runtime| runtime != context.runtime)
            || session == 0 || context.runtime == 0 || context.attempt == 0 || context.generation == 0 {
            return false;
        }
        let mut sessions = self.sessions();
        if sessions.feeds.contains_key(&session) { return false; }
        sessions.feeds.insert(session, Feeds { native: Some(context), ..Default::default() });
        true
    }

    fn native_context(&self, session: SessionId) -> Option<livi_aa_stack::manager::NativeContext> {
        self.sessions().feeds.get(&session).and_then(|feeds| feeds.native)
    }

    fn video_started_for(&self, session: SessionId, cluster: bool, width: u32, height: u32) {
        let legacy_front = {
            let mut sessions = self.sessions();
            let front = sessions.front == Some(session);
            let Some(feeds) = sessions.feeds.get_mut(&session) else { return };
            if !cluster && width != 0 && height != 0 { feeds.geometry = Some((width, height)); }
            front && feeds.native.is_none()
        };
        if legacy_front { self.video_started(cluster, width, height); }
        if !legacy_front && !cluster && self.sessions().front == Some(session) {
            self.prepare_native(session);
            if width != 0 && height != 0 { self.place_main((width, height)); }
        }
    }

    async fn feed_path(&self) -> String {
        self.gst.open_feed().await
    }

    async fn video_feed_path(&self, session: SessionId) -> String {
        if self.native_context(session).is_some() { self.gst.open_native_feed().await }
        else { self.feed_path().await }
    }

    fn video_feed(&self, session: SessionId, cluster: bool) -> u32 {
        let feed = self.feed(
            session,
            FeedTarget::Plane(if cluster {
                PLANE_CLUSTER_RECV
            } else {
                PLANE_MAIN
            }),
        );
        if self.native_context(session).is_some() { self.prepare_native(session); }
        feed
    }

    fn prime_video(&self, session: SessionId, cluster: bool, video: VideoCodec) {
        if self.native_context(session).is_some() {
            if cluster { return; }
            self.sessions().feeds.get_mut(&session).unwrap().codecs.insert(false, codec(video));
            self.prepare_native(session);
            return;
        }
        let active = {
            let mut sessions = self.sessions();
            let feeds = sessions.feeds.entry(session).or_default();
            feeds.codecs.insert(cluster, codec(video));
            feeds.active
        };
        if active {
            self.prepare(cluster, codec(video));
        }
    }

    fn video_started(&self, cluster: bool, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        if !cluster {
            self.place_main((width, height));
            return;
        }
        let cfg = self.state.borrow().config.clone();
        if crate::carplay::cluster_displayed(&cfg) {
            self.clusters.set_crop(crop_for(
                (width, height),
                (cfg.cluster_width, cfg.cluster_height),
            ));
        }
    }

    fn audio_feed(&self, session: SessionId, stream: u32) -> u32 {
        self.feed(session, FeedTarget::Audio(stream))
    }

    fn set_active(&self, session: SessionId, active: bool) {
        if self.native_context(session).is_some() {
            let (owner, audio) = {
                let mut sessions = self.sessions();
                let feeds = sessions.feeds.get_mut(&session).unwrap();
                feeds.active = active;
                let owner = feeds.native_owner;
                let audio: Vec<_> = feeds.audio.values().copied().collect();
                if active { sessions.front = Some(session); }
                else if sessions.front == Some(session) { sessions.front = None; }
                (owner, audio)
            };
            for feed in audio { self.gst.set_active_feeder(feed, active); }
            self.set_call_active(self.sessions().front.is_some());
            if active { self.prepare_native(session); }
            if let Some(owner) = owner { let _ = self.plane.native_active(owner, active); }
            return;
        }
        let (feeds, codecs, front) = {
            let mut sessions = self.sessions();
            let feeds = sessions.feeds.entry(session).or_default();
            feeds.active = active;
            let ids: Vec<u32> = feeds
                .video
                .values()
                .chain(feeds.audio.values())
                .copied()
                .collect();
            let codecs = feeds.codecs.clone();
            if active {
                sessions.front = Some(session);
            } else if sessions.front == Some(session) {
                sessions.front = None;
            }
            (ids, codecs, sessions.front.is_some())
        };
        for feed in feeds {
            self.gst.set_active_feeder(feed, active);
        }
        self.set_call_active(front);
        if !active {
            return;
        }
        for (cluster, codec) in codecs {
            self.prepare(cluster, codec);
        }
        let crop = *self.crop.borrow();
        if let Some(c) = crop {
            self.place_main((c.tier_w as u32, c.tier_h as u32));
        }
    }

    fn release(&self, session: SessionId) {
        let feeds = {
            let mut sessions = self.sessions();
            if sessions.front == Some(session) {
                sessions.front = None;
            }
            sessions.feeds.remove(&session)
        };
        if let Some(native) = feeds.as_ref().filter(|feeds| feeds.native.is_some()) {
            if let Some(owner) = native.native_owner {
                if self.plane.native_owner() == Some(owner) { self.plane.dispose(); }
                else if let Some(feed) = native.video.get(&false) {
                    let _ = self.gst.send_native(*feed, &livi_host_proto::native::Packet::Stop(owner));
                }
            }
            for feed in native.audio.values() { self.gst.close_feed_route(*feed); }
            return;
        }
        for feed in feeds
            .into_iter()
            .flat_map(|f| f.video.into_values().chain(f.audio.into_values()))
        {
            self.gst.close_feed_route(feed);
        }
    }

    fn audio_outputs(&self) -> Vec<AudioOutput> {
        self.audio().outputs.clone()
    }

    fn audio_output_opened(&self) -> broadcast::Receiver<AudioOutput> {
        self.opened.subscribe()
    }

    fn prime_audio(&self, kind: AudioKind, sample_rate: u32, channels: u32, tag: &str) {
        if sample_rate == 0 || channels == 0 {
            return;
        }
        let key: OutputKey = (kind, sample_rate, channels, tag.to_string());
        {
            let mut a = self.audio();
            if a.opening.contains(&key) || a.keys.values().any(|k| *k == key) {
                return;
            }
            a.opening.push(key.clone());
        }
        let opts = gst_host::AudioOpts {
            codec: gst_host::AudioCodec::PcmLe,
            payload_type: 0,
            clock_rate: sample_rate,
            channels: channels.min(u32::from(u8::MAX)) as u8,
            latency_ms: 0,
            realtime: false,
            fed: true,
            device: self
                .state
                .borrow()
                .config
                .audio_output_device
                .clone()
                .unwrap_or_default(),
        };
        let fallback = config_level(&self.state.borrow(), kind);
        let (gst, audio, opened) = (self.gst.clone(), self.audio.clone(), self.opened.clone());
        tokio::spawn(async move {
            let (stream, _, _) = gst.open_audio(&[0; 32], &opts).await;
            let (output, level, active) = {
                let mut a = lock(&audio);
                a.opening.retain(|k| *k != key);
                if stream == 0 {
                    return;
                }
                let output = AudioOutput {
                    kind,
                    stream,
                    tag: Some(key.3.clone()),
                };
                let active = !is_call(&output) || a.call_active;
                a.keys.insert(stream, key);
                a.outputs.push(output.clone());
                (
                    output,
                    a.levels.get(&kind).copied().unwrap_or(fallback),
                    active,
                )
            };
            gst.set_audio_active(stream, active);
            gst.set_audio_volume(stream, level, 0);
            let _ = opened.send(output);
        });
    }

    fn set_host_volume(&self, kind: AudioKind, level: f64, ramp_ms: u32) {
        let streams: Vec<u32> = {
            let mut a = self.audio();
            a.levels.insert(kind, level);
            a.outputs
                .iter()
                .filter(|o| o.kind == kind)
                .map(|o| o.stream)
                .collect()
        };
        for stream in streams {
            self.gst.set_audio_volume(stream, level, ramp_ms);
        }
    }

    fn open_mic_tap(
        &self,
        path: &str,
        sample_rate: u32,
        channels: u32,
        device: &str,
    ) -> Option<u32> {
        let channels = channels.min(u32::from(u8::MAX)) as u8;
        Some(self.gst.open_mic_tap(path, sample_rate, channels, device))
    }

    fn close_mic_tap(&self, id: u32) {
        self.gst.close_mic_tap(id);
    }
}

fn isolated_runtime() -> Option<u64> {
    let dir = crate::native_bt_dev::dir()?;
    livi_host_proto::isolated_runtime::Layout::from_transport(
        &dir.join("livi"), unsafe { libc::geteuid() }, libc::O_NOFOLLOW,
        std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
        std::env::var_os("LIVI_NATIVE_BT_DEV_DIR").as_deref(), None,
    ).map(|layout| layout.runtime)
}

pub async fn hands_free_io(
    mut asks: mpsc::UnboundedReceiver<Io>,
    media: Arc<GstAaMedia>,
    helper: AaHelperSock,
    back: mpsc::UnboundedSender<Back>,
) {
    while let Some(ask) = asks.recv().await {
        match ask {
            Io::Call(true) => media.start_call(&helper),
            Io::Call(false) => media.stop_call(&helper),
            Io::WiredPhones(ids) => {
                let helper = helper.clone();
                tokio::spawn(async move {
                    if let Err(e) = helper.set_wired_phones(&ids).await {
                        eprintln!("[core] wired phones not sent: {e}");
                    }
                });
            }
            Io::Nudge(mac) => {
                tokio::spawn(devices::nudge_hfp(mac));
            }
            Io::FindPhones => {
                let back = back.clone();
                tokio::spawn(async move {
                    let _ = back.send(Back::Phones(devices::connected_phones().await));
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use livi_core_proto::state::{Front, PerScreen};
    use livi_host_proto::{Framer, encode_reply};
    use livi_media::compositor::CompositorControl;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;

    use livi_host_proto::ops::{
        OP_AUDIO_ACTIVE, OP_AUDIO_OPEN, OP_AUDIO_STOP, OP_AUDIO_VOLUME, OP_CREATE, OP_FEED_CLOSE,
        OP_FEED_OPEN, OP_FEED_ROUTE, OP_SET_ACTIVE, OP_TAP_OPEN, OP_TAP_STOP, REPLY_AUDIO_PORTS,
        REPLY_FEED,
    };

    use super::*;
    use crate::config_file::defaults;
    use crate::config_file::tests::TempDir;

    #[cfg(all(target_os = "macos", feature = "native-bt-dev"))]
    #[test]
    fn isolated_layout_all_components_offline() {
        use std::os::unix::fs::DirBuilderExt;
        let Some(helper) = std::env::var_os("LIVI_OFFLINE_HELPER_HARNESS") else { return };
        let ui = std::env::var_os("LIVI_OFFLINE_UI_HARNESS").expect("UI test harness");
        let gst = std::env::var_os("LIVI_OFFLINE_GST_HARNESS").expect("media test harness");
        let root = std::path::PathBuf::from(format!(
            "/tmp/livi-bt-dev-shared-{}", std::process::id()));
        std::fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let core = std::env::current_exe().unwrap().into_os_string();
        let results: Vec<_> = [
            (helper, "isolated_helper::tests::offline_shared_layout_publish_child"),
            (core, "android_auto::tests::isolated_layout_core_media_child"),
            (gst, "process::isolated_layout_tests::media_guard_layout_child"),
            (ui, "native_bluetooth::tests::isolated_ui_shared_layout_child"),
        ].into_iter().map(|(bin, test)| {
            let output = std::process::Command::new(bin)
                .args(["--exact", test, "--nocapture"])
                .env("LIVI_OFFLINE_LAYOUT_CHILD", "1").env("LIVI_OFFLINE_SHARED_LAYOUT", "1")
                .env("XDG_RUNTIME_DIR", &root).env("LIVI_NATIVE_BT_DEV_DIR", &root)
                .env(livi_core_proto::LIFELINE_ENV, "1")
                .env_remove("LIVI_CORE_SOCKET").env_remove("LIVI_UI_STATE")
                .env_remove("LIVI_OFFLINE_HELPER_HARNESS")
                .output();
            (test, output)
        }).collect();
        std::fs::remove_dir_all(root).unwrap();
        for (test, output) in results {
            let output = output.unwrap();
            assert!(output.status.success(), "{test}: {}\n{}",
                String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"), "{test} did not run");
            println!("shared runtime 731: {test} passed");
        }
    }

    #[cfg(all(target_os = "macos", feature = "native-bt-dev"))]
    #[test]
    fn isolated_layout_integrates_core_and_media_offline() {
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let root = std::path::PathBuf::from(format!(
            "/tmp/livi-bt-dev-integration-core-{}", std::process::id()));
        std::fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        std::fs::DirBuilder::new().mode(0o700).create(root.join("livi")).unwrap();
        let mut marker = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600)
            .open(root.join("native-runtime")).unwrap();
        writeln!(marker, "731").unwrap();
        drop(marker);
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "android_auto::tests::isolated_layout_core_media_child", "--nocapture"])
            .env("LIVI_OFFLINE_LAYOUT_CHILD", "1")
            .env("XDG_RUNTIME_DIR", &root).env("LIVI_NATIVE_BT_DEV_DIR", &root)
            .env_remove("LIVI_CORE_SOCKET").output().unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stdout));
    }

    #[cfg(all(target_os = "macos", feature = "native-bt-dev"))]
    #[tokio::test]
    async fn isolated_layout_core_media_child() {
        if std::env::var("LIVI_OFFLINE_LAYOUT_CHILD").as_deref() != Ok("1") { return; }
        let root = crate::native_bt_dev::dir().unwrap();
        crate::native_bt_dev::validate().unwrap();
        let paths = crate::paths::Paths::from_env(unsafe { libc::geteuid() }).unwrap();
        assert_eq!(paths.runtime, root.join("livi"));
        assert_eq!(paths.socket(), root.join("livi/core.sock"));
        assert_eq!(isolated_runtime(), Some(731));
        let sock = paths.runtime.join("gst.sock");
        let gst = GstHost::new(sock.clone());
        let ctrl = CompositorControl::connect(paths.runtime.join("compositor.ctrl"));
        let plane = MainPlane::new(gst.clone(), ctrl.clone());
        let clusters = ClusterPlanes::new(gst.clone(), ctrl);
        let (_tx, state) = watch::channel(state());
        let media = GstAaMedia::new(gst, plane, clusters, state);
        assert_eq!(media.native_runtime(), Some(731));
        assert!(media.register_native(4, livi_aa_stack::manager::NativeContext {
            runtime: 731, attempt: 2, generation: 3,
        }));
        assert!(!media.register_native(5, livi_aa_stack::manager::NativeContext {
            runtime: 732, attempt: 2, generation: 3,
        }));
        let open = tokio::spawn({ let media = media.clone(); async move { media.feed_path().await } });
        let mut host = FakeHost::connect(&sock).await;
        assert_eq!(host.next().await.0, livi_host_proto::ops::OP_NATIVE_CAPABILITIES);
        assert_eq!(host.next().await.0, OP_FEED_OPEN);
        host.stream.write_all(&encode_reply(REPLY_FEED, 0, b"offline-only")).await.unwrap();
        assert_eq!(open.await.unwrap(), "offline-only");
        let helper = include_str!("../../livi-native-bt-dev/src/isolated_helper.rs");
        assert!(helper.contains(r#"directory.join("livi")"#));
        assert!(helper.contains(r#"directory.join("native-runtime")"#));
        assert!(helper.find("prepare_transport(&options.directory)?").unwrap()
            < helper.find("let _marker = publish_marker(&options.directory").unwrap());
    }

    fn route(kind: u8, target: u32, active: bool) -> Vec<u8> {
        let mut v = vec![kind];
        v.extend_from_slice(&target.to_le_bytes());
        v.push(u8::from(active));
        v
    }

    struct FakeHost {
        stream: UnixStream,
        framer: Framer,
    }

    impl FakeHost {
        async fn connect(sock: &std::path::Path) -> Self {
            for _ in 0..200 {
                if let Ok(stream) = UnixStream::connect(sock).await {
                    return Self {
                        stream,
                        framer: Framer::new(),
                    };
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("core never opened the gst-host socket");
        }

        async fn next(&mut self) -> (u8, u32, Vec<u8>) {
            let mut buf = [0u8; 512];
            loop {
                if let Some(m) = self.framer.next_message() {
                    return (m.op, m.id, m.rest);
                }
                let n = tokio::time::timeout(Duration::from_secs(5), self.stream.read(&mut buf))
                    .await
                    .unwrap()
                    .unwrap();
                assert!(n > 0, "core closed the socket");
                self.framer.push(&buf[..n]);
            }
        }
    }

    fn state() -> State {
        let mut config = defaults();
        config.projection_width = 1280;
        config.projection_height = 600;
        config.voice_assistant_volume = 0.5;
        State {
            front: PerScreen {
                main: Front::Livi,
                cluster: Front::Livi,
                aux: Front::Livi,
            },
            sessions: Default::default(),
            now_playing: Default::default(),
            telemetry: Default::default(),
            navigation: Default::default(),
            system: Default::default(),
            devices: Default::default(),
            update: Default::default(),
            cluster: Default::default(),
            dab: Default::default(),
            cameras: Vec::new(),
            mfi: Default::default(),
            config,
        }
    }

    fn volume(level: f64, ramp_ms: u32) -> Vec<u8> {
        let mut v = level.to_le_bytes().to_vec();
        v.extend_from_slice(&ramp_ms.to_le_bytes());
        v
    }

    #[test]
    fn a_screen_point_lands_on_the_stream_inside_its_margins() {
        let crop = Crop {
            crop_l: 40.0,
            crop_t: 60.0,
            vis_w: 1200.0,
            vis_h: 600.0,
            tier_w: 1280.0,
            tier_h: 720.0,
        };
        assert_eq!(
            to_stream(0.0, 0.0, Some(&crop)),
            (40.0 / 1280.0, 60.0 / 720.0)
        );
        assert_eq!(
            to_stream(1.0, 1.0, Some(&crop)),
            (1240.0 / 1280.0, 660.0 / 720.0)
        );
        assert_eq!(to_stream(0.3, 0.6, None), (0.3, 0.6));
        let empty = Crop {
            tier_w: 0.0,
            ..crop
        };
        assert_eq!(to_stream(0.3, 0.6, Some(&empty)), (0.3, 0.6));
    }

    #[test]
    fn the_phone_is_offered_what_the_host_decodes() {
        let probe = json!({
            "h264": { "hw": true, "sw": true },
            "h265": { "hw": false, "sw": true },
            "vp9": { "hw": true, "sw": true },
            "av1": { "hw": false, "sw": true }
        });
        assert_eq!(
            codecs(&probe),
            Codecs {
                hevc: false,
                vp9: true,
                av1: false
            }
        );
        assert_eq!(codecs(&json!({})), Codecs::default());
    }

    #[tokio::test]
    async fn a_channel_opens_one_fed_stream_at_its_level_and_follows_the_session() {
        let dir = TempDir::new();
        let sock = dir.0.join("gst.sock");
        let gst = GstHost::new(sock.clone());
        let ctrl = CompositorControl::connect(dir.0.join("ctrl"));
        let plane = MainPlane::new(gst.clone(), ctrl.clone());
        let clusters = ClusterPlanes::new(gst.clone(), ctrl);
        let (_state_tx, state_rx) = watch::channel(state());
        let media = GstAaMedia::new(gst, plane, clusters, state_rx);
        let mut opened = media.audio_output_opened();

        media.prime_audio(AudioKind::Speech, 16000, 1, "speech");
        media.prime_audio(AudioKind::Speech, 16000, 1, "speech");
        media.prime_audio(AudioKind::Media, 0, 2, "media");
        let mut host = FakeHost::connect(&sock).await;
        let (op, id, _) = host.next().await;
        assert_eq!(op, OP_AUDIO_OPEN);
        host.stream
            .write_all(&encode_reply(REPLY_AUDIO_PORTS, id, &[0, 0, 0, 0]))
            .await
            .unwrap();
        assert_eq!(host.next().await, (OP_AUDIO_ACTIVE, id, vec![1]));
        assert_eq!(host.next().await, (OP_AUDIO_VOLUME, id, volume(0.5, 0)));
        let output = tokio::time::timeout(Duration::from_secs(5), opened.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            output,
            AudioOutput {
                kind: AudioKind::Speech,
                stream: id,
                tag: Some("speech".into())
            }
        );
        assert_eq!(media.audio_outputs(), [output]);

        media.set_host_volume(AudioKind::Speech, 0.3, 200);
        media.set_host_volume(AudioKind::Media, 0.9, 0);
        assert_eq!(host.next().await, (OP_AUDIO_VOLUME, id, volume(0.3, 200)));
        let crop = media.crop();
        media.video_started(false, 0, 720);
        assert!(crop.borrow().is_none());
        media.video_started(false, 1280, 720);
        assert_eq!(*crop.borrow(), crop_for((1280, 720), (1280, 600)));
        assert!(media.open_mic_tap("/tmp/mic", 16000, 1, "").is_some());
    }

    #[tokio::test]
    async fn every_session_switches_only_its_own_feeds() {
        let dir = TempDir::new();
        let sock = dir.0.join("gst.sock");
        let gst = GstHost::new(sock.clone());
        let ctrl = CompositorControl::connect(dir.0.join("ctrl"));
        let plane = MainPlane::new(gst.clone(), ctrl.clone());
        let clusters = ClusterPlanes::new(gst.clone(), ctrl);
        let (_state_tx, state_rx) = watch::channel(state());
        let media = GstAaMedia::new(gst, plane, clusters, state_rx);

        let wifi = media.video_feed(1, false);
        let mut host = FakeHost::connect(&sock).await;
        assert_eq!(
            host.next().await,
            (OP_FEED_ROUTE, wifi, route(0, PLANE_MAIN, false))
        );
        let wifi_speech = media.audio_feed(1, 0x7b00_0040);
        assert_eq!(
            host.next().await,
            (OP_FEED_ROUTE, wifi_speech, route(1, 0x7b00_0040, false))
        );
        assert_eq!(media.video_feed(1, false), wifi);
        media.set_active(1, true);
        assert_eq!(host.next().await, (OP_SET_ACTIVE, wifi, vec![1]));
        assert_eq!(host.next().await, (OP_SET_ACTIVE, wifi_speech, vec![1]));

        media.set_active(2, true);
        let usb = media.video_feed(2, false);
        assert_eq!(
            host.next().await,
            (OP_FEED_ROUTE, usb, route(0, PLANE_MAIN, true))
        );
        let usb_cluster = media.video_feed(2, true);
        assert_eq!(
            host.next().await,
            (
                OP_FEED_ROUTE,
                usb_cluster,
                route(0, PLANE_CLUSTER_RECV, true)
            )
        );

        media.set_active(1, false);
        assert_eq!(host.next().await, (OP_SET_ACTIVE, wifi, vec![0]));
        assert_eq!(host.next().await, (OP_SET_ACTIVE, wifi_speech, vec![0]));
        media.release(1);
        assert_eq!(host.next().await, (OP_FEED_CLOSE, wifi, vec![]));
        assert_eq!(host.next().await, (OP_FEED_CLOSE, wifi_speech, vec![]));
        media.release(1);
        media.release(2);
        assert_eq!(host.next().await, (OP_FEED_CLOSE, usb, vec![]));
        assert_eq!(host.next().await, (OP_FEED_CLOSE, usb_cluster, vec![]));
    }

    #[tokio::test]
    async fn native_media_negotiates_before_create_and_releases_only_its_owner() {
        use livi_host_proto::native::{Capabilities, Packet, REQUIRED_CAPABILITIES};
        use livi_host_proto::ops::*;
        use livi_aa_stack::manager::NativeContext;
        let dir = TempDir::new();
        let sock = dir.0.join("gst.sock");
        let gst = GstHost::new(sock.clone());
        assert!(gst.select_native());
        let ctrl = CompositorControl::connect(dir.0.join("ctrl"));
        let plane = MainPlane::new(gst.clone(), ctrl.clone());
        let clusters = ClusterPlanes::new(gst.clone(), ctrl);
        let (_state_tx, state_rx) = watch::channel(state());
        let mut media = GstAaMedia::new(gst, plane.clone(), clusters, state_rx);
        let selected = Arc::get_mut(&mut media).unwrap();
        selected.native_runtime = Some(1);
        selected.native_selected = true;
        let context = NativeContext { runtime: 1, attempt: 2, generation: 3 };
        assert!(media.register_native(4, context));
        assert!(!media.register_native(4, context));
        assert!(!media.register_native(5, NativeContext { runtime: 9, ..context }));
        media.set_active(4, true);
        media.prime_video(4, false, VideoCodec::H264);
        let feed = media.video_feed(4, false);
        assert!(plane.native_owner().is_none(), "capabilities are not ready yet");
        let open = tokio::spawn({
            let media = media.clone();
            async move { media.feed_path().await }
        });
        let mut host = FakeHost::connect(&sock).await;
        assert_eq!(host.next().await.0, OP_NATIVE_CAPABILITIES);
        host.stream.write_all(&encode_reply(REPLY_NATIVE_CAPABILITIES, 0,
            &Packet::Capabilities(Capabilities { bits: REQUIRED_CAPABILITIES }).encode().unwrap()))
            .await.unwrap();
        assert_eq!(host.next().await.0, OP_FEED_OPEN);
        host.stream.write_all(&encode_reply(REPLY_FEED, 0, b"/offline/feed")).await.unwrap();
        assert_eq!(open.await.unwrap(), "/offline/feed");
        media.video_started_for(4, false, 1280, 720);
        let (op, id, body) = host.next().await;
        assert_eq!((op, id), (OP_NATIVE_CREATE, feed));
        let Packet::Create(create) = parse_native(op, false, &body).unwrap() else { panic!() };
        assert_eq!((create.owner.runtime, create.owner.attempt, create.owner.generation,
            create.owner.session), (1, 2, 3, 4));
        assert_eq!(host.next().await.0, OP_NATIVE_ROUTE);
        assert_eq!(host.next().await.0, OP_NATIVE_ACTIVE);
        assert!(media.register_native(6, NativeContext { attempt: 7, ..context }));
        media.release(6);
        assert_eq!(plane.native_owner(), Some(create.owner));
        media.release(4);
        assert_eq!(host.next().await.0, OP_NATIVE_STOP);
        assert!(plane.native_owner().is_none());
    }

    #[tokio::test]
    async fn shared_native_audio_and_mic_use_existing_routes_without_releasing_legacy() {
        use livi_host_proto::ops::*;
        use livi_aa_stack::manager::NativeContext;
        let dir = TempDir::new();
        let sock = dir.0.join("gst.sock");
        let gst = GstHost::new(sock.clone());
        let ctrl = CompositorControl::connect(dir.0.join("ctrl"));
        let plane = MainPlane::new(gst.clone(), ctrl.clone());
        let clusters = ClusterPlanes::new(gst.clone(), ctrl);
        let (_state_tx, state_rx) = watch::channel(state());
        let media = GstAaMedia::new_shared(gst, plane, clusters, state_rx);
        assert!(media.register_native(4, NativeContext { runtime: 1, attempt: 2, generation: 3 }));
        let native = media.audio_feed(4, 55);
        let legacy = media.audio_feed(7, 56);
        let mut host = FakeHost::connect(&sock).await;
        assert_eq!(host.next().await, (OP_FEED_ROUTE, native, route(1, 55, false)));
        assert_eq!(host.next().await, (OP_FEED_ROUTE, legacy, route(1, 56, false)));
        media.set_active(4, true);
        assert_eq!(host.next().await, (OP_SET_ACTIVE, native, vec![1]));
        let tap = media.open_mic_tap("/offline/native.mic", 16000, 1, "fake-mic").unwrap();
        let (op, id, body) = host.next().await;
        assert_eq!((op, id), (OP_TAP_OPEN, tap));
        assert!(body.ends_with(b"fake-mic/offline/native.mic"));
        media.close_mic_tap(tap);
        assert_eq!(host.next().await, (OP_TAP_STOP, tap, vec![]));
        media.set_active(4, false);
        assert_eq!(host.next().await, (OP_SET_ACTIVE, native, vec![0]));
        media.release(4);
        assert_eq!(host.next().await, (OP_FEED_CLOSE, native, vec![]));
        media.release(7);
        assert_eq!(host.next().await, (OP_FEED_CLOSE, legacy, vec![]));
    }

    #[tokio::test]
    async fn a_session_in_the_background_leaves_the_plane_to_the_front() {
        let dir = TempDir::new();
        let sock = dir.0.join("gst.sock");
        let gst = GstHost::new(sock.clone());
        let ctrl = CompositorControl::connect(dir.0.join("ctrl"));
        let plane = MainPlane::new(gst.clone(), ctrl.clone());
        let clusters = ClusterPlanes::new(gst.clone(), ctrl);
        let (_state_tx, state_rx) = watch::channel(state());
        let media = GstAaMedia::new(gst, plane, clusters, state_rx);

        let back = media.video_feed(2, false);
        let mut host = FakeHost::connect(&sock).await;
        assert_eq!(
            host.next().await,
            (OP_FEED_ROUTE, back, route(0, PLANE_MAIN, false))
        );
        media.prime_video(2, false, VideoCodec::H265);
        tokio::time::sleep(Duration::from_millis(100)).await;
        media.release(2);
        assert_eq!(host.next().await, (OP_FEED_CLOSE, back, vec![]));

        media.set_active(3, true);
        media.prime_video(3, false, VideoCodec::H265);
        let (op, id, rest) = host.next().await;
        assert_eq!((op, id), (OP_CREATE, PLANE_MAIN));
        assert_eq!(rest, [&[4u8][..], b"h265"].concat());
    }

    async fn helper_line(listener: &tokio::net::UnixListener) -> String {
        let (sock, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let (read, mut write) = sock.into_split();
        let mut line = String::new();
        tokio::io::AsyncBufReadExt::read_line(&mut tokio::io::BufReader::new(read), &mut line)
            .await
            .unwrap();
        write.write_all(b"{\"ok\":true}\n").await.unwrap();
        line.trim_end().to_string()
    }

    #[tokio::test]
    async fn a_call_gets_a_stream_the_helper_feeds_and_a_microphone_tap() {
        let dir = TempDir::new();
        let sock = dir.0.join("gst.sock");
        let gst = GstHost::new(sock.clone());
        let ctrl = CompositorControl::connect(dir.0.join("ctrl"));
        let plane = MainPlane::new(gst.clone(), ctrl.clone());
        let clusters = ClusterPlanes::new(gst.clone(), ctrl);
        let mut with_mic = state();
        with_mic.config.audio_input_device = Some("mic1".into());
        let (_state_tx, state_rx) = watch::channel(with_mic);
        let media = GstAaMedia::new(gst, plane, clusters, state_rx);
        let helper_path = dir.0.join("aa.sock");
        let listener = tokio::net::UnixListener::bind(&helper_path).unwrap();
        let helper = AaHelperSock::new(&helper_path);

        media.start_call(&helper);
        media.start_call(&helper);
        let mut host = FakeHost::connect(&sock).await;
        let mut first = vec![host.next().await, host.next().await];
        first.sort_by_key(|(op, _, _)| *op);
        let [(open, stream, _), (tap_op, tap, tap_body)] = first.as_slice() else {
            panic!()
        };
        assert_eq!((*open, *tap_op), (OP_AUDIO_OPEN, OP_TAP_OPEN));
        assert!(tap_body.ends_with(b"mic1/tmp/aa-sco.mic"));
        let stream = *stream;
        host.stream
            .write_all(&encode_reply(REPLY_AUDIO_PORTS, stream, &[0; 4]))
            .await
            .unwrap();
        assert_eq!(host.next().await, (OP_AUDIO_ACTIVE, stream, vec![1]));
        assert_eq!(host.next().await.0, OP_AUDIO_VOLUME);
        let (op, _, _) = host.next().await;
        assert_eq!(op, OP_FEED_OPEN);
        host.stream
            .write_all(&encode_reply(REPLY_FEED, 0, b"/run/feed"))
            .await
            .unwrap();
        assert_eq!(
            helper_line(&listener).await,
            format!("sco-sink /run/feed {stream}")
        );

        media.set_active(1, true);
        media.set_active(2, true);
        media.set_active(1, false);
        for _ in 0..3 {
            assert_eq!(host.next().await, (OP_AUDIO_ACTIVE, stream, vec![1]));
        }
        media.set_active(2, false);
        assert_eq!(host.next().await, (OP_AUDIO_ACTIVE, stream, vec![0]));

        media.stop_call(&helper);
        media.stop_call(&helper);
        assert_eq!(host.next().await, (OP_TAP_STOP, *tap, vec![]));
        assert_eq!(host.next().await, (OP_AUDIO_STOP, stream, vec![]));
        assert_eq!(helper_line(&listener).await, "sco-sink");
        assert!(media.audio_outputs().is_empty());
    }
}
