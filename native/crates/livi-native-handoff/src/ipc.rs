// SPDX-License-Identifier: GPL-3.0-or-later

use crate::{Error, config::Config};
use livi_native_bluetooth::Address;
use std::io;
use std::mem::{size_of, zeroed};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const HEADER: usize = 32;
const LIMIT: usize = 2048;
const ASSEMBLY_TIMEOUT: Duration = Duration::from_secs(2);
const ADOPTION_TIMEOUT: Duration = Duration::from_secs(2);

struct InFlight {
    generation: u64,
    transfer: u64,
    stream: OwnedFd,
    since: Instant,
}

fn shut_down(stream: &OwnedFd) {
    // Only this channel's owned transport. Failure must also stop a receiver
    // whose ACK was lost; closing our alias alone cannot revoke its alias.
    unsafe {
        libc::shutdown(stream.as_raw_fd(), libc::SHUT_RDWR);
    }
}

/// Admission permission is not phone authentication. Production policy must
/// authorize independently of IPC UID, generation and network configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthorizationRequest {
    pub lifetime_ms: u32,
}

impl AuthorizationRequest {
    pub fn valid(self) -> bool {
        (1..=5000).contains(&self.lifetime_ms)
    }
}

/// Server-issued, channel-local readiness. The lifetime bounds configuration
/// submission only; WPP and TCP admission have their own absolute deadlines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Readiness {
    pub runtime: u64,
    pub grant: u64,
    pub lifetime_ms: u32,
}

impl Readiness {
    pub fn valid(self) -> bool {
        self.runtime != 0
            && self.grant != 0
            && AuthorizationRequest {
                lifetime_ms: self.lifetime_ms,
            }
            .valid()
    }
}

pub enum Message {
    Authorize {
        generation: u64,
        request: AuthorizationRequest,
    },
    Ready {
        generation: u64,
        readiness: Readiness,
    },
    Progress {
        generation: u64,
        attempt: u64,
        stage: Progress,
    },
    Configure {
        generation: u64,
        config: Config,
    },
    Arrival {
        generation: u64,
        peer: Address,
        local: Address,
        stream: OwnedFd,
    },
    Cancel {
        generation: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Progress {
    Wpp = 1,
    Tcp = 2,
    Finished = 3,
    Failed = 4,
}

fn uid() -> Result<libc::uid_t, Error> {
    let uid = unsafe { libc::geteuid() };
    if uid == 0 || uid != unsafe { libc::getuid() } {
        return Err(Error::Unauthorized);
    }
    Ok(uid)
}

fn authenticate(stream: &UnixStream, expected: libc::uid_t) -> Result<(), Error> {
    let (mut peer_uid, mut peer_gid) = (0, 0);
    let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut peer_uid, &mut peer_gid) };
    if result != 0 || expected == 0 || peer_uid != expected {
        return Err(Error::Unauthorized);
    }
    Ok(())
}

fn prepare(fd: &OwnedFd) -> Result<(), Error> {
    let raw = fd.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(raw, libc::F_GETFD);
        let status = libc::fcntl(raw, libc::F_GETFL);
        let on: libc::c_int = 1;
        if flags < 0
            || status < 0
            || libc::fcntl(raw, libc::F_SETFD, flags | libc::FD_CLOEXEC) < 0
            || libc::fcntl(raw, libc::F_SETFL, status | libc::O_NONBLOCK) < 0
            || libc::setsockopt(
                raw,
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                (&on as *const libc::c_int).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            ) < 0
        {
            return Err(Error::Descriptor);
        }
    }
    validate_stream(fd)
}

fn validate_stream(fd: &OwnedFd) -> Result<(), Error> {
    let mut kind: libc::c_int = 0;
    let mut len = size_of::<libc::c_int>() as libc::socklen_t;
    let mut peer: libc::sockaddr_storage = unsafe { zeroed() };
    let mut peer_len = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    let ok = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut kind as *mut libc::c_int).cast(),
            &mut len,
        ) == 0
            && kind == libc::SOCK_STREAM
            && libc::getpeername(
                fd.as_raw_fd(),
                (&mut peer as *mut libc::sockaddr_storage).cast(),
                &mut peer_len,
            ) == 0
            && peer.ss_family as libc::c_int == libc::AF_UNIX
    };
    ok.then_some(()).ok_or(Error::Descriptor)
}

