// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;
use tokio::net::{TcpStream, UnixStream};

#[tokio::test]
async fn native_session_bind_never_replaces_an_existing_endpoint() {
    let runtime = u64::from(std::process::id()) + 1000;
    let control = Control::shared(runtime, || None).unwrap();
    let (node, path) = control.bind_session(42).unwrap();
    let meta = std::fs::symlink_metadata(&path).unwrap();
    assert_eq!(meta.mode() & 0o777, 0o600);
    assert!(control.bind_session(42).is_none());
    let same = std::fs::symlink_metadata(&path).unwrap();
    assert_eq!((meta.dev(), meta.ino()), (same.dev(), same.ino()));
    let owner = SessionOwner {
        control,
        id: 42,
        path: path.clone(),
        file_id: Some((meta.dev(), meta.ino())),
    };
    drop((node, owner));
    assert!(!std::path::Path::new(&path).exists());
}

#[test]
fn shared_routes_are_disjoint_and_rejected_native_never_becomes_legacy() {
    let dongle = Arc::new(Mutex::new(Some("10.3.0.1".parse().unwrap())));
    let current = dongle.clone();
    let control = Control::shared(88, move || *current.lock().unwrap()).unwrap();
    control
        .ready("0.0.0.0:5277".parse().unwrap(), 5277)
        .unwrap();
    let now = Instant::now();
    assert!(matches!(
        control.dispatch(
            "10.3.0.2:9000".parse().unwrap(),
            "10.3.0.1:5277".parse().unwrap(),
            now
        ),
        Ok(None)
    ));
    let lease = control.reserve(3, network(), now).unwrap();
    assert!(control.dispatch(peer(), local(), now).is_err());
    lease.wpp_complete(now).unwrap();
    assert!(
        control
            .dispatch("192.168.2.3:9000".parse().unwrap(), local(), now)
            .is_err()
    );
    assert!(matches!(
        control.dispatch(
            "10.3.0.2:9000".parse().unwrap(),
            "10.3.0.1:5277".parse().unwrap(),
            now
        ),
        Ok(None)
    ));
    // A changed dongle route must not downgrade the reserved native IP.
    *dongle.lock().unwrap() = Some(network().ip());
    assert!(control.dispatch(peer(), local(), now).is_err());
    *dongle.lock().unwrap() = Some("10.3.0.1".parse().unwrap());
    let attempt = control.dispatch(peer(), local(), now).unwrap().unwrap();
    assert_eq!(attempt.generation, 3);
    lease.fail(Failure::Cancelled);
    assert!(control.dispatch(peer(), local(), now).is_err());
    control.session_ended(attempt.id);
    assert!(control.dispatch(peer(), local(), now).is_err());
    *dongle.lock().unwrap() = Some(network().ip());
    assert_eq!(
        control.reserve(4, network(), now).err(),
        Some(Failure::AddressChanged)
    );
    assert!(Control::shared(0, || None).is_err());
}

#[tokio::test]
async fn shared_listener_preserves_legacy_capacity_beyond_eight_sessions() {
    use tokio::io::AsyncReadExt;

    let listener = bind_once(Ipv4Addr::UNSPECIFIED, 0).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let control = Control::shared(92, || Some(Ipv4Addr::LOCALHOST)).unwrap();
    let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
    let worker = tokio::spawn(serve_qualified(
        listener,
        port,
        move |path, _, context| {
            events.send((path.to_owned(), context)).unwrap();
        },
        control.clone(),
    ));
    until(|| control.status().0 == ListenerState::Ready).await;
    let mut connections = Vec::new();
    let mut paths = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), async {
        for _ in 0..10 {
            let mut phone = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                .await
                .unwrap();
            let (path, context) = received.recv().await.unwrap();
            assert_eq!(context, None);
            let node = UnixStream::connect(&path).await.unwrap();
            phone.read_exact(&mut [0; 4]).await.unwrap();
            paths.push(path);
            connections.push((phone, node));
        }
    })
    .await
    .expect("native admission must not impose a new legacy session limit");
    assert_eq!(connections.len(), 10);
    assert_eq!(control.status().0, ListenerState::Ready);
    worker.abort();
    let _ = worker.await;
    until(|| {
        paths
            .iter()
            .all(|path| !std::path::Path::new(path).exists())
    })
    .await;
    drop(connections);
}

