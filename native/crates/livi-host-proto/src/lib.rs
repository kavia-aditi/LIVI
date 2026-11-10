// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

pub mod display;
pub mod feed;
pub mod ops;
pub mod ui_planes;

/// Private development layout only; does not create paths or enable hardware.
#[cfg(unix)]
pub mod isolated_runtime {
    use std::ffi::OsStr;
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::path::{Path, PathBuf};

    #[derive(Debug, PartialEq, Eq)]
    pub struct Layout {
        pub root: PathBuf,
        pub transport: PathBuf,
        pub runtime: u64,
    }

    impl Layout {
        /// The transport is exactly ROOT/livi, matching core's unchanged
        /// runtime_dir API. Environment values always identify ROOT, not livi.
        pub fn from_transport(
            transport: &Path,
            uid: u32,
            nofollow: i32,
            xdg: Option<&OsStr>,
            configured: Option<&OsStr>,
            expected: Option<u64>,
        ) -> Option<Self> {
            let root = transport.parent()?;
            let name = root.file_name()?.to_str()?;
            let exact_root = Path::new("/tmp").join(name);
            let exact_transport = root.join("livi");
            if uid == 0 || !name.starts_with("livi-bt-dev-")
                || name.len() == "livi-bt-dev-".len() || root.as_os_str().len() > 64
                || root.as_os_str() != exact_root.as_os_str()
                || transport.as_os_str() != exact_transport.as_os_str()
                || xdg != Some(root.as_os_str()) || configured != Some(root.as_os_str()) {
                return None;
            }
            let private = |path: &Path| {
                let meta = std::fs::symlink_metadata(path).ok()?;
                (meta.is_dir() && meta.uid() == uid && meta.mode() & 0o7777 == 0o700)
                    .then_some((meta.dev(), meta.ino()))
            };
            let root_identity = private(root)?;
            let transport_identity = private(transport)?;
            let file = std::fs::OpenOptions::new().read(true).custom_flags(nofollow)
                .open(root.join("native-runtime")).ok()?;
            let meta = file.metadata().ok()?;
            if !meta.is_file() || meta.uid() != uid || meta.mode() & 0o077 != 0
                || meta.len() > 32 { return None; }
            let mut text = String::new();
            file.take(33).read_to_string(&mut text).ok()?;
            if text.len() > 32 { return None; }
            let runtime = text.trim().parse::<u64>().ok().filter(|id| *id != 0)?;
            if expected.is_some_and(|id| id != runtime)
                || private(root)? != root_identity || private(transport)? != transport_identity {
                return None;
            }
            Some(Self { root: root.to_path_buf(), transport: transport.to_path_buf(), runtime })
        }
    }

    #[cfg(all(test, target_os = "macos"))]
    mod tests {
        use super::*;
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};

        struct Fixture(PathBuf);
        impl Fixture {
            fn new() -> Self {
                let root = PathBuf::from(format!("/tmp/livi-bt-dev-contract-{}", std::process::id()));
                std::fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
                std::fs::DirBuilder::new().mode(0o700).create(root.join("livi")).unwrap();
                Self(root)
            }
            fn marker(&self, text: &str) {
                let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true)
                    .mode(0o600).open(self.0.join("native-runtime")).unwrap();
                file.write_all(text.as_bytes()).unwrap();
            }
            fn layout(&self, expected: Option<u64>) -> Option<Layout> {
                unsafe extern "C" { fn geteuid() -> u32; }
                Layout::from_transport(&self.0.join("livi"), unsafe { geteuid() }, 0x100,
                    Some(self.0.as_os_str()), Some(self.0.as_os_str()), expected)
            }
        }
        impl Drop for Fixture {
            fn drop(&mut self) { std::fs::remove_dir_all(&self.0).unwrap(); }
        }

        #[test]
        fn private_layout_requires_helper_marker_and_exact_runtime_id() {
            let f = Fixture::new();
            assert!(f.layout(None).is_none(), "not ready before helper publishes");
            for marker in ["0", "-1", "invalid", "18446744073709551616", "7\njunk",
                "777777777777777777777777777777777"] {
                f.marker(marker);
                assert!(f.layout(None).is_none());
            }
            f.marker("7\n");
            let layout = f.layout(Some(7)).unwrap();
            assert_eq!(layout.root, f.0);
            assert_eq!(layout.transport, f.0.join("livi"));
            assert_eq!(layout.runtime, 7);
            assert!(f.layout(Some(8)).is_none());
            assert!(f.layout(Some(0)).is_none());
        }

        #[test]
        fn root_child_and_marker_permissions_and_links_fail_closed() {
            // Separate name from the concurrently running identity test.
            let root = PathBuf::from(format!("/tmp/livi-bt-dev-contract-links-{}", std::process::id()));
            std::fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
            std::fs::DirBuilder::new().mode(0o700).create(root.join("livi")).unwrap();
            let f = Fixture(root);
            f.marker("9\n");
            for path in [f.0.clone(), f.0.join("livi"), f.0.join("native-runtime")] {
                let original = std::fs::metadata(&path).unwrap().permissions();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
                assert!(f.layout(None).is_none());
                std::fs::set_permissions(&path, original).unwrap();
                let saved = path.with_extension("saved");
                std::fs::rename(&path, &saved).unwrap();
                symlink(&saved, &path).unwrap();
                assert!(f.layout(None).is_none());
                std::fs::remove_file(&path).unwrap();
                std::fs::rename(saved, path).unwrap();
            }
            assert!(f.layout(Some(9)).is_some());
        }

        #[test]
        fn inconsistent_environment_owner_and_noncanonical_transport_are_rejected() {
            let root = PathBuf::from(format!("/tmp/livi-bt-dev-contract-env-{}", std::process::id()));
            std::fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
            std::fs::DirBuilder::new().mode(0o700).create(root.join("livi")).unwrap();
            let f = Fixture(root);
            f.marker("11\n");
            let uid = std::fs::metadata(&f.0).unwrap().uid();
            let env = Some(f.0.as_os_str());
            let transport = f.0.join("livi");
            for (xdg, configured) in [(None, env), (env, None), (env, Some(transport.as_os_str()))] {
                assert!(Layout::from_transport(&transport, uid, 0x100, xdg, configured, None).is_none());
            }
            for wrong_uid in [0, uid + 1] {
                assert!(Layout::from_transport(&transport, wrong_uid, 0x100, env, env, None).is_none());
            }
            for path in [f.0.clone(), f.0.join("other"), f.0.join("livi/."), f.0.join("livi/../livi")] {
                assert!(Layout::from_transport(&path, uid, 0x100, env, env, None).is_none());
            }
            assert!(f.layout(Some(11)).is_some());
        }
    }
}

