// SPDX-License-Identifier: GPL-3.0-or-later

//! No listener is created here. The production AA listener owns readiness.
//! Interface inspection is passive; it does not connect or change networking.

#[cfg(test)]
use livi_aa::server::admission::Selection;
use livi_aa::server::admission::{Control, Failure, Lease, Network, Phase};
use livi_native_handoff::config::Config;
use livi_native_handoff::ipc::{AuthorizationRequest, Readiness};
use std::net::Ipv4Addr;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

// Stage 4 is not authorization to advertise or use native Bluetooth/Wi-Fi.
const NATIVE_AUTHORIZED: bool = false;

#[derive(Clone)]
pub struct NativeTcp {
    control: Control,
    authorized: bool,
    requires_authorization: bool,
    next_grant: Arc<AtomicU64>,
    #[cfg(test)]
    offline: bool,
    #[cfg(test)]
    test_network: Option<Network>,
}

/// Not constructible from a message/configuration. Consumed once on the same
/// serve_channel stack that issued it; dropping/EOF/cancel revokes it.
pub struct Authorization {
    generation: u64,
    readiness: Readiness,
    deadline: Instant,
}

impl Authorization {
    pub fn readiness(&self) -> Readiness {
        self.readiness
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn consume(self, generation: u64, runtime: u64, now: Instant) -> Result<(), Failure> {
        if generation != self.generation
            || runtime != self.readiness.runtime
            || now >= self.deadline
        {
            return Err(Failure::Stale);
        }
        Ok(())
    }
}

impl NativeTcp {
    pub fn production() -> Self {
        // Fresh helper incarnation, not an authorization secret or phone ID.
        let runtime = ((u64::from(unsafe { libc::arc4random() }) << 32)
            | u64::from(unsafe { libc::arc4random() }))
        .max(1);
        Self {
            control: Control::shared(runtime, || {
                let iface = livi_runtime::net::iface_facing(livi_link_host::link::LINK_NAME)?;
                livi_runtime::net::ipv4_of(&iface)
            })
            .expect("nonzero runtime"),
            authorized: NATIVE_AUTHORIZED,
            requires_authorization: true,
            next_grant: Arc::new(AtomicU64::new(0)),
            #[cfg(test)]
            offline: false,
            #[cfg(test)]
            test_network: None,
        }
    }

    pub fn control(&self) -> Control {
        self.control.clone()
    }

    /// Only the separately compiled isolated helper may supply this Control.
    /// Production construction remains hardware-unauthorized.
    #[cfg(any(test, feature = "isolated-helper"))]
    #[allow(dead_code)]
    pub fn isolated(control: Control) -> Self {
        Self {
            control,
            authorized: true,
            requires_authorization: false,
            next_grant: Arc::new(AtomicU64::new(0)),
            #[cfg(test)]
            offline: false,
            #[cfg(test)]
            test_network: None,
        }
    }

    pub fn requires_authorization(&self) -> bool {
        self.requires_authorization
    }

    pub fn authorize(
        &self,
        generation: u64,
        request: AuthorizationRequest,
        now: Instant,
    ) -> Result<Authorization, Failure> {
        if !self.authorized {
            self.control.report(Failure::Disabled);
            return Err(Failure::Disabled);
        }
        if generation == 0 || !request.valid() {
            return Err(Failure::Stale);
        }
        let ready = self.control.status().0 == livi_aa::server::admission::ListenerState::Ready;
        #[cfg(test)]
        let ready = ready || self.offline;
        if !ready {
            return Err(Failure::NotReady);
        }
        let grant = self
            .next_grant
            .try_update(Ordering::AcqRel, Ordering::Acquire, |id| id.checked_add(1))
            .map_err(|_| Failure::Busy)?
            + 1;
        Ok(Authorization {
            generation,
            readiness: Readiness {
                runtime: self.control.runtime().max(1),
                grant,
                lifetime_ms: request.lifetime_ms,
            },
            deadline: now + Duration::from_millis(u64::from(request.lifetime_ms)),
        })
    }