#[test]
fn native_rejection_limit_does_not_block_the_shared_dongle_route() {
    let control = Control::shared(94, || Some("10.3.0.1".parse().unwrap())).unwrap();
    control
        .ready("0.0.0.0:5277".parse().unwrap(), 5277)
        .unwrap();
    let now = Instant::now();
    let old = control.reserve(1, network(), now).unwrap();
    old.wpp_complete(now).unwrap();
    for _ in 0..REJECTION_LIMIT {
        assert!(matches!(
            control.dispatch("192.168.2.30:9000".parse().unwrap(), local(), now),
            Err(Failure::InvalidPeer)
        ));
    }
    assert!(matches!(
        control.dispatch(
            "10.3.0.2:9000".parse().unwrap(),
            "10.3.0.1:5277".parse().unwrap(),
            now,
        ),
        Ok(None)
    ));
    assert_eq!(
        control.status(),
        (ListenerState::Ready, Some(Failure::AdmissionLimit))
    );
    assert_eq!(old.phase(now), Phase::Failed(Failure::AdmissionLimit));
    assert!(control.dispatch(peer(), local(), now).is_err());
    let next = control.reserve(2, network(), now).unwrap();
    old.fail(Failure::Cancelled);
    drop(old);
    assert_eq!(next.phase(now), Phase::Wpp);
}

#[test]
fn dongle_dispatch_expires_native_deadlines_without_consuming_the_legacy_route() {
    for waiting_tcp in [false, true] {
        let control = Control::shared(93, || Some("10.3.0.1".parse().unwrap())).unwrap();
        control
            .ready("0.0.0.0:5277".parse().unwrap(), 5277)
            .unwrap();
        let now = Instant::now();
        let lease = control.reserve(1, network(), now).unwrap();
        if waiting_tcp {
            lease.wpp_complete(now).unwrap();
        }
        let (window, failure) = if waiting_tcp {
            (TCP_WINDOW, Failure::WaitingTcpTimeout)
        } else {
            (WPP_WINDOW, Failure::WppTimeout)
        };
        assert!(matches!(
            control.dispatch(
                "10.3.0.2:9000".parse().unwrap(),
                "10.3.0.1:5277".parse().unwrap(),
                now + window,
            ),
            Ok(None)
        ));
        // Inspect before phase()/reserve() can drive a compensating tick.
        assert_eq!(control.status().1, Some(failure));
        assert_eq!(lease.phase(now + window), Phase::Failed(failure));
        assert!(control.reserve(2, network(), now + window).is_ok());
    }
}

#[tokio::test]
async fn dongle_dispatch_preserves_active_native_ownership_after_pending_deadlines() {
    let control = Control::shared(95, || Some("10.3.0.1".parse().unwrap())).unwrap();
    control
        .ready("0.0.0.0:5277".parse().unwrap(), 5277)
        .unwrap();
    let now = Instant::now();
    let lease = control.reserve(1, network(), now).unwrap();
    lease.wpp_complete(now).unwrap();
    let attempt = control.dispatch(peer(), local(), now).unwrap().unwrap();
    let session = lease.handover().unwrap();
    drop(lease);

    assert!(matches!(
        control.dispatch(
            "10.3.0.2:9000".parse().unwrap(),
            "10.3.0.1:5277".parse().unwrap(),
            now + WPP_WINDOW + TCP_WINDOW,
        ),
        Ok(None)
    ));
    assert_eq!(*attempt.phase.lock().unwrap(), Phase::Active);
    assert_eq!(control.status(), (ListenerState::Ready, None));
    assert_eq!(
        control.reserve(2, network(), now).err(),
        Some(Failure::Busy)
    );

    drop(session);
    assert_eq!(
        *attempt.phase.lock().unwrap(),
        Phase::Failed(Failure::Cancelled)
    );
    control.session_ended(attempt.id);
    assert!(control.reserve(2, network(), now).is_ok());
}

