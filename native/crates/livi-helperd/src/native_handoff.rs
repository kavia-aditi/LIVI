// SPDX-License-Identifier: GPL-3.0-or-later

//! Private native bootstrap; the existing production listener owns TCP.

use super::native_tcp::{NativeTcp, Permit};
use crate::aa::WiredPhones;
use livi_aa::server::admission::{Failure, Phase};
use livi_native_handoff::Error;
use livi_native_handoff::config::Config;
use livi_native_handoff::ipc::{Channel, Endpoint, Message, Progress};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

pub struct Attachment(JoinHandle<()>);

impl Drop for Attachment {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub fn attach(wired: WiredPhones, tcp: NativeTcp) -> Attachment {
    attach_at(wired, tcp, None)
}

#[allow(dead_code)]
pub fn attach_isolated(
    wired: WiredPhones,
    tcp: NativeTcp,
    directory: std::path::PathBuf,
) -> Attachment {
    attach_at(wired, tcp, Some(directory))
}

fn attach_at(
    wired: WiredPhones,
    tcp: NativeTcp,
    directory: Option<std::path::PathBuf>,
) -> Attachment {
    Attachment(tokio::spawn(async move {
        // Fails closed for root, unsafe paths, or an occupied endpoint. Never
        // retries, replaces an endpoint, or signals another helper.
        let endpoint = match directory {
            Some(dir) => Endpoint::bind_isolated(&dir),
            None => Endpoint::bind(),
        };
        let Ok(endpoint) = endpoint else {
            return;
        };
        loop {
            match endpoint.accept() {
                Ok(Some(channel)) => {
                    let _ = serve_channel(channel, wired.clone(), tcp.clone()).await;
                }
                Ok(None) | Err(Error::Unauthorized) => {}
                Err(_) => return,
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }))
}

#[cfg(not(test))]
struct Bootstrap(JoinHandle<std::io::Result<()>>);
#[cfg(test)]
struct Bootstrap(tests::r1_batch1::WorkerTask);

impl Bootstrap {
    fn new(task: JoinHandle<std::io::Result<()>>) -> Self {
        #[cfg(not(test))]
        { Self(task) }
        #[cfg(test)]
        { Self(tests::r1_batch1::WorkerTask::new(task)) }
    }

    async fn stop(mut self) {
        self.0.abort();
        #[cfg(not(test))]
        let _ = (&mut self.0).await;
        #[cfg(test)]
        let _ = self.0.join().await;
    }
}

impl Drop for Bootstrap {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn serve_channel(
    mut channel: Channel,
    wired: WiredPhones,
    tcp: NativeTcp,
) -> Result<(), Error> {
    let mut high_water = 0;
    let mut authorization_high_water = 0;
    let mut authorization = None;
    let mut pending: Option<(u64, Config, Permit, Instant)> = None;
    let mut active: Option<Bootstrap> = None;
    let mut last_message = Instant::now();
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel(8);
    let mut admitted = false;
    let result = 'serve: loop {
        while let Ok((generation, attempt, stage)) = progress_rx.try_recv() {
            if generation == high_water {
                if let Err(error) = channel.send(Message::Progress {
                    generation,
                    attempt,
                    stage,
                }) {
                    break 'serve Err(error);
                }
                admitted = stage == Progress::Tcp;
            }
        }
        if pending
            .as_ref()
            .is_some_and(|(_, _, _, since)| since.elapsed() >= Duration::from_secs(5))
        {
            pending = None;
        }
        if active.as_ref().is_some_and(|job| job.0.is_finished()) {
            active = None;
            admitted = false;
        }
        // Idle peers and partial frames cannot retain a bootstrap indefinitely.
        if !admitted && last_message.elapsed() >= Duration::from_secs(45) {
            break Err(Error::TimedOut);
        }
        let message = match channel.receive() {
            Ok(message) => message,
            Err(error) => break Err(error),
        };
        if let Some(message) = message {
            last_message = Instant::now();
            match message {
                Message::Progress { .. } | Message::Ready { .. } => break Err(Error::Malformed),
                Message::Authorize {
                    generation,
                    request,
                } => {
                    if generation <= authorization_high_water
                        || generation <= high_water
                        || pending.is_some()
                        || active.is_some()
                    {
                        break Err(Error::Stale);
                    }
                    authorization_high_water = generation;
                    let grant = match tcp.authorize(generation, request, Instant::now()) {
                        Ok(grant) => grant,
                        Err(_) => break Err(Error::Unauthorized),
                    };
                    channel.send(Message::Ready {
                        generation,
                        readiness: grant.readiness(),
                    })?;
                    authorization = Some(grant);
                }
                Message::Configure { generation, config } => {
                    if generation <= high_water {
                        if tcp.requires_authorization() {
                            break Err(Error::Stale);
                        }
                        continue;
                    }
                    high_water = generation;
                    admitted = false;
                    if let Some(job) = active.take() {
                        job.stop().await;
                    }
                    drop(pending.take());
                    // No WPP advertisement can run before the existing listener
                    // has a verified bind. The production native gate is false.
                    let permit =
                        match tcp.reserve_authorized(generation, &config, authorization.take()) {
                            Ok(permit) => permit,
                            Err(_) => {
                                // Adopt/drop queued SCM_RIGHTS explicitly before
                                // closing the private channel. Never await more
                                // input or let a rejected peer retain a worker.
                                for _ in 0..32 {
                                    match channel.receive() {
                                        Ok(Some(message)) => drop(message),
                                        _ => break,
                                    }
                                }
                                break Err(Error::Configuration);
                            }
                        };
                    pending = Some((generation, config, permit, Instant::now()));
                }
                Message::Arrival {
                    generation,
                    peer,
                    local,
                    stream,
                } => {
                    if pending.as_ref().is_none_or(|(want, cfg, _, _)| {
                        *want != generation || cfg.phone() != peer || !local.valid()
                    }) {
                        // The stale arrival's OwnedFd closes here, not the
                        // current worker's descriptor.
                        continue;
                    }
                    let (_, config, permit, _) = pending.take().unwrap();
                    let arrival = (livi_runtime::bluetooth::arrival::Arrival {
                        peer: peer.0.map(|b| format!("{b:02X}")).join(":"),
                        local: local.0,
                        stream,
                    })
                    .into_stream();
                    let arrival = match arrival {
                        Ok(arrival) => arrival,
                        Err(_) => break Err(Error::Descriptor),
                    };
                    let wired = wired.clone();
                    let progress = progress_tx.clone();
                    active = Some(Bootstrap::new(tokio::spawn(async move {
                        let attempt = permit.attempt_id();
                        let report = |stage| {
                            progress
                                .try_send((generation, attempt, stage))
                                .map_err(|_| std::io::Error::other("native progress backpressure"))
                        };
                        let phone = config.phone().0.map(|b| format!("{b:02X}")).join(":");
                        let handshake = crate::aa::native_handshake(arrival.stream, config, &wired);
                        let result = tokio::select! {
                            result = handshake => result,
                            error = async {
                                loop {
                                    if let Phase::Failed(error) = permit.phase() {
                                        break error;
                                    }
                                    tokio::time::sleep(Duration::from_millis(10)).await;
                                }
                            } => Err(std::io::Error::other(error)),
                        };
                        let identity = match result {
                            Ok(identity) => identity,
                            Err(error) => {
                                permit.fail(Failure::WppFailed);
                                let _ = report(Progress::Failed);
                                return Err(error);
                            }
                        };
                        let ids = [phone, identity.instance_id.clone(), identity.serial.clone()];
                        if let Err(error) = permit.wpp_complete(&identity) {
                            let _ = report(Progress::Failed);
                            return Err(std::io::Error::other(error));
                        }
                        report(Progress::Wpp)?;
                        let mut session_owner = None;
                        loop {
                            if wired.contains(&[&ids[0], &ids[1], &ids[2]]) {
                                permit.fail(Failure::UsbActive);
                                return Err(std::io::Error::other(Failure::UsbActive));
                            }
                            match permit.phase() {
                                Phase::Active if session_owner.is_none() => {
                                    session_owner =
                                        permit.handover().map_err(std::io::Error::other)?;
                                    report(Progress::Tcp)?;
                                }
                                Phase::Finished => {
                                    report(Progress::Finished)?;
                                    return Ok(());
                                }
                                Phase::Failed(error) => {
                                    let _ = report(Progress::Failed);
                                    return Err(std::io::Error::other(error));
                                }
                                _ => tokio::time::sleep(Duration::from_millis(10)).await,
                            }
                        }
                    })));
                }
                Message::Cancel { generation } => {
                    if authorization
                        .as_ref()
                        .is_some_and(|grant| grant.generation() == generation)
                    {
                        authorization = None;
                    }
                    if generation == high_water {
                        pending = None;
                        admitted = false;
                        if let Some(job) = active.take() {
                            job.stop().await;
                        }
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    drop(pending);
    if let Some(job) = active.take() {
        job.stop().await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use livi_aa::wpp;
    use std::os::unix::net::UnixStream as StdStream;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;

    pub(crate) mod r1_batch1 {
        include!("native_handoff_r1_tests.rs");
    }

    struct TestDirectory(std::path::PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            use std::os::unix::fs::DirBuilderExt;
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "livi-stage4-integration-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
            ));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir(&self.0);
        }
    }

    async fn pump_until(
        router: &mut livi_native_handoff::router::Router,
        origin: Instant,
        mut done: impl FnMut(&livi_native_handoff::router::Router) -> bool,
    ) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                assert_eq!(router.tick(origin.elapsed()), None);
                if done(router) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn real_router_learns_identity_but_shared_listener_rejects_unverified_tcp() {
        use livi_aa::server::admission::offline_test_support::shared_listener;
        use livi_native_handoff::router::Router;

        let directory = TestDirectory::new();
        let endpoint = Endpoint::bind_isolated(&directory.0).unwrap();
        let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
        let (control, network, address, listener) = shared_listener(91, move |path, _, owner| {
            events.send((path.to_owned(), owner)).unwrap();
        })
        .await
        .unwrap();
        assert!(address.ip().is_loopback());
        assert_ne!(address.port(), 5277);
        let native = NativeTcp::offline_shared(control.clone(), network);
        let mut router = Router::at_endpoint(directory.0.clone());
        let origin = Instant::now();
        router
            .prepare_for_runtime(1, 91, discovery_config(), origin.elapsed())
            .unwrap();
        let channel = endpoint.accept().unwrap().unwrap();
        let handler = tokio::spawn(serve_channel(channel, WiredPhones::default(), native));
        pump_until(&mut router, origin, |router| {
            router.readiness(origin.elapsed()).is_some()
        })
        .await;
        assert_eq!(router.readiness(origin.elapsed()).unwrap().runtime, 91);

        let (stream, phone) = StdStream::pair().unwrap();
        phone.set_nonblocking(true).unwrap();
        let mut phone = UnixStream::from_std(phone).unwrap();
        router
            .route_arrival(
                1,
                config().phone(),
                livi_native_handoff::config::address("02:09:08:07:06:05").unwrap(),
                stream.into(),
                origin.elapsed(),
            )
            .unwrap();
        // Real strict helper WPP, with synthetic identity and no radio/AP.
        tokio::time::timeout(Duration::from_secs(2), async {
            packet(&mut phone).await;
            phone
                .write_all(&version("offline-instance", "offline-serial"))
                .await
                .unwrap();
            assert_eq!(
                packet(&mut phone).await,
                wpp::wifi_start_request("192.168.1.20", 5277)
            );
            let mut premature = tokio::net::TcpStream::connect(address).await.unwrap();
            assert_eq!(premature.read(&mut [0]).await.unwrap(), 0);
            assert!(received.try_recv().is_err());
            phone
                .write_all(&wpp::frame(wpp::MSG_WIFI_INFO_REQUEST, &[]))
                .await
                .unwrap();
            assert_eq!(
                packet(&mut phone).await,
                wpp::wifi_info_response(
                    "offline-ap",
                    "offline-key-only",
                    "02:11:22:33:44:55",
                    livi_wifi::Security::Wpa2,
                )
            );
            phone
                .write_all(&wpp::frame(wpp::MSG_WIFI_CONNECT_STATUS, &[8, 0]))
                .await
                .unwrap();
        })
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while router.tick(origin.elapsed()).is_none() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }).await.unwrap();
        assert!(router.readiness(origin.elapsed()).is_none());
        assert!(router.progress().is_none());

        let mut tcp = tokio::net::TcpStream::connect(address).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), tcp.read(&mut [0]))
                .await.unwrap().unwrap(),
            0
        );
        assert!(received.try_recv().is_err());
        assert_eq!(control.status().1, Some(Failure::IdentityUnverified));
        // Cancel/EOF must not make the learned labels reusable.
        router.cancel();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), handler)
                .await
                .unwrap()
                .unwrap(),
            Err(Error::Disconnected)
        );
        assert_eq!(
            control.status().0,
            livi_aa::server::admission::ListenerState::Ready
        );
        let mut stale = tokio::net::TcpStream::connect(address).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), stale.read(&mut [0]))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert!(received.try_recv().is_err());
        drop((tcp, stale, phone));
        listener.abort();
        let _ = listener.await;
        drop(endpoint);
    }

    #[tokio::test]
    async fn real_shared_admission_rejects_runtime_policy_restart_and_listener_loss() {
        use livi_aa::server::admission::offline_test_support::shared_listener;
        use livi_native_handoff::router::Router;

        for case in 0..5 {
            let directory = TestDirectory::new();
            let endpoint = Endpoint::bind_isolated(&directory.0).unwrap();
            let (control, network, _, listener) =
                shared_listener(91, |_, _, _| panic!("no arrival"))
                    .await
                    .unwrap();
            let native = if case == 1 {
                NativeTcp::production()
            } else {
                NativeTcp::offline_shared(control.clone(), network)
            };
            let mut router = Router::at_endpoint(directory.0.clone());
            let origin = Instant::now();
            router
                .prepare_for_runtime(
                    1,
                    if case == 0 { 92 } else { 91 },
                    config(),
                    origin.elapsed(),
                )
                .unwrap();
            let channel = endpoint.accept().unwrap().unwrap();
            let handler = tokio::spawn(serve_channel(channel, WiredPhones::default(), native));
            if case <= 1 {
                tokio::time::timeout(Duration::from_secs(2), async {
                    while router.tick(origin.elapsed()).is_none() {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                })
                .await
                .unwrap();
                assert!(router.readiness(origin.elapsed()).is_none());
                let _ = tokio::time::timeout(Duration::from_secs(2), handler)
                    .await
                    .unwrap();
            } else {
                pump_until(&mut router, origin, |router| {
                    router.readiness(origin.elapsed()).is_some()
                })
                .await;
                if case == 2 {
                    router.cancel();
                    assert_eq!(
                        tokio::time::timeout(Duration::from_secs(2), handler)
                            .await
                            .unwrap()
                            .unwrap(),
                        Err(Error::Disconnected)
                    );
                } else if case == 3 {
                    // Simulate helper loss by aborting only this test-owned task.
                    handler.abort();
                    let _ = handler.await;
                    assert_eq!(router.tick(origin.elapsed()), Some(1));
                    assert!(router.readiness(origin.elapsed()).is_none());
                } else {
                    listener.abort();
                    // Bound client revocation even if no further message arrives.
                    assert_eq!(
                        router.tick(origin.elapsed() + Duration::from_secs(5)),
                        Some(1)
                    );
                    let _ = tokio::time::timeout(Duration::from_secs(2), handler)
                        .await
                        .unwrap();
                }
            }
            router.cancel();
            listener.abort();
            let _ = listener.await;
            drop(endpoint);
        }
    }

    fn authorized_private() -> (Channel, JoinHandle<Result<(), Error>>) {
        let (a, b) = StdStream::pair().unwrap();
        let task = tokio::spawn(serve_channel(
            Channel::from_stream(b).unwrap(),
            WiredPhones::default(),
            NativeTcp::offline_authorized(),
        ));
        (Channel::from_stream(a).unwrap(), task)
    }

    async fn grant(tx: &mut Channel, generation: u64, lifetime_ms: u32) {
        tx.send(Message::Authorize {
            generation,
            request: livi_native_handoff::ipc::AuthorizationRequest { lifetime_ms },
        })
        .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(message) = tx.receive().unwrap() {
                    assert!(
                        matches!(message, Message::Ready { generation: got, readiness }
                        if got == generation && readiness.valid())
                    );
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn production_rejects_authorization_even_for_same_uid_and_valid_request() {
        let (a, b) = StdStream::pair().unwrap();
        let mut tx = Channel::from_stream(a).unwrap();
        tx.send(Message::Authorize {
            generation: 1,
            request: livi_native_handoff::ipc::AuthorizationRequest { lifetime_ms: 5000 },
        })
        .unwrap();
        assert_eq!(
            serve_channel(
                Channel::from_stream(b).unwrap(),
                WiredPhones::default(),
                NativeTcp::production()
            )
            .await,
            Err(Error::Unauthorized)
        );
        assert_eq!(tx.receive().err(), Some(Error::Disconnected));
    }

    #[tokio::test]
    async fn authorization_replay_wrong_role_expiry_and_cancel_fail_closed() {
        for case in 0..5 {
            let (mut tx, task) = authorized_private();
            if case == 0 {
                tx.send(Message::Ready {
                    generation: 1,
                    readiness: livi_native_handoff::ipc::Readiness {
                        runtime: 1,
                        grant: 1,
                        lifetime_ms: 5000,
                    },
                })
                .unwrap();
                assert_eq!(task.await.unwrap(), Err(Error::Malformed));
                continue;
            }
            grant(&mut tx, 1, if case == 2 { 1 } else { 5000 }).await;
            if case == 1 {
                tx.send(Message::Authorize {
                    generation: 1,
                    request: livi_native_handoff::ipc::AuthorizationRequest { lifetime_ms: 5000 },
                })
                .unwrap();
                assert_eq!(task.await.unwrap(), Err(Error::Stale));
                continue;
            }
            if case == 2 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            if case == 3 {
                tx.send(Message::Cancel { generation: 1 }).unwrap();
            }
            tx.send(Message::Configure {
                generation: if case == 4 { 2 } else { 1 },
                config: config(),
            })
            .unwrap();
            assert_eq!(task.await.unwrap(), Err(Error::Configuration));
        }
    }

    #[tokio::test]
    async fn grant_allows_one_configuration_and_cannot_be_replayed_or_cross_channel() {
        let (mut tx, task) = authorized_private();
        grant(&mut tx, 2, 5000).await;
        let (mut other, other_task) = authorized_private();
        other
            .send(Message::Configure {
                generation: 2,
                config: config(),
            })
            .unwrap();
        assert_eq!(other_task.await.unwrap(), Err(Error::Configuration));
        tx.send(Message::Configure {
            generation: 2,
            config: config(),
        })
        .unwrap();
        let mut phone = arrival(&mut tx, 2).await;
        packet(&mut phone).await;
        tx.send(Message::Configure {
            generation: 2,
            config: config(),
        })
        .unwrap();
        assert_eq!(task.await.unwrap(), Err(Error::Stale));
        assert_eq!(phone.read(&mut [0]).await.unwrap(), 0);
    }

    fn config() -> Config {
        Config::from_json(br#"{"ssid":"offline-ap","passphrase":"offline-key-only","security":"wpa2","channel":1,"bssid":"02:11:22:33:44:55","ap_ip":"192.168.1.20","port":5277,"phone_address":"02:01:02:03:04:05","instance_id":"offline-instance","serial":"offline-serial"}"#).unwrap()
    }

    fn version(instance: &str, serial: &str) -> Vec<u8> {
        fn string(tag: u8, text: &str) -> Vec<u8> {
            [vec![tag, text.len() as u8], text.as_bytes().to_vec()].concat()
        }
        let nested = string(10, instance);
        let body = [
            vec![8, 6, 16, 0, 32, 0],
            string(26, serial),
            vec![50, nested.len() as u8],
            nested,
        ]
        .concat();
        wpp::frame(wpp::MSG_WIFI_VERSION_RESPONSE, &body)
    }

    async fn packet(phone: &mut UnixStream) -> Vec<u8> {
        let mut header = [0; 4];
        phone.read_exact(&mut header).await.unwrap();
        let mut body = vec![0; u16::from_be_bytes([header[0], header[1]]) as usize];
        phone.read_exact(&mut body).await.unwrap();
        [header.to_vec(), body].concat()
    }

    fn native(wired: WiredPhones) -> (UnixStream, JoinHandle<std::io::Result<wpp::Identity>>) {
        native_config(wired, config())
    }

    fn discovery_config() -> Config {
        Config::from_json(br#"{"ssid":"offline-ap","passphrase":"offline-key-only","security":"wpa2","channel":1,"bssid":"02:11:22:33:44:55","ap_ip":"192.168.1.20","port":5277,"phone_address":"02:01:02:03:04:05"}"#).unwrap()
    }

    fn native_config(wired: WiredPhones, cfg: Config) -> (UnixStream, JoinHandle<std::io::Result<wpp::Identity>>) {
        let (stream, phone) = UnixStream::pair().unwrap();
        let task =
            tokio::spawn(
                async move { crate::aa::native_handshake(stream, cfg, &wired).await },
            );
        (phone, task)
    }

    #[tokio::test]
    async fn channel_one_advertises_2412_and_wpa2_static_ap() {
        let (mut phone, task) = native(WiredPhones::default());
        assert_eq!(
            packet(&mut phone).await,
            [0, 7, 0, 4, 8, 6, 16, 0, 32, 236, 18]
        );
        phone
            .write_all(&version("offline-instance", "offline-serial"))
            .await
            .unwrap();
        assert_eq!(
            packet(&mut phone).await,
            wpp::wifi_start_request("192.168.1.20", 5277)
        );
        phone
            .write_all(&wpp::frame(wpp::MSG_WIFI_INFO_REQUEST, &[]))
            .await
            .unwrap();
        let info = packet(&mut phone).await;
        assert_eq!(
            info,
            wpp::wifi_info_response(
                "offline-ap",
                "offline-key-only",
                "02:11:22:33:44:55",
                livi_wifi::Security::Wpa2,
            )
        );
        assert!(info.ends_with(&[32, 8, 40, 0]));
        phone
            .write_all(&wpp::frame(wpp::MSG_WIFI_CONNECT_STATUS, &[8, 0]))
            .await
            .unwrap();
        assert!(task.await.unwrap().is_ok());
        assert_eq!(phone.read(&mut [0]).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn missing_malformed_or_wrong_identity_never_gets_an_ap_offer() {
        let mut wrong_version = version("offline-instance", "offline-serial");
        wrong_version[5] = 5;
        let mut failed_version = version("offline-instance", "offline-serial");
        failed_version[9] = 1;
        for response in [
            wrong_version,
            failed_version,
            wpp::frame(wpp::MSG_WIFI_INFO_REQUEST, &[]),
            wpp::frame(wpp::MSG_WIFI_VERSION_RESPONSE, &[255]),
            wpp::frame(wpp::MSG_WIFI_VERSION_RESPONSE, &[]),
            version("different-instance", "offline-serial"),
            version("offline-instance", "different-serial"),
        ] {
            let (mut phone, task) = native(WiredPhones::default());
            packet(&mut phone).await;
            phone.write_all(&response).await.unwrap();
            assert!(task.await.unwrap().is_err());
            assert_eq!(phone.read(&mut [0]).await.unwrap(), 0);
        }
    }

    #[tokio::test]
    async fn usb_suppresses_initial_offer_and_late_credential_reply() {
        let wired = WiredPhones::default();
        wired.set(vec!["OFFLINE-SERIAL".into()]);
        let (mut phone, task) = native(wired.clone());
        assert!(task.await.unwrap().is_err());
        assert_eq!(phone.read(&mut [0]).await.unwrap(), 0);

        wired.set(Vec::new());
        let (mut phone, task) = native(wired.clone());
        packet(&mut phone).await;
        phone
            .write_all(&version("offline-instance", "offline-serial"))
            .await
            .unwrap();
        packet(&mut phone).await;
        wired.set(vec!["OFFLINE-INSTANCE".into()]);
        phone
            .write_all(&wpp::frame(wpp::MSG_WIFI_INFO_REQUEST, &[]))
            .await
            .unwrap();
        assert!(task.await.unwrap().is_err());
        assert_eq!(phone.read(&mut [0]).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn malformed_join_status_is_not_success_and_eof_is_failure() {
        let (mut phone, task) = native(WiredPhones::default());
        packet(&mut phone).await;
        phone
            .write_all(&version("offline-instance", "offline-serial"))
            .await
            .unwrap();
        packet(&mut phone).await;
        phone
            .write_all(&wpp::frame(wpp::MSG_WIFI_CONNECT_STATUS, &[255]))
            .await
            .unwrap();
        assert!(task.await.unwrap().is_err());

        let (mut phone, task) = native(WiredPhones::default());
        packet(&mut phone).await;
        drop(phone);
        assert!(task.await.unwrap().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn missing_version_times_out_without_offering_credentials() {
        let (mut phone, task) = native(WiredPhones::default());
        packet(&mut phone).await;
        assert!(task.await.unwrap().is_err());
        assert_eq!(phone.read(&mut [0]).await.unwrap(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn write_backpressure_obeys_whole_exchange_deadline() {
        let (stream, _unread_phone) = tokio::io::duplex(1);
        let error = crate::aa::native_handshake(stream, config(), &WiredPhones::default())
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }

    fn private() -> (Channel, JoinHandle<Result<(), Error>>) {
        let (a, b) = StdStream::pair().unwrap();
        let rx = Channel::from_stream(b).unwrap();
        let task = tokio::spawn(serve_channel(
            rx,
            WiredPhones::default(),
            NativeTcp::offline(),
        ));
        (Channel::from_stream(a).unwrap(), task)
    }

    async fn arrival(tx: &mut Channel, generation: u64) -> UnixStream {
        let (stream, phone) = StdStream::pair().unwrap();
        tx.send(Message::Arrival {
            generation,
            peer: config().phone(),
            local: livi_native_handoff::config::address("02:09:08:07:06:05").unwrap(),
            stream: stream.into(),
        })
        .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !tx.poll_adoption().unwrap() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        phone.set_nonblocking(true).unwrap();
        UnixStream::from_std(phone).unwrap()
    }

    #[tokio::test]
    async fn disabled_production_admission_sends_no_wpp_and_closes_arrival() {
        let (a, b) = StdStream::pair().unwrap();
        let mut tx = Channel::from_stream(a).unwrap();
        tx.send(Message::Configure {
            generation: 1,
            config: config(),
        })
        .unwrap();
        let (stream, mut phone) = StdStream::pair().unwrap();
        phone
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        tx.send(Message::Arrival {
            generation: 1,
            peer: config().phone(),
            local: livi_native_handoff::config::address("02:09:08:07:06:05").unwrap(),
            stream: stream.into(),
        })
        .unwrap();
        let tcp = NativeTcp::production();
        assert_eq!(
            serve_channel(
                Channel::from_stream(b).unwrap(),
                WiredPhones::default(),
                tcp.clone()
            )
            .await,
            Err(Error::Configuration)
        );
        assert_eq!(tcp.control().status().1, Some(Failure::Disabled));
        assert!(tx.poll_adoption().unwrap());
        assert_eq!(std::io::Read::read(&mut phone, &mut [0]).unwrap(), 0);
    }

    #[tokio::test]
    async fn private_cancel_and_eof_close_only_owned_bootstrap() {
        for cancel in [true, false] {
            let (mut tx, task) = private();
            tx.send(Message::Configure {
                generation: 1,
                config: config(),
            })
            .unwrap();
            let mut phone = arrival(&mut tx, 1).await;
            packet(&mut phone).await;
            if cancel {
                tx.send(Message::Cancel { generation: 1 }).unwrap();
            }
            drop(tx);
            assert_eq!(task.await.unwrap(), Err(Error::Disconnected));
            assert_eq!(phone.read(&mut [0]).await.unwrap(), 0);
        }
    }

    #[tokio::test]
    async fn stale_config_arrival_and_cancel_do_not_replace_current_generation() {
        let (mut tx, task) = private();
        tx.send(Message::Configure {
            generation: 2,
            config: config(),
        })
        .unwrap();
        tx.send(Message::Configure {
            generation: 1,
            config: config(),
        })
        .unwrap();
        let mut stale = arrival(&mut tx, 1).await;
        assert_eq!(stale.read(&mut [0]).await.unwrap(), 0);
        let mut phone = arrival(&mut tx, 2).await;
        packet(&mut phone).await;
        tx.send(Message::Cancel { generation: 1 }).unwrap();
        phone
            .write_all(&version("offline-instance", "offline-serial"))
            .await
            .unwrap();
        assert_eq!(
            packet(&mut phone).await,
            wpp::wifi_start_request("192.168.1.20", 5277)
        );
        tx.send(Message::Cancel { generation: 2 }).unwrap();
        assert_eq!(phone.read(&mut [0]).await.unwrap(), 0);
        drop(tx);
        assert_eq!(task.await.unwrap(), Err(Error::Disconnected));
    }
}
