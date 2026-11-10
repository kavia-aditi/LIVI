// SPDX-License-Identifier: GPL-3.0-or-later
//! Default-off development composition. Never calls the legacy receiver.
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use livi_aa::server::admission::{Control, ListenerState};
use livi_native_handoff::config::Config;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;
use tokio::task::JoinSet;

fn refused() -> io::Error { io::Error::other("isolated native helper refused") }

struct Options {
    directory: PathBuf,
    runtime: u64,
    config: PathBuf,
}

impl Options {
    fn parse(args: &[String]) -> io::Result<Self> {
        let mut directory = None;
        let mut runtime = None;
        let mut config = None;
        let mut isolated = false;
        let mut authorized = false;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--isolated-helper" if !isolated => isolated = true,
                "--authorize-native-hardware" if !authorized => authorized = true,
                "--runtime-dir" if directory.is_none() => {
                    i += 1;
                    directory = Some(PathBuf::from(args.get(i).ok_or_else(refused)?));
                }
                "--runtime-id" if runtime.is_none() => {
                    i += 1;
                    runtime = Some(args.get(i).ok_or_else(refused)?.parse::<u64>().map_err(|_| refused())?);
                }
                "--ap-config" if config.is_none() => {
                    i += 1;
                    config = Some(PathBuf::from(args.get(i).ok_or_else(refused)?));
                }
                _ => return Err(refused()),
            }
            i += 1;
        }
        if !isolated || !authorized || runtime.is_none_or(|id| id == 0) { return Err(refused()); }
        Ok(Self {
            directory: directory.ok_or_else(refused)?,
            runtime: runtime.ok_or_else(refused)?,
            config: config.ok_or_else(refused)?,
        })
    }
}

fn private_config(path: &Path) -> io::Result<Config> {
    let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o077 != 0 || meta.len() > 2048 { return Err(refused()); }
    let mut bytes = Vec::new();
    file.take(2049).read_to_end(&mut bytes)?;
    Config::from_json(&bytes).map_err(|_| refused())
}

fn validate_directory(directory: &Path) -> io::Result<()> {
    let uid = unsafe { libc::geteuid() };
    let meta = std::fs::symlink_metadata(directory)?;
    if uid == 0 || !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o7777 != 0o700
        || directory.as_os_str().len() > 64
        || directory.parent() != Some(Path::new("/tmp"))
        || !directory.file_name().is_some_and(|n| n.to_string_lossy().starts_with("livi-bt-dev-"))
        || std::env::var_os("XDG_RUNTIME_DIR").as_deref() != Some(directory.as_os_str())
        || std::env::var_os("LIVI_NATIVE_BT_DEV_DIR").as_deref() != Some(directory.as_os_str()) {
        return Err(refused());
    }
    for name in ["native-runtime", "aa.sock", "bridge.sock", "livi"] {
        match std::fs::symlink_metadata(directory.join(name)) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            _ => return Err(refused()),
        }
    }
    Ok(())
}

fn prepare_transport(directory: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let transport = directory.join("livi");
    // No create_dir_all/chmod: an existing child (including a link) is refused.
    std::fs::DirBuilder::new().mode(0o700).create(&transport)?;
    let meta = std::fs::symlink_metadata(transport)?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o7777 != 0o700 { return Err(refused()); }
    Ok(())
}

struct OwnedPath {
    path: PathBuf,
    identity: (u64, u64),
}
impl OwnedPath {
    fn capture(path: PathBuf) -> io::Result<Self> {
        let meta = std::fs::symlink_metadata(&path)?;
        Ok(Self { path, identity: (meta.dev(), meta.ino()) })
    }
}
impl Drop for OwnedPath {
    fn drop(&mut self) {
        if std::fs::symlink_metadata(&self.path).is_ok_and(|m| (m.dev(), m.ino()) == self.identity) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn publish_marker(directory: &Path, runtime: u64) -> io::Result<OwnedPath> {
    if runtime == 0 { return Err(refused()); }
    let marker_path = directory.join("native-runtime");
    let mut marker = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600)
        .open(&marker_path)?;
    let owned = OwnedPath::capture(marker_path)?;
    writeln!(marker, "{runtime}")?;
    marker.sync_all()?;
    Ok(owned)
}

fn same_uid(stream: &UnixStream) -> bool {
    let mut uid = 0;
    let mut gid = 0;
    unsafe {
        libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) == 0 && uid == libc::geteuid()
    }
}

async fn subscriber(mut stream: UnixStream, mut events: broadcast::Receiver<String>) -> io::Result<()> {
    if !same_uid(&stream) { return Err(refused()); }
    let mut line = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let byte = stream.read_u8().await?;
            if byte == b'\n' { break; }
            if line.len() >= 256 { return Err(refused()); }
            line.push(byte);
        }
        Ok::<_, io::Error>(())
    }).await.map_err(|_| refused())??;
    if line != b"subscribe" {
        tokio::time::timeout(Duration::from_secs(2), stream.write_all(b"{\"ok\":false,\"error\":\"unsupported\"}\n"))
            .await.map_err(|_| refused())??;
        return Ok(());
    }
    let mut probe = [0u8; 1];
    loop {
        tokio::select! {
            event = events.recv() => {
                let event = event.map_err(|_| refused())?;
                // Lag and write backpressure close the subscriber, never drop/reorder an event.
                tokio::time::timeout(Duration::from_secs(2), async {
                    stream.write_all(event.as_bytes()).await?;
                    stream.write_all(b"\n").await
                }).await.map_err(|_| refused())??;
            }
            _ = stream.read(&mut probe) => return Ok(()),
        }
    }
}