#[tokio::test]
async fn one_listener_serves_native_and_dongle_in_same_run_and_cancel_is_scoped() {
    use tokio::io::AsyncReadExt;
    let listener = bind_once(Ipv4Addr::UNSPECIFIED, 0).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    // macOS does not configure 127.0.0.2 by default. A synthetic route
    // transition uses localhost without changing host interfaces. The pure
    // test above covers simultaneous dispatch on distinct destination IPs.
    let route = Arc::new(Mutex::new(Some(Ipv4Addr::LOCALHOST)));
    let current = route.clone();
    let control = Control::shared(91, move || *current.lock().unwrap()).unwrap();
    let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
    let worker = tokio::spawn(serve_qualified(
        listener,
        port,
        move |path, _, context| {
            events.send((path.to_owned(), context)).unwrap();
        },
        control.clone(),
    ));
    until(|| control.status().0 == ListenerState::Ready).await;
    let mut legacy = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    let (legacy_path, context) = received.recv().await.unwrap();
    assert_eq!(context, None);
    assert_eq!(
        std::fs::metadata(&legacy_path).unwrap().mode() & 0o777,
        0o666
    );
    *route.lock().unwrap() = None;
    let lease = control
        .reserve(7, loopback_network(), Instant::now())
        .unwrap();
    // The legacy runner continues during native WPP and TCP admission.
    let mut premature = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    assert_eq!(premature.read(&mut [0]).await.unwrap(), 0);
    assert!(received.try_recv().is_err());
    lease.wpp_complete(Instant::now()).unwrap();
    let native = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    let (native_path, context) = received.recv().await.unwrap();
    assert_eq!(context, Some(lease.context()));
    assert_eq!(
        std::fs::metadata(&native_path).unwrap().mode() & 0o777,
        0o600
    );
    let session = lease.handover().unwrap();
    drop(lease);
    assert!(std::path::Path::new(&legacy_path).exists());
    drop(session);
    until(|| !std::path::Path::new(&native_path).exists()).await;
    assert!(std::path::Path::new(&legacy_path).exists());
    // Surviving legacy runners still send AA version after native cancellation.
    let node = UnixStream::connect(&legacy_path).await.unwrap();
    let mut header = [0; 4];
    legacy.read_exact(&mut header).await.unwrap();
    // The same listener accepts the dongle route again without a restart.
    *route.lock().unwrap() = Some(Ipv4Addr::LOCALHOST);
    let mut legacy_two = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    let (legacy_two_path, context) = received.recv().await.unwrap();
    assert_eq!(context, None);
    let node_two = UnixStream::connect(&legacy_two_path).await.unwrap();
    legacy_two.read_exact(&mut header).await.unwrap();
    assert_eq!(control.status().0, ListenerState::Ready);
    drop((legacy, legacy_two, node, node_two, native));
    worker.abort();
    let _ = worker.await;
    until(|| {
        !std::path::Path::new(&legacy_path).exists()
            && !std::path::Path::new(&legacy_two_path).exists()
    })
    .await;
    assert_eq!(
        control.status().0,
        ListenerState::Failed(Failure::ListenerStopped)
    );
}

fn network() -> Network {
    Network::new(
        "192.168.1.20".parse().unwrap(),
        "255.255.255.0".parse().unwrap(),
    )
    .unwrap()
}

fn local() -> SocketAddr {
    "192.168.1.20:5277".parse().unwrap()
}

fn peer() -> SocketAddr {
    "192.168.1.30:12345".parse().unwrap()
}

fn ready() -> Control {
    let control = Control::new(Selection::Native(network()));
    control.ready(local(), 5277).unwrap();
    control
}