const LEN_BYTES: usize = 4;
const HEAD_BYTES: usize = 5;

#[derive(Debug, PartialEq, Eq)]
pub struct Message {
    pub op: u8,
    pub id: u32,
    pub rest: Vec<u8>,
}

#[derive(Default)]
pub struct Framer {
    buf: Vec<u8>,
}

impl Framer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    pub fn next_message(&mut self) -> Option<Message> {
        loop {
            if self.buf.len() < LEN_BYTES {
                return None;
            }
            let len =
                u32::from_ne_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
            if self.buf.len() < LEN_BYTES + len {
                return None;
            }

            let frame: Vec<u8> = self.buf.drain(..LEN_BYTES + len).skip(LEN_BYTES).collect();
            if len < HEAD_BYTES {
                continue;
            }
            return Some(Message {
                op: frame[0],
                id: u32::from_ne_bytes([frame[1], frame[2], frame[3], frame[4]]),
                rest: frame[HEAD_BYTES..].to_vec(),
            });
        }
    }
}

pub fn encode_reply(op: u8, id: u32, rest: &[u8]) -> Vec<u8> {
    let len = (HEAD_BYTES + rest.len()) as u32;
    let mut out = Vec::with_capacity(LEN_BYTES + len as usize);
    out.extend_from_slice(&len.to_ne_bytes());
    out.push(op);
    out.extend_from_slice(&id.to_ne_bytes());
    out.extend_from_slice(rest);
    out
}

pub const PLANE_MAIN: u32 = 0x7a00_0001;
pub const PLANE_VIDEO: u32 = 0x7a00_0002;
pub const PLANE_CAMERA: u32 = 0x7a00_0003;
pub const PLANE_CLUSTER_RECV: u32 = 0x7a00_0010;
pub const CLUSTER_RECV_ID: u32 = 0x7a00_0010;
pub const CLUSTER_PLANE_MIN: u32 = 0x7a00_0011;
pub const CLUSTER_PLANE_MAX: u32 = 0x7a00_0013;

pub fn is_cluster_plane(id: u32) -> bool {
    (CLUSTER_PLANE_MIN..=CLUSTER_PLANE_MAX).contains(&id)
}

pub fn feeder_of(plane_id: u32) -> u32 {
    if is_cluster_plane(plane_id) {
        CLUSTER_RECV_ID
    } else {
        plane_id
    }
}

pub const SCREENS: [&str; 3] = ["main", "cluster", "aux"];
pub const MAIN_TAG: &str = "main";
pub const VIDEO_TAG: &str = "video";
pub const CAMERA_TAG: &str = "camera";

pub fn cluster_tag(screen: &str) -> String {
    format!("cluster-{screen}")
}

pub fn plane_place(id: u32) -> Option<(String, &'static str)> {
    if id == PLANE_MAIN {
        return Some((MAIN_TAG.into(), SCREENS[0]));
    }
    if id == PLANE_VIDEO {
        return Some((VIDEO_TAG.into(), SCREENS[0]));
    }
    if id == PLANE_CAMERA {
        return Some((CAMERA_TAG.into(), SCREENS[0]));
    }
    let screen = SCREENS.get(id.checked_sub(CLUSTER_PLANE_MIN)? as usize)?;
    Some((cluster_tag(screen), screen))
}

const CODEC_MAX: usize = 15;

pub fn plane_body(codec: &str, codec_data: &[u8]) -> Vec<u8> {
    let codec = &codec.as_bytes()[..codec.len().min(CODEC_MAX)];
    let mut rest = Vec::with_capacity(1 + codec.len() + codec_data.len());
    rest.push(codec.len() as u8);
    rest.extend_from_slice(codec);
    rest.extend_from_slice(codec_data);
    rest
}

pub fn parse_plane_body(rest: &[u8]) -> Option<(String, &[u8])> {
    let len = usize::from(*rest.first()?).min(CODEC_MAX);
    let codec = rest.get(1..1 + len)?;
    Some((
        String::from_utf8_lossy(codec).into_owned(),
        &rest[1 + len..],
    ))
}

pub fn url_body(url: &str, audio_device: &str) -> Vec<u8> {
    let device = &audio_device.as_bytes()[..audio_device.len().min(usize::from(u16::MAX))];
    let mut rest = Vec::with_capacity(2 + device.len() + url.len());
    rest.extend_from_slice(&(device.len() as u16).to_le_bytes());
    rest.extend_from_slice(device);
    rest.extend_from_slice(url.as_bytes());
    rest
}