    pub fn reserve_authorized(
        &self,
        generation: u64,
        config: &Config,
        authorization: Option<Authorization>,
    ) -> Result<Permit, Failure> {
        if self.requires_authorization {
            if !self.authorized {
                self.control.report(Failure::Disabled);
                return Err(Failure::Disabled);
            }
            authorization.ok_or(Failure::Disabled)?.consume(
                generation,
                self.control.runtime().max(1),
                Instant::now(),
            )?;
        }
        self.reserve(generation, config)
    }

    fn reserve(&self, generation: u64, config: &Config) -> Result<Permit, Failure> {
        #[cfg(test)]
        if self.offline {
            return Ok(Permit::Offline);
        }
        let result = (|| {
            if !self.authorized {
                return Err(Failure::Disabled);
            }
            #[cfg(not(test))]
            let network = verify_current(config.ip())?;
            #[cfg(test)]
            let network = match self.test_network {
                Some(network) => network,
                None => verify_current(config.ip())?,
            };
            self.control
                .reserve(generation, network, Instant::now())
                .map(Permit::Listener)
        })();
        if let Err(error) = result {
            self.control.report(error);
        }
        result
    }

    #[cfg(test)]
    pub fn offline_shared(control: Control, network: Network) -> Self {
        Self {
            control,
            authorized: true,
            requires_authorization: true,
            next_grant: Arc::new(AtomicU64::new(0)),
            offline: false,
            test_network: Some(network),
        }
    }

    #[cfg(test)]
    pub fn offline_authorized() -> Self {
        let mut tcp = Self::offline();
        tcp.requires_authorization = true;
        tcp.control = Control::shared(1, || None).unwrap();
        tcp
    }

    #[cfg(test)]
    pub fn offline() -> Self {
        Self {
            control: Control::new(Selection::Legacy),
            authorized: true,
            requires_authorization: false,
            next_grant: Arc::new(AtomicU64::new(0)),
            offline: true,
            test_network: None,
        }
    }
}

pub enum Permit {
    Listener(Lease),
    #[cfg(test)]
    Offline,
}

impl Permit {
    pub fn attempt_id(&self) -> u64 {
        match self {
            Self::Listener(lease) => lease.context().attempt,
            #[cfg(test)]
            Self::Offline => 1,
        }
    }

    pub fn handover(&self) -> Result<Option<livi_aa::server::admission::SessionLease>, Failure> {
        match self {
            Self::Listener(lease) => lease.handover().map(Some),
            #[cfg(test)]
            Self::Offline => Ok(None),
        }
    }

    pub fn wpp_complete(&self, _identity: &livi_aa::wpp::Identity) -> Result<(), Failure> {
        match self {
            Self::Listener(lease) => {
                // WPP supplies labels, not proof that a later TCP peer owns
                // this Bluetooth stream. AA TLS currently has no pinned peer
                // verifier. No config pin, IP, or matching string may bypass it.
                lease.fail(Failure::IdentityUnverified);
                Err(Failure::IdentityUnverified)
            }
            #[cfg(test)]
            Self::Offline => Ok(()),
        }
    }

    pub fn phase(&self) -> Phase {
        match self {
            Self::Listener(lease) => lease.phase(Instant::now()),
            #[cfg(test)]
            Self::Offline => Phase::WaitingTcp,
        }
    }