async fn serve_events(listener: UnixListener, events: broadcast::Sender<String>) -> io::Result<()> {
    let mut subscribers = JoinSet::new();
    loop {
        tokio::select! {
            Some(_) = subscribers.join_next(), if !subscribers.is_empty() => {}
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                if subscribers.len() >= 3 { drop(stream); continue; }
                subscribers.spawn(subscriber(stream, events.subscribe()));
            }
        }
    }
}

pub fn run() -> io::Result<()> {
    let options = Options::parse(&std::env::args().skip(1).collect::<Vec<_>>())?;
    validate_directory(&options.directory)?;
    let config = private_config(&options.config)?;
    prepare_transport(&options.directory)?;
    // Passive snapshot only; this does not configure or join an access point.
    let network = crate::native_tcp::verify_current(config.ip()).map_err(|_| refused())?;
    let control = Control::native_isolated(options.runtime, network, options.directory.clone())
        .map_err(|_| refused())?;
    let port = config.port();
    drop(config);
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(async move {
        let path = options.directory.join("aa.sock");
        let listener = UnixListener::bind(&path)?;
        let _socket = OwnedPath::capture(path.clone())?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let (events, _) = broadcast::channel::<String>(16);
        let publish = events.clone();
        let mut tasks = JoinSet::new();
        tasks.spawn(serve_events(listener, events));
        let observe = control.clone();
        tasks.spawn(async move {
            livi_aa::server::run_with_context(port, move |path, peer, context| {
                if let Some(context) = context.filter(|context| context.valid()) {
                    let event = serde_json::json!({
                        "event":"aa-session", "socket":path, "peer":peer.to_string(), "transport":"wifi",
                        "native":{"runtime":context.runtime,"attempt":context.attempt,"generation":context.generation}
                    });
                    let _ = publish.send(event.to_string());
                }
            }, observe).await;
            Err::<(), _>(refused())
        });
        let _handoff = crate::native_handoff::attach_isolated(
            crate::aa::WiredPhones::default(),
            crate::native_tcp::NativeTcp::isolated(control.clone()),
            options.directory.clone(),
        );
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if tasks.is_empty() || matches!(control.status().0, ListenerState::Failed(_) | ListenerState::Stopped) {
                    return Err(refused());
                }
                let bridge_ready = std::fs::symlink_metadata(options.directory.join("bridge.sock"))
                    .is_ok_and(|m| m.file_type().is_socket() && m.uid() == unsafe { libc::geteuid() });
                if control.status().0 == ListenerState::Ready && bridge_ready { return Ok(()); }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.map_err(|_| refused())??;
        let _marker = publish_marker(&options.directory, options.runtime)?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => Ok(()),
            _ = tasks.join_next() => Err(refused()),
        }
        // JoinSet and bootstrap guards abort only tasks owned by this process.
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_shared_layout_publish_child() {
        if std::env::var("LIVI_OFFLINE_SHARED_LAYOUT").as_deref() != Ok("1") { return; }
        let root = PathBuf::from(std::env::var_os("LIVI_NATIVE_BT_DEV_DIR").unwrap());
        validate_directory(&root).unwrap();
        prepare_transport(&root).unwrap();
        assert!(publish_marker(&root, 0).is_err());
        let marker = publish_marker(&root, 731).unwrap();
        assert!(publish_marker(&root, 732).is_err(), "never overwrite another incarnation");
        assert_eq!(std::fs::read_to_string(&marker.path).unwrap(), "731\n");
        // The outer fixture owns cleanup after all consumer test processes exit.
        let _fixture_owned_marker = std::mem::ManuallyDrop::new(marker);
    }

    #[test]
    fn transport_is_private_fresh_and_never_follows_a_preexisting_child() {
        use std::os::unix::fs::{DirBuilderExt, symlink};
        let root = PathBuf::from(format!("/tmp/livi-bt-dev-layout-helper-{}", std::process::id()));
        std::fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        assert!(prepare_transport(&root).is_ok());
        let child = root.join("livi");
        let meta = std::fs::symlink_metadata(&child).unwrap();
        assert_eq!(meta.mode() & 0o7777, 0o700);
        assert_eq!(meta.uid(), unsafe { libc::geteuid() });
        assert!(prepare_transport(&root).is_err());
        std::fs::remove_dir(&child).unwrap();
        symlink(&root, &child).unwrap();
        assert!(prepare_transport(&root).is_err());
        std::fs::remove_file(child).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    fn args() -> Vec<String> {
        ["--isolated-helper", "--authorize-native-hardware", "--runtime-dir",
            "/tmp/livi-bt-dev-offline", "--runtime-id", "7", "--ap-config", "/private/offline.json"]
            .map(str::to_owned).to_vec()
    }

    #[test]
    fn explicit_authorization_and_complete_nonzero_identity_are_required() {
        assert_eq!(Options::parse(&args()).unwrap().runtime, 7);
        for flag in ["--isolated-helper", "--authorize-native-hardware"] {
            let mut bad = args();
            bad.retain(|arg| arg != flag);
            assert!(Options::parse(&bad).is_err());
        }
        for id in ["0", "-1", "not-an-id"] {
            let mut bad = args();
            bad[5] = id.into();
            assert!(Options::parse(&bad).is_err());
        }
        let mut duplicate = args();
        duplicate.push("--authorize-native-hardware".into());
        assert!(Options::parse(&duplicate).is_err());
        let mut unknown = args();
        unknown.push("--production-fallback".into());
        assert!(Options::parse(&unknown).is_err());
    }
}