pub fn parse_url_body(rest: &[u8]) -> Option<(String, String)> {
    let len = usize::from(u16::from_le_bytes([*rest.first()?, *rest.get(1)?]));
    let device = core::str::from_utf8(rest.get(2..2 + len)?).ok()?;
    let url = core::str::from_utf8(&rest[2 + len..])
        .ok()
        .filter(|u| !u.is_empty())?;
    Some((url.to_owned(), device.to_owned()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraOrder {
    pub device: String,
    pub rotation: u16,
    pub mirror: bool,
}

pub fn camera_body(order: &CameraOrder) -> Vec<u8> {
    let mut rest = Vec::with_capacity(3 + order.device.len());
    rest.extend_from_slice(&order.rotation.to_le_bytes());
    rest.push(u8::from(order.mirror));
    rest.extend_from_slice(order.device.as_bytes());
    rest
}

pub fn parse_camera_body(rest: &[u8]) -> Option<CameraOrder> {
    let rotation = u16::from_le_bytes([*rest.first()?, *rest.get(1)?]);
    let mirror = *rest.get(2)? != 0;
    let device = core::str::from_utf8(&rest[3..]).ok()?.to_owned();
    Some(CameraOrder {
        device,
        rotation,
        mirror,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraSeen {
    pub id: String,
    pub name: String,
}

pub fn cameras_body(cameras: &[CameraSeen]) -> Vec<u8> {
    let clean = |s: &str| s.replace(['\t', '\n'], " ");
    cameras
        .iter()
        .map(|c| format!("{}\t{}\n", clean(&c.id), clean(&c.name)))
        .collect::<String>()
        .into_bytes()
}

pub fn parse_cameras_body(rest: &[u8]) -> Vec<CameraSeen> {
    String::from_utf8_lossy(rest)
        .lines()
        .filter_map(|line| {
            let (id, name) = line.split_once('\t')?;
            Some(CameraSeen {
                id: id.to_owned(),
                name: name.to_owned(),
            })
        })
        .collect()
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct UrlStatus {
    pub position: Option<f64>,
    pub duration: Option<f64>,
    pub seekable: Option<(f64, f64)>,
    pub playing: bool,
    pub ready: bool,
    pub buffered_percent: Option<u8>,
    pub ended: bool,
    pub failed: bool,
}

const STATUS_PLAYING: u8 = 1;
const STATUS_READY: u8 = 2;
const STATUS_ENDED: u8 = 4;
const STATUS_FAILED: u8 = 8;
const STATUS_BUFFERING: u8 = 16;

pub fn url_status_body(s: &UrlStatus) -> Vec<u8> {
    let flag = |on: bool, bit: u8| if on { bit } else { 0 };
    let flags = flag(s.playing, STATUS_PLAYING)
        | flag(s.ready, STATUS_READY)
        | flag(s.ended, STATUS_ENDED)
        | flag(s.failed, STATUS_FAILED)
        | flag(s.buffered_percent.is_some(), STATUS_BUFFERING);
    let mut rest = vec![flags, s.buffered_percent.unwrap_or(0)];
    let (start, end) = s.seekable.unzip();
    for t in [s.position, s.duration, start, end] {
        rest.extend_from_slice(&t.unwrap_or(f64::NAN).to_le_bytes());
    }
    rest
}

pub fn parse_url_status(rest: &[u8]) -> Option<UrlStatus> {
    let (&flags, rest) = rest.split_first()?;
    let (&percent, rest) = rest.split_first()?;
    let (times, _) = rest.as_chunks::<8>();
    if times.len() < 4 {
        return None;
    }
    let time = |i: usize| Some(f64::from_le_bytes(times[i])).filter(|t| !t.is_nan());
    Some(UrlStatus {
        position: time(0),
        duration: time(1),
        seekable: time(2).zip(time(3)),
        playing: flags & STATUS_PLAYING != 0,
        ready: flags & STATUS_READY != 0,
        buffered_percent: (flags & STATUS_BUFFERING != 0).then_some(percent),
        ended: flags & STATUS_ENDED != 0,
        failed: flags & STATUS_FAILED != 0,
    })
}

/// Opt-in Stage 5 contracts. Legacy Framer, plane_body and operation values
/// above are intentionally unchanged. No packet here enables hardware.
pub mod native {
    use super::Message;
    use std::collections::VecDeque;

    pub const VERSION: u16 = 1;
    pub const MAX_CODEC_DATA: usize = 64 * 1024;
    pub const MAX_COMPRESSED: usize = 4 * 1024 * 1024;
    pub const MAX_BODY: usize = MAX_COMPRESSED + 128;
    pub const MAX_FRAME: usize = MAX_BODY + 5;
    pub const MAX_BUFFER: usize = MAX_FRAME + 4;
    pub const MAX_MESSAGES: usize = 32;
    pub const CAP_OWNER_ROUTING: u32 = 1;
    pub const CAP_DECODED_BGRA: u32 = 2;
    pub const CAP_ACTUAL_PRESENTATION: u32 = 4;
    pub const CAP_LIFECYCLE: u32 = 8;
    pub const REQUIRED_CAPABILITIES: u32 = 15;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Error {
        Malformed,
        TooLarge,
        Backpressure,
        Poisoned,
    }

    /// Unique runtime incarnation and helper-local attempt, never a phone ID.
    /// Every component must compare the entire value, not just PLANE_MAIN/IP.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct Owner {
        pub runtime: u64,
        pub attempt: u64,
        pub generation: u64,
        pub session: u64,
        pub connection: u64,
        pub player: u64,
    }

    impl Owner {
        pub fn valid(self) -> bool {
            self.ids().into_iter().all(|id| id != 0)
        }

        fn ids(self) -> [u64; 6] {
            [self.runtime, self.attempt, self.generation, self.session, self.connection, self.player]
        }

        fn encode(self, out: &mut Vec<u8>) {
            for id in self.ids() {
                out.extend_from_slice(&id.to_le_bytes());
            }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Capabilities {
        pub bits: u32,
    }

    impl Capabilities {
        pub fn supports_stage_five(self) -> bool {
            self.bits == REQUIRED_CAPABILITIES
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Create {
        pub owner: Owner,
        pub codec: String,
        pub codec_data: Vec<u8>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[repr(u8)]
    pub enum Failure {
        Unsupported = 0,
        Decode = 1,
        Render = 2,
        Backpressure = 3,
        Disconnected = 4,
        Stale = 5,
    }

    /// Decoded is appsink delivery after conversion, not presentation.
    /// Only the renderer may originate Presented after the protected GPU AND
    /// actual-presentedTime gate. Parsing this value is not evidence of M7.
    #[derive(Debug, Clone, PartialEq)]
    pub enum Feedback {
        Decoded { owner: Owner, frame: u64, ready_time: f64 },
        Presented { owner: Owner, frame: u64, visibility: u64, time: f64 },
        Invalidated { owner: Owner, visibility: u64 },
        Failed { owner: Owner, code: Failure },
    }

    #[derive(Debug, Clone, PartialEq)]
    pub enum Packet {
        Capabilities(Capabilities),
        Create(Create),
        Data { owner: Owner, bytes: Vec<u8> },
        Stop(Owner),
        Flush(Owner),
        Route { owner: Owner, feed: u32 },
        Active { owner: Owner, active: bool },
        Feedback(Feedback),
    }

    fn positive(time: f64) -> bool {
        time.is_finite() && time > 0.0
    }

    impl Packet {
        /// Capabilities have no media owner and must be negotiated separately.
        pub fn owner(&self) -> Option<Owner> {
            match self {
                Self::Capabilities(_) => None,
                Self::Create(c) => Some(c.owner),
                Self::Data { owner, .. } | Self::Route { owner, .. }
                | Self::Active { owner, .. } | Self::Stop(owner) | Self::Flush(owner) => Some(*owner),
                Self::Feedback(f) => Some(match f {
                    Feedback::Decoded { owner, .. } | Feedback::Presented { owner, .. }
                    | Feedback::Invalidated { owner, .. } | Feedback::Failed { owner, .. } => *owner,
                }),
            }
        }

        /// Compare the complete immutable owner, including connection/player.
        /// Receivers must additionally validate opcode/direction and lifecycle.
        pub fn decode_owned(bytes: &[u8], expected: Owner) -> Option<Self> {
            let packet = Self::decode(bytes)?;
            (expected.valid() && packet.owner() == Some(expected)).then_some(packet)
        }

        pub fn encode(&self) -> Result<Vec<u8>, Error> {
            let mut out = VERSION.to_le_bytes().to_vec();
            match self {
                Self::Capabilities(c) => {
                    if c.bits & !REQUIRED_CAPABILITIES != 0 { return Err(Error::Malformed); }
                    out.push(0);
                    out.extend_from_slice(&c.bits.to_le_bytes());
                }
                Self::Create(c) => {
                    if !c.owner.valid() || !matches!(c.codec.as_str(), "h264" | "h265" | "vp9" | "av1") {
                        return Err(Error::Malformed);
                    }
                    if c.codec_data.len() > MAX_CODEC_DATA { return Err(Error::TooLarge); }
                    out.push(1);
                    c.owner.encode(&mut out);
                    out.push(c.codec.len() as u8);
                    out.extend_from_slice(c.codec.as_bytes());
                    out.extend_from_slice(&(c.codec_data.len() as u32).to_le_bytes());
                    out.extend_from_slice(&c.codec_data);
                }
                Self::Data { owner, bytes } => {
                    if !owner.valid() || bytes.is_empty() { return Err(Error::Malformed); }
                    if bytes.len() > MAX_COMPRESSED { return Err(Error::TooLarge); }
                    out.push(2);
                    owner.encode(&mut out);
                    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                    out.extend_from_slice(bytes);
                }
                Self::Stop(owner) | Self::Flush(owner) => {
                    if !owner.valid() { return Err(Error::Malformed); }
                    out.push(if matches!(self, Self::Stop(_)) { 3 } else { 4 });
                    owner.encode(&mut out);
                }
                Self::Route { owner, feed } => {
                    if !owner.valid() || *feed == 0 { return Err(Error::Malformed); }
                    out.push(5);
                    owner.encode(&mut out);
                    out.extend_from_slice(&feed.to_le_bytes());
                }
                Self::Active { owner, active } => {
                    if !owner.valid() { return Err(Error::Malformed); }
                    out.push(6);
                    owner.encode(&mut out);
                    out.push(u8::from(*active));
                }
                Self::Feedback(feedback) => {
                    out.push(7);
                    match feedback {
                        Feedback::Decoded { owner, frame, ready_time } => {
                            if !owner.valid() || *frame == 0 || !positive(*ready_time) { return Err(Error::Malformed); }
                            out.push(1);
                            owner.encode(&mut out);
                            out.extend_from_slice(&frame.to_le_bytes());
                            out.extend_from_slice(&ready_time.to_le_bytes());
                        }
                        Feedback::Presented { owner, frame, visibility, time } => {
                            if !owner.valid() || *frame == 0 || *visibility == 0 || !positive(*time) { return Err(Error::Malformed); }
                            out.push(2);
                            owner.encode(&mut out);
                            out.extend_from_slice(&frame.to_le_bytes());
                            out.extend_from_slice(&visibility.to_le_bytes());
                            out.extend_from_slice(&time.to_le_bytes());
                        }
                        Feedback::Invalidated { owner, visibility } => {
                            if !owner.valid() || *visibility == 0 { return Err(Error::Malformed); }
                            out.push(3);
                            owner.encode(&mut out);
                            out.extend_from_slice(&visibility.to_le_bytes());
                        }
                        Feedback::Failed { owner, code } => {
                            if !owner.valid() { return Err(Error::Malformed); }
                            out.push(4);
                            owner.encode(&mut out);
                            out.push(*code as u8);
                        }
                    }
                }
            }
            Ok(out)
        }

        pub fn decode(bytes: &[u8]) -> Option<Self> {
            if bytes.len() > MAX_BODY { return None; }
            let mut r = Reader(bytes);
            if u16::from_le_bytes(r.take(2)?.try_into().ok()?) != VERSION { return None; }
            let packet = match r.byte()? {
                0 => Self::Capabilities(Capabilities { bits: r.u32()? }),
                1 => {
                    let owner = r.owner()?;
                    let len = usize::from(r.byte()?);
                    if len > 4 { return None; }
                    let codec = std::str::from_utf8(r.take(len)?).ok()?.to_owned();
                    let size = r.u32()? as usize;
                    if size > MAX_CODEC_DATA { return None; }
                    Self::Create(Create { owner, codec, codec_data: r.take(size)?.to_vec() })
                }
                2 => {
                    let owner = r.owner()?;
                    let size = r.u32()? as usize;
                    if size > MAX_COMPRESSED { return None; }
                    Self::Data { owner, bytes: r.take(size)?.to_vec() }
                }
                3 => Self::Stop(r.owner()?),
                4 => Self::Flush(r.owner()?),
                5 => Self::Route { owner: r.owner()?, feed: r.u32()? },
                6 => {
                    let owner = r.owner()?;
                    let active = match r.byte()? { 0 => false, 1 => true, _ => return None };
                    Self::Active { owner, active }
                }
                7 => Self::Feedback(match r.byte()? {
                    1 => Feedback::Decoded { owner: r.owner()?, frame: r.u64()?, ready_time: r.time()? },
                    2 => Feedback::Presented { owner: r.owner()?, frame: r.u64()?, visibility: r.u64()?, time: r.time()? },
                    3 => Feedback::Invalidated { owner: r.owner()?, visibility: r.u64()? },
                    4 => Feedback::Failed {
                        owner: r.owner()?,
                        code: match r.byte()? {
                            0 => Failure::Unsupported, 1 => Failure::Decode, 2 => Failure::Render,
                            3 => Failure::Backpressure, 4 => Failure::Disconnected, 5 => Failure::Stale,
                            _ => return None,
                        },
                    },
                    _ => return None,
                }),
                _ => return None,
            };
            // Exact consumption and shared validation, without re-copying large data.
            if !r.0.is_empty() || !packet.valid() { return None; }
            Some(packet)
        }

        fn valid(&self) -> bool {
            match self {
                Self::Capabilities(c) => c.bits & !REQUIRED_CAPABILITIES == 0,
                Self::Create(c) => c.owner.valid() && matches!(c.codec.as_str(), "h264" | "h265" | "vp9" | "av1"),
                Self::Data { owner, bytes } => owner.valid() && !bytes.is_empty(),
                Self::Stop(o) | Self::Flush(o) => o.valid(),
                Self::Route { owner, feed } => owner.valid() && *feed != 0,
                Self::Active { owner, .. } => owner.valid(),
                Self::Feedback(f) => match f {
                    Feedback::Decoded { owner, frame, ready_time } => owner.valid() && *frame != 0 && positive(*ready_time),
                    Feedback::Presented { owner, frame, visibility, time } => owner.valid() && *frame != 0 && *visibility != 0 && positive(*time),
                    Feedback::Invalidated { owner, visibility } => owner.valid() && *visibility != 0,
                    Feedback::Failed { owner, .. } => owner.valid(),
                },
            }
        }
    }

    struct Reader<'a>(&'a [u8]);
    impl<'a> Reader<'a> {
        fn take(&mut self, n: usize) -> Option<&'a [u8]> {
            let got = self.0.get(..n)?;
            self.0 = &self.0[n..];
            Some(got)
        }
        fn byte(&mut self) -> Option<u8> { Some(self.take(1)?[0]) }
        fn u32(&mut self) -> Option<u32> { Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?)) }
        fn u64(&mut self) -> Option<u64> { Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?)) }
        fn time(&mut self) -> Option<f64> { Some(f64::from_le_bytes(self.take(8)?.try_into().ok()?)) }
        fn owner(&mut self) -> Option<Owner> {
            let owner = Owner { runtime: self.u64()?, attempt: self.u64()?, generation: self.u64()?,
                session: self.u64()?, connection: self.u64()?, player: self.u64()? };
            owner.valid().then_some(owner)
        }
    }

    /// Opt-in bounded framing. Validates the advertised length at four bytes,
    /// before retaining any payload. A failure poisons and clears the connection.
    /// Callers must close on error and validate Packet plus opcode/direction.
    #[derive(Default)]
    pub struct Framer {
        partial: Vec<u8>,
        ready: VecDeque<Vec<u8>>,
        retained: usize,
        poisoned: bool,
    }

    impl Framer {
        pub fn new() -> Self { Self::default() }

        pub fn push(&mut self, mut chunk: &[u8]) -> Result<(), Error> {
            if self.poisoned { return Err(Error::Poisoned); }
            let result = self.append(&mut chunk);
            if result.is_err() {
                self.partial.clear();
                self.ready.clear();
                self.retained = 0;
                self.poisoned = true;
            }
            result
        }

        fn append(&mut self, chunk: &mut &[u8]) -> Result<(), Error> {
            while !chunk.is_empty() {
                let target = if self.partial.len() < 4 {
                    4
                } else {
                    let len = u32::from_ne_bytes(self.partial[..4].try_into().unwrap()) as usize;
                    if len < 5 { return Err(Error::Malformed); }
                    if len > MAX_FRAME { return Err(Error::TooLarge); }
                    len + 4
                };
                let count = (target - self.partial.len()).min(chunk.len());
                if count > MAX_BUFFER.saturating_sub(self.retained) { return Err(Error::Backpressure); }
                self.partial.extend_from_slice(&chunk[..count]);
                self.retained += count;
                *chunk = &chunk[count..];
                if self.partial.len() >= 4 {
                    let len = u32::from_ne_bytes(self.partial[..4].try_into().unwrap()) as usize;
                    if len < 5 { return Err(Error::Malformed); }
                    if len > MAX_FRAME { return Err(Error::TooLarge); }
                    if self.partial.len() == len + 4 {
                        if self.ready.len() == MAX_MESSAGES { return Err(Error::Backpressure); }
                        self.ready.push_back(std::mem::take(&mut self.partial));
                    }
                }
            }
            Ok(())
        }

        pub fn next_message(&mut self) -> Option<Message> {
            let bytes = self.ready.pop_front()?;
            self.retained -= bytes.len();
            Some(Message { op: bytes[4], id: u32::from_ne_bytes(bytes[5..9].try_into().ok()?),
                rest: bytes[9..].to_vec() })
        }

        /// EOF never completes a partial frame. Complete queued messages may be
        /// drained first, but the caller must still invalidate connection state.
        pub fn finish(&mut self) -> Result<(), Error> {
            if self.poisoned { return Err(Error::Poisoned); }
            if self.partial.is_empty() { return Ok(()); }
            self.partial.clear();
            self.ready.clear();
            self.retained = 0;
            self.poisoned = true;
            Err(Error::Malformed)
        }
    }

    pub fn frame(op: u8, id: u32, packet: &Packet) -> Result<Vec<u8>, Error> {
        let body = packet.encode()?;
        if body.len() > MAX_BODY { return Err(Error::TooLarge); }
        Ok(super::encode_reply(op, id, &body))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn owner() -> Owner {
            Owner { runtime: 1, attempt: 2, generation: 3, session: 4, connection: 5, player: 6 }
        }

        #[test]
        fn every_native_packet_round_trips_and_requires_exact_consumption() {
            let o = owner();
            for p in [
                Packet::Capabilities(Capabilities { bits: REQUIRED_CAPABILITIES }),
                Packet::Create(Create { owner: o, codec: "h264".into(), codec_data: vec![0, 1, 255] }),
                Packet::Data { owner: o, bytes: vec![1, 2] },
                Packet::Stop(o), Packet::Flush(o), Packet::Route { owner: o, feed: 7 },
                Packet::Active { owner: o, active: true },
                Packet::Feedback(Feedback::Decoded { owner: o, frame: 8, ready_time: 1.0 }),
                Packet::Feedback(Feedback::Presented { owner: o, frame: 8, visibility: 9, time: 2.0 }),
                Packet::Feedback(Feedback::Invalidated { owner: o, visibility: 10 }),
                Packet::Feedback(Feedback::Failed { owner: o, code: Failure::Decode }),
            ] {
                let b = p.encode().unwrap();
                assert_eq!(Packet::decode(&b), Some(p));
                for n in 0..b.len() { assert!(Packet::decode(&b[..n]).is_none()); }
                let mut extra = b.clone(); extra.push(0);
                assert!(Packet::decode(&extra).is_none());
                let mut version = b; version[0] = 2;
                assert!(Packet::decode(&version).is_none());
            }
        }

        #[test]
        fn malformed_identity_caps_bool_codec_and_presentation_are_rejected() {
            let mut bad = owner(); bad.connection = 0;
            assert!(Packet::Stop(bad).encode().is_err());
            assert!(Packet::Capabilities(Capabilities { bits: 16 }).encode().is_err());
            assert!(!Capabilities { bits: 0 }.supports_stage_five());
            assert!(Packet::Create(Create { owner: owner(), codec: "raw".into(), codec_data: vec![] }).encode().is_err());
            for time in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
                assert!(Packet::Feedback(Feedback::Presented { owner: owner(), frame: 1, visibility: 1, time }).encode().is_err());
                let mut bytes = Packet::Feedback(Feedback::Presented { owner: owner(), frame: 1, visibility: 1, time: 2.0 }).encode().unwrap();
                let end = bytes.len();
                bytes[end - 8..].copy_from_slice(&time.to_le_bytes());
                assert!(Packet::decode(&bytes).is_none());
            }
            let mut b = Packet::Active { owner: owner(), active: true }.encode().unwrap();
            *b.last_mut().unwrap() = 2;
            assert!(Packet::decode(&b).is_none());
            assert!(Packet::Feedback(Feedback::Presented { owner: owner(), frame: 0, visibility: 1, time: 1.0 }).encode().is_err());
        }

        #[test]
        fn payload_limits_are_checked_before_copying() {
            let c = Create { owner: owner(), codec: "h265".into(), codec_data: vec![9; MAX_CODEC_DATA] };
            assert!(Packet::Create(c.clone()).encode().is_ok());
            let mut c = c; c.codec_data.push(0);
            assert_eq!(Packet::Create(c).encode(), Err(Error::TooLarge));
            let mut bytes = Packet::Data { owner: owner(), bytes: vec![1] }.encode().unwrap();
            bytes[51..55].copy_from_slice(&((MAX_COMPRESSED + 1) as u32).to_le_bytes());
            assert!(Packet::decode(&bytes).is_none());
        }

        #[test]
        fn bounded_framer_handles_fragmentation_coalescing_and_poisoning() {
            let bytes = frame(64, 1, &Packet::Capabilities(Capabilities { bits: 0 })).unwrap();
            let mut f = Framer::new();
            for byte in &bytes { f.push(&[*byte]).unwrap(); }
            assert_eq!(f.next_message().unwrap().rest, bytes[9..]);
            f.push(&[bytes.clone(), bytes.clone()].concat()).unwrap();
            assert!(f.next_message().is_some());
            assert!(f.next_message().is_some());
            assert!(f.next_message().is_none());
            assert_eq!(f.push(&u32::MAX.to_ne_bytes()), Err(Error::TooLarge));
            assert_eq!(f.push(&bytes), Err(Error::Poisoned));
            assert!(f.next_message().is_none());
            let mut f = Framer::new();
            assert_eq!(f.push(&4u32.to_ne_bytes()), Err(Error::Malformed));
        }

        #[test]
        fn count_and_byte_backpressure_clear_queued_messages() {
            let bytes = frame(64, 1, &Packet::Capabilities(Capabilities { bits: 0 })).unwrap();
            let mut f = Framer::new();
            for _ in 0..MAX_MESSAGES { f.push(&bytes).unwrap(); }
            assert_eq!(f.push(&bytes), Err(Error::Backpressure));
            assert!(f.next_message().is_none());
            let mut f = Framer::new();
            let big = frame(66, 1, &Packet::Data { owner: owner(), bytes: vec![1; MAX_COMPRESSED] }).unwrap();
            f.push(&big).unwrap();
            assert_eq!(f.push(&big), Err(Error::Backpressure));
            assert!(f.next_message().is_none());
        }

        #[test]
        fn each_owner_dimension_is_required_and_stale_owners_are_rejected() {
            let bytes = Packet::Stop(owner()).encode().unwrap();
            assert!(Packet::decode_owned(&bytes, owner()).is_some());
            for index in 0..6 {
                let mut stale = bytes.clone();
                stale[3 + index * 8..11 + index * 8].copy_from_slice(&99u64.to_le_bytes());
                assert!(Packet::decode(&stale).is_some());
                assert!(Packet::decode_owned(&stale, owner()).is_none());
                stale[3 + index * 8..11 + index * 8].fill(0);
                assert!(Packet::decode(&stale).is_none());
            }
            let caps = Packet::Capabilities(Capabilities { bits: 0 }).encode().unwrap();
            assert!(Packet::decode_owned(&caps, owner()).is_none());
        }

        #[test]
        fn partial_eof_never_yields_a_message() {
            let bytes = frame(64, 1, &Packet::Capabilities(Capabilities { bits: 0 })).unwrap();
            for n in 1..bytes.len() {
                let mut f = Framer::new();
                f.push(&bytes[..n]).unwrap();
                assert_eq!(f.finish(), Err(Error::Malformed));
                assert!(f.next_message().is_none());
            }
            let mut f = Framer::new();
            f.push(&bytes).unwrap();
            assert_eq!(f.finish(), Ok(()));
            assert!(f.next_message().is_some());
        }

        #[test]
        fn opcode_direction_and_legacy_bytes_are_not_reinterpreted() {
            let packet = Packet::Stop(owner()).encode().unwrap();
            assert!(crate::ui_planes::parse_native(crate::ui_planes::NATIVE_STOP, false, &packet).is_some());
            assert!(crate::ui_planes::parse_native(crate::ui_planes::NATIVE_STOP, true, &packet).is_none());
            assert!(crate::ui_planes::parse_native(crate::ui_planes::CREATE, false, &packet).is_none());
            assert!(crate::ops::parse_native(crate::ops::OP_NATIVE_STOP, false, &packet).is_some());
            assert!(crate::ops::parse_native(crate::ops::OP_NATIVE_STOP, true, &packet).is_none());
            assert!(crate::ops::parse_native(crate::ops::OP_CREATE, false, &packet).is_none());
            let route = Packet::Route { owner: owner(), feed: 7 }.encode().unwrap();
            assert!(crate::ops::parse_native(crate::ops::OP_NATIVE_ROUTE, false, &route).is_some());
            assert!(crate::ui_planes::parse_native(crate::ops::OP_NATIVE_ROUTE, false, &route).is_none());
            assert_eq!(super::super::plane_body("h264", &[9, 8]), [4, b'h', b'2', b'6', b'4', 9, 8]);
            assert_eq!(crate::ops::OP_CREATE, 1);
            assert_eq!(crate::ops::OP_CAMERAS_WATCH, 27);
            assert_eq!(crate::ops::REPLY_STARTED, 3);
            assert_eq!(crate::ui_planes::CAMERA, 10);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(op: u8, id: u32, rest: &[u8]) -> Vec<u8> {
        encode_reply(op, id, rest)
    }

    #[test]
    fn a_whole_message_comes_back_out() {
        let mut f = Framer::new();
        f.push(&frame(2, 0x7a00_0001, &[1, 2, 3]));

        assert_eq!(
            f.next_message(),
            Some(Message {
                op: 2,
                id: 0x7a00_0001,
                rest: vec![1, 2, 3]
            })
        );
        assert_eq!(f.next_message(), None);
    }

    #[test]
    fn a_message_split_across_chunks_is_reassembled() {
        let msg = frame(3, 7, &[9; 40]);
        let mut f = Framer::new();

        for piece in msg.chunks(6) {
            f.push(piece);
        }

        assert_eq!(f.next_message().unwrap().rest.len(), 40);
    }

    #[test]
    fn several_messages_in_one_chunk_come_out_in_order() {
        let mut buf = frame(1, 10, b"a");
        buf.extend(frame(2, 20, b"bb"));
        let mut f = Framer::new();
        f.push(&buf);

        assert_eq!(f.next_message().unwrap().id, 10);
        assert_eq!(f.next_message().unwrap().id, 20);
        assert_eq!(f.next_message(), None);
    }

    #[test]
    fn a_payload_too_short_for_a_header_is_skipped() {
        let mut buf = 3u32.to_ne_bytes().to_vec();
        buf.extend_from_slice(&[9, 9, 9]);
        buf.extend(frame(4, 5, b"ok"));
        let mut f = Framer::new();
        f.push(&buf);

        assert_eq!(f.next_message().unwrap().id, 5);
    }

    #[test]
    fn a_message_with_no_rest_is_still_a_message() {
        let mut f = Framer::new();
        f.push(&frame(3, 99, &[]));

        assert_eq!(
            f.next_message(),
            Some(Message {
                op: 3,
                id: 99,
                rest: vec![]
            })
        );
    }

    #[test]
    fn nothing_comes_out_of_nothing() {
        let mut f = Framer::new();
        assert_eq!(f.next_message(), None);
        f.push(&[1, 2, 3]);
        assert_eq!(f.next_message(), None);
    }

    #[test]
    fn every_plane_has_its_tag_and_screen() {
        assert_eq!(plane_place(PLANE_MAIN), Some(("main".into(), "main")));
        assert_eq!(plane_place(PLANE_VIDEO), Some(("video".into(), "main")));
        assert_eq!(plane_place(PLANE_CAMERA), Some(("camera".into(), "main")));
        assert_eq!(
            plane_place(CLUSTER_PLANE_MIN + 1),
            Some(("cluster-cluster".into(), "cluster"))
        );
        assert_eq!(
            plane_place(CLUSTER_PLANE_MAX),
            Some(("cluster-aux".into(), "aux"))
        );
        assert_eq!(plane_place(PLANE_CLUSTER_RECV), None);
        assert_eq!(plane_place(CLUSTER_PLANE_MAX + 1), None);
    }

    #[test]
    fn a_camera_order_and_the_camera_list_come_back_apart() {
        let order = CameraOrder {
            device: "/dev/video0".into(),
            rotation: 270,
            mirror: true,
        };
        assert_eq!(parse_camera_body(&camera_body(&order)), Some(order));
        assert_eq!(parse_camera_body(&[1]), None);
        let seen = vec![
            CameraSeen {
                id: "/dev/video0".into(),
                name: "USB\tCam".into(),
            },
            CameraSeen {
                id: "0x1420000".into(),
                name: "FaceTime HD".into(),
            },
        ];
        let back = parse_cameras_body(&cameras_body(&seen));
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].name, "USB Cam");
        assert_eq!(back[1], seen[1]);
        assert!(parse_cameras_body(&[]).is_empty());
    }

    #[test]
    fn a_plane_body_comes_back_apart() {
        let body = plane_body("h265", &[1, 2]);
        assert_eq!(body, [&[4u8][..], b"h265", &[1, 2]].concat());
        assert_eq!(
            parse_plane_body(&body),
            Some(("h265".into(), &[1u8, 2][..]))
        );
        assert_eq!(parse_plane_body(&[4, b'h', b'2']), None);
        assert_eq!(parse_plane_body(&[]), None);
    }

    #[test]
    fn a_url_body_comes_back_apart() {
        let body = url_body("http://127.0.0.1:9/a.m3u8", "speakers");
        assert_eq!(&body[..2], &[8, 0]);
        assert_eq!(
            parse_url_body(&body),
            Some(("http://127.0.0.1:9/a.m3u8".into(), "speakers".into()))
        );
        assert_eq!(
            parse_url_body(&url_body("http://x/", "")),
            Some(("http://x/".into(), "".into()))
        );
    }

    #[test]
    fn a_url_body_without_its_url_is_nothing() {
        assert_eq!(parse_url_body(&url_body("", "speakers")), None);
        assert_eq!(parse_url_body(&[9, 0, b's']), None);
        assert_eq!(parse_url_body(&[1]), None);
        assert_eq!(parse_url_body(&[0, 0, 0xff]), None);
    }

    #[test]
    fn a_reply_carries_its_length_ahead_of_the_header() {
        let out = encode_reply(2, 0x0102_0304, &[7, 8]);

        assert_eq!(out.len(), 4 + 5 + 2);
        assert_eq!(u32::from_ne_bytes([out[0], out[1], out[2], out[3]]), 7);
        assert_eq!(out[4], 2);
        assert_eq!(
            u32::from_ne_bytes([out[5], out[6], out[7], out[8]]),
            0x0102_0304
        );
        assert_eq!(&out[9..], &[7, 8]);
    }

    #[test]
    fn a_url_status_comes_back_apart() {
        let full = UrlStatus {
            position: Some(12.5),
            duration: Some(600.0),
            seekable: Some((0.0, 600.0)),
            playing: true,
            ready: true,
            buffered_percent: Some(40),
            ended: true,
            failed: true,
        };
        assert_eq!(parse_url_status(&url_status_body(&full)), Some(full));
        let live = UrlStatus {
            position: Some(3.0),
            ready: true,
            ..Default::default()
        };
        assert_eq!(parse_url_status(&url_status_body(&live)), Some(live));
        assert_eq!(parse_url_status(&url_status_body(&live)[..20]), None);
        assert_eq!(parse_url_status(&[1]), None);
        assert_eq!(parse_url_status(&[]), None);
    }
}