#[test]
fn handover_requires_active_and_preserves_scoped_cancellation() {
    let control = Control::native(73, network()).unwrap();
    control.ready(local(), 5277).unwrap();
    let now = Instant::now();
    let lease = control.reserve(9, network(), now).unwrap();
    assert_eq!(
        lease.context(),
        Context {
            runtime: 73,
            attempt: 1,
            generation: 9
        }
    );
    assert!(lease.handover().is_err());
    lease.wpp_complete(now).unwrap();
    let attempt = control.admit(peer(), local(), now).unwrap();
    let session = lease.handover().unwrap();
    assert!(lease.handover().is_err());
    drop(lease);
    assert_eq!(*attempt.phase.lock().unwrap(), Phase::Active);
    control.tick(now + Duration::from_secs(300));
    assert_eq!(*attempt.phase.lock().unwrap(), Phase::Active);
    drop(session);
    assert_eq!(
        *attempt.phase.lock().unwrap(),
        Phase::Failed(Failure::Cancelled)
    );
    control.session_ended(attempt.id);
    assert!(control.reserve(10, network(), now).is_ok());
    assert!(Control::native(0, network()).is_err());
}

#[test]
fn native_address_and_actual_contiguous_netmask_are_required() {
    for (ip, mask) in [
        ("127.0.0.1", "255.0.0.0"),
        ("0.0.0.0", "255.255.255.0"),
        ("8.8.8.8", "255.255.255.0"),
        ("192.168.1.0", "255.255.255.0"),
        ("192.168.1.255", "255.255.255.0"),
        ("192.168.1.20", "255.0.255.0"),
        ("192.168.1.20", "0.0.0.0"),
        ("192.168.1.20", "255.255.255.255"),
    ] {
        assert!(Network::new(ip.parse().unwrap(), mask.parse().unwrap()).is_err());
    }
    assert!(Network::new("10.2.3.4".parse().unwrap(), "255.255.0.0".parse().unwrap()).is_ok());
}

#[test]
fn readiness_and_wpp_completion_must_precede_tcp_acceptance() {
    let control = Control::new(Selection::Native(network()));
    let now = Instant::now();
    assert_eq!(
        control.reserve(1, network(), now).err(),
        Some(Failure::NotReady)
    );
    assert_eq!(
        control.ready("0.0.0.0:5277".parse().unwrap(), 5277),
        Err(Failure::AddressChanged)
    );
    control.ready(local(), 5277).unwrap();
    let lease = control.reserve(1, network(), now).unwrap();
    assert_eq!(lease.generation(), 1);
    assert_eq!(
        control.admit(peer(), local(), now).err(),
        Some(Failure::NotReady)
    );
    lease.wpp_complete(now).unwrap();
    let accepted = control.admit(peer(), local(), now).unwrap();
    assert_eq!(accepted.id, lease.attempt.id);
    assert_eq!(lease.phase(now), Phase::Active);
    control.session_ended(accepted.id);
    assert_eq!(lease.phase(now), Phase::Finished);
}

#[test]
fn legacy_selection_never_becomes_native_readiness() {
    let control = Control::new(Selection::Legacy);
    assert!(control.is_legacy());
    assert_eq!(
        control.reserve(1, network(), Instant::now()).err(),
        Some(Failure::NativeNotSelected)
    );
    assert_eq!(control.status().0, ListenerState::Disabled);
    let native = ready();
    assert!(!native.is_legacy());
    assert_eq!(
        native
            .reserve(
                1,
                Network::new(
                    "192.168.2.20".parse().unwrap(),
                    "255.255.255.0".parse().unwrap()
                )
                .unwrap(),
                Instant::now()
            )
            .err(),
        Some(Failure::AddressChanged)
    );
}