fn collect_rights(header: &libc::msghdr, control: &[usize]) -> Result<Vec<OwnedFd>, Error> {
    let mut rights = Vec::new();
    let mut bad = false;
    if header.msg_control != control.as_ptr().cast_mut().cast() {
        return Err(Error::Descriptor);
    }
    unsafe {
        let begin = header.msg_control as usize;
        let end = begin + (header.msg_controllen as usize).min(size_of_val(control));
        let mut cmsg = libc::CMSG_FIRSTHDR(header);
        while !cmsg.is_null() {
            let start = cmsg as usize;
            let base = libc::CMSG_LEN(0) as usize;
            if start < begin || start > end || end - start < base {
                bad = true;
                break;
            }
            let len = (*cmsg).cmsg_len as usize;
            if len < base {
                bad = true;
                break;
            }
            // Darwin can preserve original cmsg_len when setting MSG_CTRUNC.
            // Adopt only externalized descriptors in bytes actually returned.
            let returned = len.min(end - start);
            bad |= len > returned;
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let count = (returned - base) / size_of::<libc::c_int>();
                bad |= !(returned - base).is_multiple_of(size_of::<libc::c_int>());
                for i in 0..count {
                    let raw = std::ptr::read_unaligned(
                        libc::CMSG_DATA(cmsg).cast::<libc::c_int>().add(i),
                    );
                    let fd = OwnedFd::from_raw_fd(raw);
                    bad |= prepare(&fd).is_err();
                    rights.push(fd);
                }
            } else {
                bad = true;
            }
            if len > returned {
                break;
            }
            cmsg = libc::CMSG_NXTHDR(header, cmsg);
        }
    }
    if bad || header.msg_flags & (libc::MSG_CTRUNC | libc::MSG_TRUNC) != 0 || rights.len() > 1 {
        return Err(Error::Descriptor);
    }
    Ok(rights)
}

pub struct Channel {
    stream: Option<UnixStream>,
    bytes: Vec<u8>,
    rights: Vec<OwnedFd>,
    target: usize,
    started: Option<Instant>,
    poisoned: bool,
    pending: Option<InFlight>,
    next_transfer: u64,
    received_transfer: u64,
}

impl Channel {
    pub fn from_stream(stream: UnixStream) -> Result<Self, Error> {
        authenticate(&stream, uid()?)?;
        let fd: OwnedFd = stream.into();
        prepare(&fd)?;
        Ok(Self {
            stream: Some(fd.into()),
            bytes: Vec::with_capacity(HEADER + LIMIT),
            rights: Vec::new(),
            target: HEADER,
            started: None,
            poisoned: false,
            pending: None,
            next_transfer: 0,
            received_transfer: 0,
        })
    }

    pub fn connect() -> Result<Self, Error> {
        Self::connect_in(&directory()?)
    }

    /// Caller supplies an owner-only isolated directory, never an env fallback.
    pub fn connect_isolated(dir: &Path) -> Result<Self, Error> {
        Self::connect_in(dir)
    }

    fn connect_in(dir: &Path) -> Result<Self, Error> {
        check_directory(dir)?;
        let path = dir.join("bridge.sock");
        let meta = std::fs::symlink_metadata(&path).map_err(|_| Error::Disconnected)?;
        use std::os::unix::fs::FileTypeExt;
        if !meta.file_type().is_socket() || meta.uid() != uid()? || meta.mode() & 0o777 != 0o600 {
            return Err(Error::UnsafeEndpoint);
        }
        let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
        if raw < 0 {
            return Err(Error::Disconnected);
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // Set flags before connect: a full listener backlog must not block UI.
        unsafe {
            if libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) < 0
                || libc::fcntl(raw, libc::F_SETFL, libc::O_NONBLOCK) < 0
            {
                return Err(Error::Descriptor);
            }
        }
        let bytes = path.as_os_str().as_bytes();
        let mut address: libc::sockaddr_un = unsafe { zeroed() };
        if bytes.len() >= address.sun_path.len() {
            return Err(Error::UnsafeEndpoint);
        }
        address.sun_family = libc::AF_UNIX as _;
        address.sun_len = size_of::<libc::sockaddr_un>() as _;
        for (out, byte) in address.sun_path.iter_mut().zip(bytes) {
            *out = *byte as _;
        }
        if unsafe {
            libc::connect(
                raw,
                (&address as *const libc::sockaddr_un).cast(),
                size_of::<libc::sockaddr_un>() as libc::socklen_t,
            )
        } != 0
        {
            return Err(Error::Disconnected);
        }
        Self::from_stream(fd.into())
    }

    fn poison(&mut self) {
        self.poisoned = true;
        self.abort_pending();
        self.rights.clear();
        self.bytes.fill(0);
        self.bytes.clear();
        if let Some(stream) = self.stream.take() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }

    fn abort_pending(&mut self) {
        if let Some(pending) = self.pending.take() {
            shut_down(&pending.stream);
        }
    }