    pub fn fail(&self, error: Failure) {
        match self {
            Self::Listener(lease) => lease.fail(error),
            #[cfg(test)]
            Self::Offline => {}
        }
    }
}

#[derive(Clone, Copy)]
struct Interface {
    ip: Ipv4Addr,
    mask: Ipv4Addr,
    flags: u32,
}

fn verify_snapshot(ip: Ipv4Addr, interfaces: &[Interface]) -> Result<Network, Failure> {
    let mut found = None;
    for interface in interfaces.iter().filter(|interface| interface.ip == ip) {
        let required = (libc::IFF_UP | libc::IFF_RUNNING | libc::IFF_BROADCAST) as u32;
        let forbidden = (libc::IFF_LOOPBACK | libc::IFF_POINTOPOINT) as u32;
        if interface.flags & required != required || interface.flags & forbidden != 0 {
            continue;
        }
        let network = Network::new(ip, interface.mask)?;
        // Ambiguous duplicate addresses are not evidence of one reachable Mac
        // interface. Fail closed instead of choosing the first one.
        if found.is_some() {
            return Err(Failure::InvalidAddress);
        }
        found = Some(network);
    }
    found.ok_or(Failure::InvalidAddress)
}

pub fn verify_current(ip: Ipv4Addr) -> Result<Network, Failure> {
    let mut head = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return Err(Failure::InvalidAddress);
    }
    struct List(*mut libc::ifaddrs);
    impl Drop for List {
        fn drop(&mut self) {
            unsafe { libc::freeifaddrs(self.0) };
        }
    }
    let _list = List(head);
    let mut interfaces = Vec::new();
    let mut current = head;
    while !current.is_null() {
        let interface = unsafe { &*current };
        current = interface.ifa_next;
        if interface.ifa_addr.is_null() || interface.ifa_netmask.is_null() {
            continue;
        }
        let addr = unsafe { &*interface.ifa_addr };
        let mask = unsafe { &*interface.ifa_netmask };
        if i32::from(addr.sa_family) != libc::AF_INET
            || i32::from(mask.sa_family) != libc::AF_INET
            || usize::from(addr.sa_len) < std::mem::size_of::<libc::sockaddr_in>()
            || usize::from(mask.sa_len) < std::mem::size_of::<libc::sockaddr_in>()
        {
            continue;
        }
        let addr = unsafe { &*interface.ifa_addr.cast::<libc::sockaddr_in>() };
        let mask = unsafe { &*interface.ifa_netmask.cast::<libc::sockaddr_in>() };
        interfaces.push(Interface {
            ip: Ipv4Addr::from(addr.sin_addr.s_addr.to_ne_bytes()),
            mask: Ipv4Addr::from(mask.sin_addr.s_addr.to_ne_bytes()),
            flags: interface.ifa_flags,
        });
    }
    verify_snapshot(ip, &interfaces)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn real_permits_reject_labels_with_or_without_pins_and_cannot_reopen_new_attempts() {
        use livi_aa::server::admission::offline_test_support::shared_listener;
        let (control, network, _, listener) =
            shared_listener(91, |_, _, _| panic!("unverified native session"))
                .await.unwrap();
        let tcp = NativeTcp::offline_shared(control.clone(), network);
        let identity = livi_aa::wpp::Identity {
            instance_id: "offline-instance".into(),
            serial: "offline-serial".into(),
        };
        for generation in 1..=2 {
            let mut value = serde_json::json!({
                "ssid": "offline-ap", "passphrase": "offline-key-only",
                "security": "wpa2", "channel": 1, "bssid": "02:11:22:33:44:55",
                "ap_ip": "192.168.1.20", "port": 5277,
                "phone_address": "02:01:02:03:04:05",
            });
            if generation == 1 {
                value["instance_id"] = identity.instance_id.clone().into();
                value["serial"] = identity.serial.clone().into();
            }
            let config = Config::from_json(&serde_json::to_vec(&value).unwrap()).unwrap();
            let grant = tcp.authorize(generation,
                AuthorizationRequest { lifetime_ms: 5000 }, Instant::now()).unwrap();
            let permit = tcp.reserve_authorized(generation, &config, Some(grant)).unwrap();
            assert_eq!(permit.wpp_complete(&identity), Err(Failure::IdentityUnverified));
            assert_eq!(permit.phase(), Phase::Failed(Failure::IdentityUnverified));
            assert_eq!(control.status().1, Some(Failure::IdentityUnverified));

            let grant = tcp.authorize(generation + 10,
                AuthorizationRequest { lifetime_ms: 5000 }, Instant::now()).unwrap();
            let next = tcp.reserve_authorized(generation + 10, &config, Some(grant)).unwrap();
            assert_ne!(permit.attempt_id(), next.attempt_id());
            assert_eq!(permit.wpp_complete(&identity), Err(Failure::IdentityUnverified));
            assert_eq!(next.phase(), Phase::Wpp);
            drop(permit);
            assert_eq!(next.phase(), Phase::Wpp);
            drop(next);
        }
        listener.abort();
        let _ = listener.await;
    }