#[test]
fn wrong_interface_off_subnet_and_non_host_connections_are_rejected() {
    let now = Instant::now();
    let control = ready();
    let lease = control.reserve(1, network(), now).unwrap();
    lease.wpp_complete(now).unwrap();
    for (peer, local) in [
        ("192.168.2.30:1000", "192.168.1.20:5277"),
        ("192.168.1.30:1000", "192.168.2.20:5277"),
        ("192.168.1.0:1000", "192.168.1.20:5277"),
        ("192.168.1.255:1000", "192.168.1.20:5277"),
        ("192.168.1.20:1000", "192.168.1.20:5277"),
        ("[::1]:1000", "192.168.1.20:5277"),
    ] {
        assert_eq!(
            control
                .admit(peer.parse().unwrap(), local.parse().unwrap(), now)
                .err(),
            Some(Failure::InvalidPeer)
        );
    }
    assert!(control.admit(peer(), local(), now).is_ok());
}

#[test]
fn one_pending_or_active_session_is_allowed_without_replacing_it() {
    let now = Instant::now();
    let control = ready();
    let lease = control.reserve(7, network(), now).unwrap();
    assert_eq!(
        control.reserve(8, network(), now).err(),
        Some(Failure::Busy)
    );
    lease.wpp_complete(now).unwrap();
    let attempt = control.admit(peer(), local(), now).unwrap();
    assert_eq!(
        control.admit(peer(), local(), now).err(),
        Some(Failure::NotReady)
    );
    drop(lease);
    // Cancellation does not release the slot while its TCP task still owns I/O.
    assert_eq!(
        control.reserve(8, network(), now).err(),
        Some(Failure::Busy)
    );
    control.session_ended(attempt.id);
    assert!(control.reserve(8, network(), now).is_ok());
}

#[test]
fn wpp_and_waiting_tcp_deadlines_are_absolute_even_before_timer_tick() {
    let now = Instant::now();
    let control = ready();
    let lease = control.reserve(1, network(), now).unwrap();
    assert!(lease.wpp_complete(now + WPP_WINDOW).is_err());
    assert_eq!(lease.phase(now), Phase::Failed(Failure::WppTimeout));

    let lease = control.reserve(2, network(), now).unwrap();
    lease.wpp_complete(now).unwrap();
    assert!(control.admit(peer(), local(), now + TCP_WINDOW).is_err());
    assert_eq!(lease.phase(now), Phase::Failed(Failure::WaitingTcpTimeout));
}

#[test]
fn old_lease_failure_does_not_cancel_reused_generation_in_new_channel() {
    let now = Instant::now();
    let control = ready();
    let old = control.reserve(1, network(), now).unwrap();
    old.fail(Failure::WppFailed);
    let new = control.reserve(1, network(), now).unwrap();
    old.fail(Failure::Cancelled);
    drop(old);
    assert_eq!(new.phase(now), Phase::Wpp);
    new.wpp_complete(now).unwrap();
    assert!(control.admit(peer(), local(), now).is_ok());
}

#[test]
fn invalid_connection_budget_is_bounded_and_does_not_dispatch() {
    let now = Instant::now();
    let control = ready();
    let lease = control.reserve(1, network(), now).unwrap();
    lease.wpp_complete(now).unwrap();
    for _ in 0..REJECTION_LIMIT {
        assert!(
            control
                .admit("10.0.0.9:9999".parse().unwrap(), local(), now)
                .is_err()
        );
    }
    assert_eq!(lease.phase(now), Phase::Failed(Failure::AdmissionLimit));
    assert!(control.admit(peer(), local(), now).is_err());
}

#[tokio::test]
async fn conflict_does_not_replace_or_terminate_the_existing_listener() {
    let owner = bind_once(Ipv4Addr::LOCALHOST, 0).await.unwrap();
    let address = owner.local_addr().unwrap();
    assert_eq!(
        bind_once(Ipv4Addr::LOCALHOST, address.port()).await.err(),
        Some(Failure::BindConflict)
    );
    let client = TcpStream::connect(address).await.unwrap();
    assert!(owner.accept().await.is_ok());
    drop(client);
}

#[tokio::test]
async fn wrong_native_port_fails_without_creating_a_listener() {
    let control = Control::new(Selection::Native(network()));
    run(5288, |_, _| panic!("no session"), control.clone()).await;
    assert_eq!(
        control.status().0,
        ListenerState::Failed(Failure::WrongPort)
    );
    assert!(control.reserve(1, network(), Instant::now()).is_err());
}