    fn check_pending(&self) -> Result<(), Error> {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.since.elapsed() >= ADOPTION_TIMEOUT)
        {
            return Err(Error::TimedOut);
        }
        Ok(())
    }

    /// Drives the sender's ACK/deadline from its existing event-loop timer.
    /// No wait, retry, worker, or unbounded queue. True means no adoption pending.
    pub fn poll_adoption(&mut self) -> Result<bool, Error> {
        if self.receive()?.is_some() {
            self.poison();
            return Err(Error::Malformed);
        }
        Ok(self.pending.is_none())
    }

    /// Consumes the message on every outcome. A successful Arrival send retains
    /// its descriptor until a matching adoption ACK. A partial send fails closed.
    pub fn send(&mut self, message: Message) -> Result<(), Error> {
        if let Err(error) = self.check_pending() {
            self.poison();
            return Err(error);
        }
        let (kind, generation, peer, local, mut payload, fd) = match message {
            Message::Authorize {
                generation,
                request,
            } => {
                if !request.valid() {
                    return Err(Error::Malformed);
                }
                (
                    6,
                    generation,
                    [0; 6],
                    [0; 6],
                    request.lifetime_ms.to_be_bytes().to_vec(),
                    None,
                )
            }
            Message::Ready {
                generation,
                readiness,
            } => {
                if !readiness.valid() {
                    return Err(Error::Malformed);
                }
                let mut body = readiness.runtime.to_be_bytes().to_vec();
                body.extend_from_slice(&readiness.grant.to_be_bytes());
                body.extend_from_slice(&readiness.lifetime_ms.to_be_bytes());
                (7, generation, [0; 6], [0; 6], body, None)
            }
            Message::Progress {
                generation,
                attempt,
                stage,
            } => {
                if attempt == 0 {
                    return Err(Error::Stale);
                }
                let mut body = attempt.to_be_bytes().to_vec();
                body.push(stage as u8);
                (5, generation, [0; 6], [0; 6], body, None)
            }
            Message::Configure { generation, config } => {
                (1, generation, [0; 6], [0; 6], config.encode()?, None)
            }
            Message::Arrival {
                generation,
                peer,
                local,
                stream,
            } => {
                prepare(&stream)?;
                if !peer.valid() || !local.valid() {
                    return Err(Error::Descriptor);
                }
                (2, generation, peer.0, local.0, Vec::new(), Some(stream))
            }
            Message::Cancel { generation } => (3, generation, [0; 6], [0; 6], Vec::new(), None),
        };
        if self.poisoned || generation == 0 || payload.len() > LIMIT {
            return Err(Error::Malformed);
        }
        if fd.is_some() {
            if self.pending.is_some() {
                return Err(Error::Backpressure);
            }
            self.next_transfer = self.next_transfer.checked_add(1).ok_or(Error::Stale)?;
            payload.extend_from_slice(&self.next_transfer.to_be_bytes());
        }
        let result = self.transmit(kind, generation, peer, local, &payload, fd.as_ref());
        if let Err(error) = result {
            if let Some(stream) = &fd {
                shut_down(stream);
            }
            return Err(error);
        }
        if let Some(stream) = fd {
            self.pending = Some(InFlight {
                generation,
                transfer: self.next_transfer,
                stream,
                since: Instant::now(),
            });
        } else if kind == 3
            && self
                .pending
                .as_ref()
                .is_some_and(|pending| pending.generation == generation)
        {
            self.abort_pending();
        }
        Ok(())
    }

    fn transmit(
        &mut self,
        kind: u8,
        generation: u64,
        peer: [u8; 6],
        local: [u8; 6],
        payload: &[u8],
        fd: Option<&OwnedFd>,
    ) -> Result<(), Error> {
        let mut bytes = Vec::with_capacity(HEADER + payload.len());
        bytes.extend_from_slice(b"LNH1");
        bytes.extend_from_slice(&[kind, 0, 0, 0]);
        bytes.extend_from_slice(&generation.to_be_bytes());
        bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&peer);
        bytes.extend_from_slice(&local);
        bytes.extend_from_slice(payload);
        let mut iov = libc::iovec {
            iov_base: bytes.as_mut_ptr().cast(),
            iov_len: bytes.len(),
        };
        let mut control = [0usize; 8];
        let mut header: libc::msghdr = unsafe { zeroed() };
        header.msg_iov = &mut iov;
        header.msg_iovlen = 1;
        if let Some(fd) = fd {
            header.msg_control = control.as_mut_ptr().cast();
            header.msg_controllen = unsafe { libc::CMSG_SPACE(size_of::<libc::c_int>() as _) } as _;
            unsafe {
                let cmsg = libc::CMSG_FIRSTHDR(&header);
                (*cmsg).cmsg_level = libc::SOL_SOCKET;
                (*cmsg).cmsg_type = libc::SCM_RIGHTS;
                (*cmsg).cmsg_len = libc::CMSG_LEN(size_of::<libc::c_int>() as _) as _;
                std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast(), fd.as_raw_fd());
            }
        }
        let raw = self.stream.as_ref().ok_or(Error::Disconnected)?.as_raw_fd();
        let sent = unsafe { libc::sendmsg(raw, &header, 0) };
        let error = io::Error::last_os_error();
        bytes.fill(0);
        if sent != bytes.len() as isize {
            self.poison();
            return Err(if sent < 0 && error.kind() == io::ErrorKind::WouldBlock {
                Error::Backpressure
            } else {
                Error::Disconnected
            });
        }
        Ok(())
    }

    /// Polling preserves partially received bytes and owned ancillary fds
    /// across cancellation points. Reads never cross a frame boundary.
    pub fn receive(&mut self) -> Result<Option<Message>, Error> {
        if self.poisoned {
            return Err(Error::Disconnected);
        }
        let result = self.check_pending().and_then(|()| self.receive_inner());
        if result.is_err() {
            self.poison();
        }
        result
    }

    fn receive_inner(&mut self) -> Result<Option<Message>, Error> {
        if self
            .started
            .is_some_and(|t| t.elapsed() >= ASSEMBLY_TIMEOUT)
        {
            return Err(Error::TimedOut);
        }
        for _ in 0..32 {
            let mut bytes = [0u8; HEADER + LIMIT];
            let mut iov = libc::iovec {
                iov_base: bytes.as_mut_ptr().cast(),
                iov_len: self.target - self.bytes.len(),
            };
            // A deliberately tiny rights buffer leaks omitted externalized
            // descriptors on Darwin. Accept the full bounded kernel control
            // message into memory, then reject and close every extra right.
            let mut control = [0usize; 4096 / size_of::<usize>()];
            let mut header: libc::msghdr = unsafe { zeroed() };
            header.msg_iov = &mut iov;
            header.msg_iovlen = 1;
            header.msg_control = control.as_mut_ptr().cast();
            header.msg_controllen = size_of_val(&control) as _;
            let raw = self.stream.as_ref().ok_or(Error::Disconnected)?.as_raw_fd();
            let got = unsafe { libc::recvmsg(raw, &mut header, 0) };
            if got < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    return Ok(None);
                }
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(Error::Disconnected);
            }
            // Adopt every returned descriptor before inspecting flags/payload.
            self.rights.extend(collect_rights(&header, &control)?);
            if self.rights.len() > 1 {
                return Err(Error::Descriptor);
            }
            if got == 0 {
                return Err(Error::Disconnected);
            }
            self.started.get_or_insert_with(Instant::now);
            self.bytes.extend_from_slice(&bytes[..got as usize]);
            if self.bytes.len() == HEADER && self.target == HEADER {
                if &self.bytes[..4] != b"LNH1"
                    || self.bytes[5..8] != [0; 3]
                    || !matches!(self.bytes[4], 1..=7)
                {
                    return Err(Error::Malformed);
                }
                let len = u32::from_be_bytes(self.bytes[16..20].try_into().unwrap()) as usize;
                if len > LIMIT {
                    return Err(Error::Malformed);
                }
                self.target += len;
            }
            if self.bytes.len() != self.target {
                continue;
            }
            let generation = u64::from_be_bytes(self.bytes[8..16].try_into().unwrap());
            let peer = Address(self.bytes[20..26].try_into().unwrap());
            let local = Address(self.bytes[26..32].try_into().unwrap());
            if generation == 0 {
                return Err(Error::Stale);
            }
            let payload = &self.bytes[HEADER..];
            let message = match self.bytes[4] {
                6 if self.rights.is_empty()
                    && payload.len() == 4
                    && peer.0 == [0; 6]
                    && local.0 == [0; 6] =>
                {
                    let request = AuthorizationRequest {
                        lifetime_ms: u32::from_be_bytes(payload.try_into().unwrap()),
                    };
                    if !request.valid() {
                        return Err(Error::Malformed);
                    }
                    Some(Message::Authorize {
                        generation,
                        request,
                    })
                }
                7 if self.rights.is_empty()
                    && payload.len() == 20
                    && peer.0 == [0; 6]
                    && local.0 == [0; 6] =>
                {
                    let readiness = Readiness {
                        runtime: u64::from_be_bytes(payload[..8].try_into().unwrap()),
                        grant: u64::from_be_bytes(payload[8..16].try_into().unwrap()),
                        lifetime_ms: u32::from_be_bytes(payload[16..20].try_into().unwrap()),
                    };
                    if !readiness.valid() {
                        return Err(Error::Malformed);
                    }
                    Some(Message::Ready {
                        generation,
                        readiness,
                    })
                }
                5 if self.rights.is_empty()
                    && payload.len() == 9
                    && peer.0 == [0; 6]
                    && local.0 == [0; 6] =>
                {
                    let attempt = u64::from_be_bytes(payload[..8].try_into().unwrap());
                    if attempt == 0 {
                        return Err(Error::Stale);
                    }
                    let stage = match payload[8] {
                        1 => Progress::Wpp,
                        2 => Progress::Tcp,
                        3 => Progress::Finished,
                        4 => Progress::Failed,
                        _ => return Err(Error::Malformed),
                    };
                    Some(Message::Progress {
                        generation,
                        attempt,
                        stage,
                    })
                }
                1 if self.rights.is_empty() && peer.0 == [0; 6] && local.0 == [0; 6] => {
                    Some(Message::Configure {
                        generation,
                        config: Config::from_json(payload)?,
                    })
                }
                2 if self.rights.len() == 1
                    && payload.len() == 8
                    && peer.valid()
                    && local.valid() =>
                {
                    let transfer = u64::from_be_bytes(payload.try_into().unwrap());
                    if transfer <= self.received_transfer {
                        return Err(Error::Stale);
                    }
                    let stream = self.rights.pop().unwrap();
                    // collect_rights has adopted and validated the descriptor.
                    // This ACK promises ownership only, never WPP/TCP readiness.
                    self.transmit(4, generation, [0; 6], [0; 6], &transfer.to_be_bytes(), None)?;
                    self.received_transfer = transfer;
                    Some(Message::Arrival {
                        generation,
                        peer,
                        local,
                        stream,
                    })
                }
                3 if self.rights.is_empty()
                    && payload.is_empty()
                    && peer.0 == [0; 6]
                    && local.0 == [0; 6] =>
                {
                    Some(Message::Cancel { generation })
                }
                4 if self.rights.is_empty()
                    && payload.len() == 8
                    && peer.0 == [0; 6]
                    && local.0 == [0; 6] =>
                {
                    let transfer = u64::from_be_bytes(payload.try_into().unwrap());
                    if self.pending.as_ref().is_none_or(|pending| {
                        pending.generation != generation || pending.transfer != transfer
                    }) {
                        return Err(Error::Stale);
                    }
                    // The receiver already owns a valid alias. Normal release
                    // must NOT shut it down.
                    drop(self.pending.take());
                    None
                }
                _ => return Err(Error::Malformed),
            };
            self.bytes.fill(0);
            self.bytes.clear();
            self.target = HEADER;
            self.started = None;
            return Ok(message);
        }
        Ok(None)
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        self.poison();
    }
}