    #[test]
    fn authorization_is_expiring_runtime_and_generation_bound_and_gate_is_independent() {
        let now = Instant::now();
        let request = AuthorizationRequest { lifetime_ms: 5000 };
        let native = NativeTcp::offline_authorized();
        let first = native.authorize(4, request, now).unwrap();
        let second = native.authorize(5, request, now).unwrap();
        assert_ne!(first.readiness().grant, second.readiness().grant);
        assert!(
            first
                .consume(4, 1, now + Duration::from_millis(4999))
                .is_ok()
        );
        assert_eq!(
            second.consume(5, 1, now + Duration::from_secs(5)),
            Err(Failure::Stale)
        );
        assert_eq!(
            native
                .authorize(6, request, now)
                .unwrap()
                .consume(7, 1, now),
            Err(Failure::Stale)
        );
        assert_eq!(
            native
                .authorize(7, request, now)
                .unwrap()
                .consume(7, 2, now),
            Err(Failure::Stale)
        );
        assert!(native.authorize(0, request, now).is_err());
        for lifetime_ms in [0, 5001] {
            assert!(
                native
                    .authorize(8, AuthorizationRequest { lifetime_ms }, now)
                    .is_err()
            );
        }
        let production = NativeTcp::production();
        assert_eq!(
            production.authorize(1, request, now).err(),
            Some(Failure::Disabled)
        );
        assert_ne!(production.control.runtime(), 0);
    }

    fn interface() -> Interface {
        Interface {
            ip: "192.168.1.20".parse().unwrap(),
            mask: "255.255.255.0".parse().unwrap(),
            flags: (libc::IFF_UP | libc::IFF_RUNNING | libc::IFF_BROADCAST) as u32,
        }
    }

    #[test]
    fn address_must_be_unique_up_running_non_loopback_with_real_netmask() {
        let good = interface();
        assert!(verify_snapshot(good.ip, &[good]).is_ok());
        assert!(verify_snapshot(good.ip, &[]).is_err());
        assert!(verify_snapshot(good.ip, &[good, good]).is_err());
        for flags in [
            0,
            good.flags & !(libc::IFF_UP as u32),
            good.flags & !(libc::IFF_RUNNING as u32),
            good.flags | libc::IFF_LOOPBACK as u32,
            good.flags | libc::IFF_POINTOPOINT as u32,
        ] {
            assert!(verify_snapshot(good.ip, &[Interface { flags, ..good }]).is_err());
        }
        for mask in ["0.0.0.0", "255.0.255.0", "255.255.255.255"] {
            assert!(
                verify_snapshot(
                    good.ip,
                    &[Interface {
                        mask: mask.parse().unwrap(),
                        ..good
                    }]
                )
                .is_err()
            );
        }
    }

    #[test]
    fn production_gate_cannot_be_enabled_by_configuration_or_environment() {
        let native = NativeTcp::production();
        let config = Config::from_json(br#"{"ssid":"offline-ap","passphrase":"offline-key-only","security":"wpa2","channel":1,"bssid":"02:11:22:33:44:55","ap_ip":"192.168.1.20","port":5277,"phone_address":"02:01:02:03:04:05","instance_id":"offline-instance","serial":"offline-serial"}"#).unwrap();
        assert_eq!(native.reserve(1, &config).err(), Some(Failure::Disabled));
        assert!(!native.control().is_legacy());
        assert!(native.requires_authorization());
        assert_eq!(
            native.control().status(),
            (
                livi_aa::server::admission::ListenerState::Binding,
                Some(Failure::Disabled)
            )
        );
    }
}