#[tokio::test]
async fn wildcard_owner_cannot_be_taken_over_by_native_bind() {
    let owner = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).await.unwrap();
    let port = owner.local_addr().unwrap().port();
    assert_eq!(
        bind_once(Ipv4Addr::LOCALHOST, port).await.err(),
        Some(Failure::BindConflict)
    );
    assert_eq!(owner.local_addr().unwrap().port(), port);
}

fn loopback_network() -> Network {
    // Test-only sentinel: production Network::new rejects both loopback and /0.
    Network {
        ip: Ipv4Addr::LOCALHOST,
        mask: 0,
    }
}

async fn until(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn accepted_tcp_uses_existing_session_event_and_owned_cancel_cleans_socket() {
    let listener = bind_once(Ipv4Addr::LOCALHOST, 0).await.unwrap();
    let address = listener.local_addr().unwrap();
    let control = Control::new(Selection::Native(loopback_network()));
    let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
    let worker = tokio::spawn(serve(
        listener,
        address.port(),
        move |path, peer| {
            events.send((path.to_owned(), peer)).unwrap();
        },
        control.clone(),
    ));
    until(|| control.status().0 == ListenerState::Ready).await;
    let lease = control
        .reserve(1, loopback_network(), Instant::now())
        .unwrap();
    // Premature TCP is closed and emits no event.
    let mut premature = TcpStream::connect(address).await.unwrap();
    use tokio::io::AsyncReadExt;
    assert_eq!(premature.read(&mut [0]).await.unwrap(), 0);
    assert!(received.try_recv().is_err());

    lease.wpp_complete(Instant::now()).unwrap();
    let mut tcp = TcpStream::connect(address).await.unwrap();
    let (path, peer) = tokio::time::timeout(Duration::from_secs(2), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer, IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert!(std::path::Path::new(&path).exists());
    assert_eq!(lease.phase(Instant::now()), Phase::Active);
    // Attach as the existing AA manager would. No app or manager is launched.
    let node = UnixStream::connect(&path).await.unwrap();
    let mut version = [0; 4];
    tcp.read_exact(&mut version).await.unwrap();
    // Cancellation belongs to this lease, not a global wireless/USB command.
    lease.fail(Failure::Cancelled);
    until(|| !std::path::Path::new(&path).exists()).await;
    assert_eq!(
        lease.phase(Instant::now()),
        Phase::Failed(Failure::Cancelled)
    );
    drop((tcp, node, lease));
    assert!(
        control
            .reserve(2, loopback_network(), Instant::now())
            .is_ok()
    );
    worker.abort();
    let _ = worker.await;
    assert_ne!(control.status().0, ListenerState::Ready);
}

#[tokio::test]
async fn listener_owner_shutdown_closes_only_its_pending_session() {
    let listener = bind_once(Ipv4Addr::LOCALHOST, 0).await.unwrap();
    let address = listener.local_addr().unwrap();
    let control = Control::new(Selection::Native(loopback_network()));
    let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
    let worker = tokio::spawn(serve(
        listener,
        address.port(),
        move |path, _| {
            events.send(path.to_owned()).unwrap();
        },
        control.clone(),
    ));
    until(|| control.status().0 == ListenerState::Ready).await;
    let lease = control
        .reserve(1, loopback_network(), Instant::now())
        .unwrap();
    lease.wpp_complete(Instant::now()).unwrap();
    let _tcp = TcpStream::connect(address).await.unwrap();
    let path = received.recv().await.unwrap();
    worker.abort();
    let _ = worker.await;
    until(|| !std::path::Path::new(&path).exists()).await;
    assert_eq!(
        lease.phase(Instant::now()),
        Phase::Failed(Failure::ListenerStopped)
    );
    assert!(
        control
            .reserve(2, loopback_network(), Instant::now())
            .is_err()
    );
}