#[cfg(test)]
mod adoption_tests;

fn directory() -> Result<PathBuf, Error> {
    Ok(std::env::temp_dir().join(format!("livi-native-handoff-{}", uid()?)))
}

fn check_directory(dir: &Path) -> Result<(), Error> {
    let meta = std::fs::symlink_metadata(dir).map_err(|_| Error::UnsafeEndpoint)?;
    if !meta.is_dir() || meta.uid() != uid()? || meta.mode() & 0o777 != 0o700 {
        return Err(Error::UnsafeEndpoint);
    }
    Ok(())
}

/// Never unlinks a pre-existing endpoint. Cleanup checks inode ownership before
/// removing only this listener's socket. The empty owner-only directory stays.
pub struct Endpoint {
    listener: UnixListener,
    path: PathBuf,
    inode: (u64, u64),
}

impl Endpoint {
    pub fn bind() -> Result<Self, Error> {
        Self::bind_in(directory()?)
    }

    pub fn bind_isolated(dir: &Path) -> Result<Self, Error> {
        Self::bind_in(dir.to_path_buf())
    }

    fn bind_in(dir: PathBuf) -> Result<Self, Error> {
        uid()?;
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err(Error::UnsafeEndpoint),
        }
        check_directory(&dir)?;
        let path = dir.join("bridge.sock");
        let listener = UnixListener::bind(&path).map_err(|_| Error::Occupied)?;
        let meta = std::fs::symlink_metadata(&path).map_err(|_| Error::UnsafeEndpoint)?;
        let endpoint = Self {
            listener,
            path,
            inode: (meta.dev(), meta.ino()),
        };
        std::fs::set_permissions(&endpoint.path, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| Error::UnsafeEndpoint)?;
        endpoint
            .listener
            .set_nonblocking(true)
            .map_err(|_| Error::UnsafeEndpoint)?;
        Ok(endpoint)
    }

    pub fn accept(&self) -> Result<Option<Channel>, Error> {
        match self.listener.accept() {
            Ok((stream, _)) => Channel::from_stream(stream).map(Some),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(_) => Err(Error::Disconnected),
        }
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        if let Ok(meta) = std::fs::symlink_metadata(&self.path)
            && (meta.dev(), meta.ino()) == self.inode
            && meta.uid() == unsafe { libc::geteuid() }
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::fixture;
    use std::io::{Read, Write};

    #[test]
    fn typed_authorization_and_readiness_round_trip_with_bounded_lifetime() {
        let (mut tx, mut rx) = pair();
        for lifetime_ms in [1, 5000] {
            let request = AuthorizationRequest { lifetime_ms };
            tx.send(Message::Authorize {
                generation: 4,
                request,
            })
            .unwrap();
            assert!(matches!(rx.receive().unwrap(),
                Some(Message::Authorize { generation: 4, request: got }) if got == request));
            let readiness = Readiness {
                runtime: 9,
                grant: 12,
                lifetime_ms,
            };
            rx.send(Message::Ready {
                generation: 4,
                readiness,
            })
            .unwrap();
            assert!(matches!(tx.receive().unwrap(),
                Some(Message::Ready { generation: 4, readiness: got }) if got == readiness));
        }
        for lifetime_ms in [0, 5001, u32::MAX] {
            assert_eq!(
                tx.send(Message::Authorize {
                    generation: 1,
                    request: AuthorizationRequest { lifetime_ms },
                }),
                Err(Error::Malformed)
            );
        }
        for readiness in [
            Readiness {
                runtime: 0,
                grant: 1,
                lifetime_ms: 1,
            },
            Readiness {
                runtime: 1,
                grant: 0,
                lifetime_ms: 1,
            },
            Readiness {
                runtime: 1,
                grant: 1,
                lifetime_ms: 5001,
            },
        ] {
            assert_eq!(
                rx.send(Message::Ready {
                    generation: 1,
                    readiness
                }),
                Err(Error::Malformed)
            );
        }
        assert_eq!(
            tx.send(Message::Authorize {
                generation: 0,
                request: AuthorizationRequest { lifetime_ms: 1 },
            }),
            Err(Error::Malformed)
        );
    }

    #[test]
    fn malformed_authorization_frames_poison_channel() {
        for (kind, body) in [
            (6, Vec::new()),
            (6, 0u32.to_be_bytes().to_vec()),
            (6, 5001u32.to_be_bytes().to_vec()),
            (7, vec![0; 20]),
            (7, vec![0; 19]),
        ] {
            let (stream, mut attacker) = UnixStream::pair().unwrap();
            let mut rx = Channel::from_stream(stream).unwrap();
            let mut bytes = raw_frame(kind, body.len() as u32);
            bytes[20..32].fill(0);
            bytes.extend_from_slice(&body);
            attacker.write_all(&bytes).unwrap();
            assert_eq!(rx.receive().err(), Some(Error::Malformed));
            assert_eq!(rx.receive().err(), Some(Error::Disconnected));
        }
        // No address metadata or SCM_RIGHTS is allowed on these messages.
        for kind in [6, 7] {
            let body = if kind == 6 {
                1u32.to_be_bytes().to_vec()
            } else {
                [
                    1u64.to_be_bytes().to_vec(),
                    1u64.to_be_bytes().to_vec(),
                    1u32.to_be_bytes().to_vec(),
                ]
                .concat()
            };
            let (stream, mut attacker) = UnixStream::pair().unwrap();
            let mut rx = Channel::from_stream(stream).unwrap();
            let mut bytes = raw_frame(kind, body.len() as u32);
            bytes.extend_from_slice(&body);
            attacker.write_all(&bytes).unwrap();
            assert_eq!(rx.receive().err(), Some(Error::Malformed));
            let (stream, attacker) = UnixStream::pair().unwrap();
            let mut rx = Channel::from_stream(stream).unwrap();
            bytes[20..32].fill(0);
            let (fd, mut peer) = UnixStream::pair().unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
            let rights = vec![fd.into()];
            raw_rights(&attacker, &mut bytes, &rights);
            drop(rights);
            assert_eq!(rx.receive().err(), Some(Error::Malformed));
            assert_eq!(peer.read(&mut [0]).unwrap(), 0);
        }
    }

    pub(crate) fn pair() -> (Channel, Channel) {
        let (a, b) = UnixStream::pair().unwrap();
        (
            Channel::from_stream(a).unwrap(),
            Channel::from_stream(b).unwrap(),
        )
    }

    #[test]
    fn progress_round_trips_without_rights_and_rejects_zero_attempt() {
        let (mut tx, mut rx) = pair();
        for stage in [
            Progress::Wpp,
            Progress::Tcp,
            Progress::Finished,
            Progress::Failed,
        ] {
            tx.send(Message::Progress {
                generation: 3,
                attempt: 8,
                stage,
            })
            .unwrap();
            assert!(matches!(rx.receive().unwrap(),
                Some(Message::Progress { generation: 3, attempt: 8, stage: got }) if got == stage));
        }
        assert_eq!(
            tx.send(Message::Progress {
                generation: 3,
                attempt: 0,
                stage: Progress::Tcp
            }),
            Err(Error::Stale)
        );
    }

    fn raw_frame(kind: u8, payload_len: u32) -> Vec<u8> {
        let mut bytes = Vec::from(*b"LNH1");
        bytes.extend_from_slice(&[kind, 0, 0, 0]);
        bytes.extend_from_slice(&1u64.to_be_bytes());
        bytes.extend_from_slice(&payload_len.to_be_bytes());
        bytes.extend_from_slice(&fixture().phone().0);
        bytes.extend_from_slice(&fixture().phone().0);
        bytes
    }

    pub(super) fn raw_rights(stream: &UnixStream, bytes: &mut [u8], fds: &[OwnedFd]) {
        let mut iov = libc::iovec {
            iov_base: bytes.as_mut_ptr().cast(),
            iov_len: bytes.len(),
        };
        let space = unsafe { libc::CMSG_SPACE(std::mem::size_of_val(fds) as u32) } as usize;
        let mut control = vec![0usize; space.div_ceil(size_of::<usize>())];
        let mut header: libc::msghdr = unsafe { zeroed() };
        header.msg_iov = &mut iov;
        header.msg_iovlen = 1;
        header.msg_control = control.as_mut_ptr().cast();
        header.msg_controllen = space as _;
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&header);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(fds) as u32) as _;
            for (index, fd) in fds.iter().enumerate() {
                std::ptr::write_unaligned(
                    libc::CMSG_DATA(cmsg).cast::<libc::c_int>().add(index),
                    fd.as_raw_fd(),
                );
            }
            assert_eq!(
                libc::sendmsg(stream.as_raw_fd(), &header, 0),
                bytes.len() as isize
            );
        }
    }

    #[test]
    fn missing_rights_and_oversized_frames_are_rejected() {
        for (kind, len) in [(2, 0), (1, LIMIT as u32 + 1)] {
            let (stream, mut attacker) = UnixStream::pair().unwrap();
            let mut receiver = Channel::from_stream(stream).unwrap();
            attacker.write_all(&raw_frame(kind, len)).unwrap();
            assert_eq!(receiver.receive().err(), Some(Error::Malformed));
        }
    }

    #[test]
    fn excess_and_previously_truncated_rights_are_all_closed() {
        for count in [2, 80] {
            let (stream, attacker) = UnixStream::pair().unwrap();
            let mut receiver = Channel::from_stream(stream).unwrap();
            let mut fds = Vec::new();
            let mut phones = Vec::new();
            for _ in 0..count {
                let (fd, phone) = UnixStream::pair().unwrap();
                phone
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                fds.push(fd.into());
                phones.push(phone);
            }
            raw_rights(&attacker, &mut raw_frame(2, 0), &fds);
            drop(fds);
            assert_eq!(receiver.receive().err(), Some(Error::Descriptor));
            for mut phone in phones {
                assert_eq!(phone.read(&mut [0]).unwrap(), 0);
            }
        }
    }

    #[test]
    fn incomplete_frame_eof_releases_received_rights() {
        let (stream, attacker) = UnixStream::pair().unwrap();
        let mut receiver = Channel::from_stream(stream).unwrap();
        let (fd, mut phone) = UnixStream::pair().unwrap();
        phone
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let fds = vec![fd.into()];
        raw_rights(&attacker, &mut raw_frame(2, 0)[..8], &fds);
        drop(fds);
        assert!(receiver.receive().unwrap().is_none());
        drop(attacker);
        assert_eq!(receiver.receive().err(), Some(Error::Disconnected));
        assert_eq!(phone.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn raw_non_socket_rights_are_rejected_by_receiver() {
        let (stream, attacker) = UnixStream::pair().unwrap();
        let mut receiver = Channel::from_stream(stream).unwrap();
        let fd = std::fs::File::open("/dev/null").unwrap().into();
        raw_rights(&attacker, &mut raw_frame(2, 0), &[fd]);
        assert_eq!(receiver.receive().err(), Some(Error::Descriptor));
    }

    #[test]
    fn truncated_ancillary_length_never_reads_past_returned_bytes() {
        use std::os::fd::IntoRawFd;
        let (fd, mut phone) = UnixStream::pair().unwrap();
        phone
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut control = [0usize; 2];
        let mut header: libc::msghdr = unsafe { zeroed() };
        header.msg_control = control.as_mut_ptr().cast();
        header.msg_controllen = unsafe { libc::CMSG_SPACE(size_of::<libc::c_int>() as _) } as _;
        header.msg_flags = libc::MSG_CTRUNC;
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&header);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(80 * size_of::<libc::c_int>() as u32) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast(), fd.into_raw_fd());
        }
        assert_eq!(
            collect_rights(&header, &control).err(),
            Some(Error::Descriptor)
        );
        assert_eq!(phone.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn valid_fragmented_cancel_and_zero_generation_rejection() {
        for generation in [0u64, 1] {
            let (stream, mut sender) = UnixStream::pair().unwrap();
            let mut receiver = Channel::from_stream(stream).unwrap();
            let mut bytes = raw_frame(3, 0);
            bytes[20..32].fill(0);
            bytes[8..16].copy_from_slice(&generation.to_be_bytes());
            sender.write_all(&bytes[..7]).unwrap();
            assert!(receiver.receive().unwrap().is_none());
            sender.write_all(&bytes[7..]).unwrap();
            if generation == 0 {
                assert_eq!(receiver.receive().err(), Some(Error::Stale));
            } else {
                assert!(matches!(
                    receiver.receive().unwrap(),
                    Some(Message::Cancel { generation: 1 })
                ));
            }
        }
    }

    #[test]
    fn config_and_owned_descriptor_round_trip_with_cloexec() {
        let (mut tx, mut rx) = pair();
        tx.send(Message::Configure {
            generation: 1,
            config: fixture(),
        })
        .unwrap();
        assert!(matches!(
            rx.receive().unwrap(),
            Some(Message::Configure { generation: 1, .. })
        ));
        let (stream, mut phone) = UnixStream::pair().unwrap();
        tx.send(Message::Arrival {
            generation: 1,
            peer: fixture().phone(),
            local: fixture().phone(),
            stream: stream.into(),
        })
        .unwrap();
        let Some(Message::Arrival { stream, .. }) = rx.receive().unwrap() else {
            panic!()
        };
        assert!(tx.poll_adoption().unwrap());
        assert_ne!(
            unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        let mut stream = UnixStream::from(stream);
        stream.write_all(b"x").unwrap();
        let mut byte = [0];
        phone.read_exact(&mut byte).unwrap();
        assert_eq!(byte, [b'x']);
        drop(stream);
        assert_eq!(phone.read(&mut byte).unwrap(), 0);
    }

    #[test]
    fn eof_and_wrong_uid_are_rejected() {
        let (a, b) = UnixStream::pair().unwrap();
        assert_eq!(authenticate(&a, 0), Err(Error::Unauthorized));
        assert_eq!(
            authenticate(&a, unsafe { libc::geteuid() } + 1),
            Err(Error::Unauthorized)
        );
        let mut a = Channel::from_stream(a).unwrap();
        drop(b);
        assert_eq!(a.receive().err(), Some(Error::Disconnected));
    }

    #[test]
    fn malformed_fragmented_header_and_assembly_timeout_are_bounded() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let mut a = Channel::from_stream(a).unwrap();
        b.write_all(b"LNH1").unwrap();
        assert!(a.receive().unwrap().is_none());
        a.started = Some(Instant::now() - ASSEMBLY_TIMEOUT);
        assert_eq!(a.receive().err(), Some(Error::TimedOut));
        let (a, mut b) = UnixStream::pair().unwrap();
        let mut a = Channel::from_stream(a).unwrap();
        b.write_all(&[255; HEADER]).unwrap();
        assert_eq!(a.receive().err(), Some(Error::Malformed));
    }

    #[test]
    fn non_socket_descriptor_is_consumed_on_rejection() {
        let (mut tx, _) = pair();
        let fd: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        assert_eq!(
            tx.send(Message::Arrival {
                generation: 1,
                peer: fixture().phone(),
                local: fixture().phone(),
                stream: fd,
            }),
            Err(Error::Descriptor)
        );
    }

    #[test]
    fn sender_backpressure_is_fail_closed_and_bounded() {
        let (mut tx, _rx) = pair();
        let mut failed = false;
        for generation in 1..10000 {
            if let Err(error) = tx.send(Message::Configure {
                generation,
                config: fixture(),
            }) {
                assert!(matches!(error, Error::Backpressure | Error::Disconnected));
                failed = true;
                break;
            }
        }
        assert!(failed);
        assert!(tx.poisoned);
    }

    #[test]
    fn private_endpoint_refuses_unsafe_directory_and_existing_socket() {
        let dir = std::env::temp_dir().join(format!("livi-handoff-test-{}", std::process::id()));
        let endpoint = Endpoint::bind_in(dir.clone()).unwrap();
        assert_eq!(
            Endpoint::bind_in(dir.clone()).err().unwrap(),
            Error::Occupied
        );
        let _client = Channel::connect_in(&dir).unwrap();
        assert!(endpoint.accept().unwrap().is_some());
        drop(endpoint);
        assert!(!dir.join("bridge.sock").exists());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            Endpoint::bind_in(dir.clone()).err().unwrap(),
            Error::UnsafeEndpoint
        );
        std::fs::remove_dir(dir).unwrap();
    }
}
