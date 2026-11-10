// SPDX-License-Identifier: GPL-3.0-or-later

use crate::{
    Error,
    config::Config,
    ipc::{AuthorizationRequest, Channel, Message, Progress, Readiness},
};
use livi_native_bluetooth::Event;
use std::time::Duration;

const GRANT_WINDOW: Duration = Duration::from_secs(5);
const BOOTSTRAP_WINDOW: Duration = Duration::from_secs(40);

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Phase {
    #[default]
    Idle,
    Authorizing,
    WaitingArrival,
    Bootstrapping,
    Active,
}

/// Owns one fresh Config and at most one transferred stream. UID checks protect
/// IPC access, not user consent or phone identity. No implicit retry/reconnect:
/// another attempt requires fresh configuration and an explicit owner request.
#[derive(Default)]
pub struct Router {
    generation: u64,
    prepared: Option<Config>,
    channel: Option<Channel>,
    deadline: Option<Duration>,
    high_water: u64,
    endpoint: Option<std::path::PathBuf>,
    progress: Option<(u64, Progress)>,
    phase: Phase,
    readiness: Option<Readiness>,
    expected_runtime: Option<u64>,
    request_started: Duration,
    peer: Option<livi_native_bluetooth::Address>,
    isolated_compatibility: bool,
}

impl Router {
    /// Only the existing explicitly authorized isolated helper omits the
    /// production authorization exchange. Never selected as an error fallback.
    #[cfg(any(test, feature = "isolated-development"))]
    pub fn isolated(directory: std::path::PathBuf) -> Self {
        let mut router = Self::at_endpoint(directory);
        router.isolated_compatibility = true;
        router
    }

    /// Same production protocol at an explicitly supplied private endpoint.
    /// Endpoint ownership/modes and same-UID authentication remain enforced.
    pub fn at_endpoint(directory: std::path::PathBuf) -> Self {
        Self {
            generation: 0,
            prepared: None,
            channel: None,
            deadline: None,
            high_water: 0,
            endpoint: Some(directory),
            progress: None,
            phase: Phase::Idle,
            readiness: None,
            expected_runtime: None,
            request_started: Duration::ZERO,
            peer: None,
            isolated_compatibility: false,
        }
    }

    pub fn progress(&self) -> Option<(u64, u64, Progress)> {
        self.progress
            .map(|(attempt, stage)| (self.generation, attempt, stage))
    }

    /// Readiness is attempt/channel-local, never a persistent hardware grant.
    /// An optional expected incarnation is checked before exposing this value.
    pub fn readiness(&self, now: Duration) -> Option<Readiness> {
        if self.deadline.is_some_and(|deadline| now >= deadline) {
            return None;
        }
        self.readiness
    }

    /// Consumes fresh validated credentials; never reads saved state or env.
    /// The first Ready pins the authenticated channel's helper incarnation.
    pub fn prepare(&mut self, generation: u64, config: Config, now: Duration) -> Result<(), Error> {
        self.prepare_inner(generation, None, config, now)
    }

    /// Callers with an independently known incarnation can require an exact
    /// match. A restart is not silently accepted on an existing attempt.
    pub fn prepare_for_runtime(
        &mut self,
        generation: u64,
        runtime: u64,
        config: Config,
        now: Duration,
    ) -> Result<(), Error> {
        if runtime == 0 {
            return Err(Error::Stale);
        }
        self.prepare_inner(generation, Some(runtime), config, now)
    }

    fn prepare_inner(
        &mut self,
        generation: u64,
        runtime: Option<u64>,
        config: Config,
        now: Duration,
    ) -> Result<(), Error> {
        if generation == 0 || generation <= self.high_water {
            return Err(Error::Stale);
        }
        if self.isolated_compatibility {
            self.cancel();
            self.high_water = generation;
            self.generation = generation;
            self.peer = Some(config.phone());
            self.prepared = Some(config);
            self.phase = Phase::WaitingArrival;
            self.deadline = Some(now.saturating_add(Duration::from_secs(90)));
            return Ok(());
        }
        self.cancel();
        // Consume the generation even if connect/send fails. Never replay it.
        self.high_water = generation;
        let channel = match &self.endpoint {
            Some(dir) => Channel::connect_isolated(dir),
            None => Channel::connect(),
        }?;
        self.begin(generation, runtime, config, channel, now)
    }

    fn begin(
        &mut self,
        generation: u64,
        runtime: Option<u64>,
        config: Config,
        channel: Channel,
        now: Duration,
    ) -> Result<(), Error> {
        self.generation = generation;
        self.high_water = generation;
        self.peer = Some(config.phone());
        self.prepared = Some(config);
        self.channel = Some(channel);
        self.expected_runtime = runtime;
        self.request_started = now;
        self.deadline = Some(now.saturating_add(GRANT_WINDOW));
        self.phase = Phase::Authorizing;
        let result = self.channel.as_mut().unwrap().send(Message::Authorize {
            generation,
            request: AuthorizationRequest { lifetime_ms: 5000 },
        });
        if result.is_err() {
            self.cancel();
        }
        result
    }

    /// Adapter for owners receiving an owned stream rather than an Event.
    /// This performs exactly the same checks as the native event path.
    pub fn route_arrival(
        &mut self,
        generation: u64,
        peer: livi_native_bluetooth::Address,
        local: livi_native_bluetooth::Address,
        stream: std::os::fd::OwnedFd,
        now: Duration,
    ) -> Result<(), (u64, Error)> {
        self.route(
            Event::Connected {
                generation,
                stream,
                peer,
                local,
            },
            now,
        )
    }

    /// A Connected event cannot send credentials or SCM_RIGHTS before Ready.
    /// Premature/duplicate current arrivals fail closed, closing their OwnedFd.
    pub fn route(&mut self, event: Event, now: Duration) -> Result<(), (u64, Error)> {
        match event {
            Event::Connected {
                generation,
                stream,
                peer,
                local,
            } => {
                let error = if generation != self.generation {
                    Some(Error::Stale)
                } else if self.deadline.is_some_and(|deadline| now >= deadline) {
                    Some(Error::TimedOut)
                } else if self.phase != Phase::WaitingArrival
                    || self.peer != Some(peer)
                    || !peer.valid()
                    || !local.valid()
                {
                    Some(Error::Stale)
                } else {
                    None
                };
                if let Some(error) = error {
                    drop(stream);
                    if generation == self.generation {
                        self.cancel();
                    }
                    return Err((generation, error));
                }
                let result = (|| {
                    if self.isolated_compatibility {
                        let channel = match &self.endpoint {
                            Some(dir) => Channel::connect_isolated(dir)?,
                            None => return Err(Error::UnsafeEndpoint),
                        };
                        self.channel = Some(channel);
                        self.channel.as_mut().unwrap().send(Message::Configure {
                            generation,
                            config: self.prepared.take().ok_or(Error::Stale)?,
                        })?;
                    }
                    self.channel
                        .as_mut()
                        .ok_or(Error::Disconnected)?
                        .send(Message::Arrival {
                            generation,
                            peer,
                            local,
                            stream,
                        })?;
                    self.phase = Phase::Bootstrapping;
                    self.deadline = Some(now.saturating_add(BOOTSTRAP_WINDOW));
                    Ok(())
                })();
                if let Err(error) = result {
                    self.cancel();
                    return Err((generation, error));
                }
            }
            Event::Failure { generation, .. } | Event::Closed { generation }
                if generation == self.generation || generation == 0 =>
            {
                self.cancel();
            }
            Event::Availability(false) => self.cancel(),
            _ => {}
        }
        Ok(())
    }

    fn receive(&mut self, message: Message, now: Duration) -> Result<(), Error> {
        match message {
            Message::Ready {
                generation,
                readiness,
            } if !self.isolated_compatibility
                && self.phase == Phase::Authorizing
                && generation == self.generation
                && readiness.valid()
                && self
                    .expected_runtime
                    .is_none_or(|runtime| runtime == readiness.runtime) =>
            {
                // Start at request transmission, not receipt: transit/UI delay
                // cannot extend a server grant. No renewal on the same attempt.
                let deadline = self
                    .request_started
                    .saturating_add(Duration::from_millis(u64::from(readiness.lifetime_ms)));
                if now >= deadline {
                    return Err(Error::TimedOut);
                }
                self.channel
                    .as_mut()
                    .ok_or(Error::Disconnected)?
                    .send(Message::Configure {
                        generation,
                        config: self.prepared.take().ok_or(Error::Stale)?,
                    })?;
                self.expected_runtime = Some(readiness.runtime);
                self.readiness = Some(readiness);
                self.phase = Phase::WaitingArrival;
                // Also bounds the wait for Arrival, conservatively earlier
                // than the helper's five-second pending-configuration deadline.
                self.deadline = Some(deadline);
            }
            Message::Progress {
                generation,
                attempt,
                stage,
            } if generation == self.generation
                && matches!(self.phase, Phase::Bootstrapping | Phase::Active) =>
            {
                let valid = match (self.progress, stage) {
                    (None, Progress::Wpp) => true,
                    (Some((id, Progress::Wpp)), Progress::Tcp) => id == attempt,
                    (Some((id, _)), Progress::Failed | Progress::Finished) => id == attempt,
                    _ => false,
                };
                if !valid || attempt == 0 {
                    return Err(Error::Stale);
                }
                self.progress = Some((attempt, stage));
                match stage {
                    Progress::Wpp => {
                        // Never extend the original whole-bootstrap deadline.
                        self.deadline = self.deadline.map(|deadline| {
                            deadline.min(now.saturating_add(Duration::from_secs(15)))
                        });
                    }
                    Progress::Tcp => {
                        self.phase = Phase::Active;
                        self.deadline = None;
                    }
                    Progress::Failed | Progress::Finished => return Err(Error::Disconnected),
                }
            }
            _ => return Err(Error::Stale),
        }
        Ok(())
    }

    pub fn tick(&mut self, now: Duration) -> Option<u64> {
        let generation = self.generation;
        if generation == 0 {
            return None;
        }
        let mut failed = self.deadline.is_some_and(|deadline| now >= deadline);
        if !failed {
            // Bounded reads also drive SCM_RIGHTS adoption ACK/deadlines. An
            // unexpected message (including another runtime's Ready) revokes
            // this attempt rather than switching helper or falling back.
            for _ in 0..8 {
                let message = match self.channel.as_mut() {
                    Some(channel) => channel.receive(),
                    None => break,
                };
                match message {
                    Ok(Some(message)) => {
                        if self.receive(message, now).is_err() {
                            failed = true;
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(_) => {
                        failed = true;
                        break;
                    }
                }
            }
        }
        if failed {
            self.cancel();
            Some(generation)
        } else {
            None
        }
    }

    pub fn cancel(&mut self) {
        if let Some(channel) = &mut self.channel {
            let _ = channel.send(Message::Cancel {
                generation: self.generation,
            });
        }
        self.channel = None;
        self.prepared = None;
        self.generation = 0;
        self.deadline = None;
        self.progress = None;
        self.phase = Phase::Idle;
        self.readiness = None;
        self.expected_runtime = None;
        self.peer = None;
    }
}

impl Drop for Router {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::fixture;
    use std::io::Read;
    use std::os::unix::net::UnixStream;

    fn start(runtime: Option<u64>) -> (Router, Channel) {
        let (a, b) = UnixStream::pair().unwrap();
        let mut helper = Channel::from_stream(b).unwrap();
        let mut router = Router::default();
        router
            .begin(
                1,
                runtime,
                fixture(),
                Channel::from_stream(a).unwrap(),
                Duration::ZERO,
            )
            .unwrap();
        assert!(matches!(
            helper.receive().unwrap(),
            Some(Message::Authorize { generation: 1, request }) if request.lifetime_ms == 5000
        ));
        (router, helper)
    }

    fn ready(helper: &mut Channel, runtime: u64, generation: u64, lifetime_ms: u32) {
        helper
            .send(Message::Ready {
                generation,
                readiness: Readiness {
                    runtime,
                    grant: 7,
                    lifetime_ms,
                },
            })
            .unwrap();
    }

    fn connected(generation: u64) -> (Event, UnixStream) {
        let (stream, phone) = UnixStream::pair().unwrap();
        phone
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        (
            Event::Connected {
                generation,
                stream: stream.into(),
                peer: fixture().phone(),
                local: fixture().phone(),
            },
            phone,
        )
    }

    fn configured() -> (Router, Channel) {
        let (mut router, mut helper) = start(Some(73));
        ready(&mut helper, 73, 1, 5000);
        assert_eq!(router.tick(Duration::from_secs(1)), None);
        assert!(matches!(
            helper.receive().unwrap(),
            Some(Message::Configure { generation: 1, .. })
        ));
        (router, helper)
    }

    #[test]
    fn authorization_precedes_credentials_and_arrival_and_exact_ack_releases_alias() {
        let (mut router, mut helper) = start(Some(73));
        assert!(helper.receive().unwrap().is_none());
        assert!(router.readiness(Duration::ZERO).is_none());
        ready(&mut helper, 73, 1, 5000);
        assert_eq!(router.tick(Duration::from_secs(1)), None);
        assert_eq!(
            router.readiness(Duration::from_secs(1)).unwrap().runtime,
            73
        );
        assert!(router.prepared.is_none());
        assert!(matches!(
            helper.receive().unwrap(),
            Some(Message::Configure { .. })
        ));
        assert!(helper.receive().unwrap().is_none());
        let (event, mut phone) = connected(1);
        router.route(event, Duration::from_secs(2)).unwrap();
        let Some(Message::Arrival { stream, .. }) = helper.receive().unwrap() else {
            panic!()
        };
        assert_eq!(router.tick(Duration::from_secs(2)), None);
        router.cancel();
        assert!(matches!(
            helper.receive().unwrap(),
            Some(Message::Cancel { generation: 1 })
        ));
        // ACK transferred ownership; channel cancellation is not global shutdown.
        drop(stream);
        assert_eq!(phone.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn wrong_runtime_generation_duplicate_or_wrong_role_fails_closed() {
        for case in 0..4 {
            let (mut router, mut helper) = start(Some(73));
            match case {
                0 => ready(&mut helper, 74, 1, 5000),
                1 => ready(&mut helper, 73, 2, 5000),
                2 => {
                    ready(&mut helper, 73, 1, 5000);
                    ready(&mut helper, 73, 1, 5000);
                }
                _ => helper
                    .send(Message::Authorize {
                        generation: 1,
                        request: AuthorizationRequest { lifetime_ms: 5000 },
                    })
                    .unwrap(),
            }
            assert_eq!(router.tick(Duration::from_secs(1)), Some(1));
            assert!(router.readiness(Duration::from_secs(1)).is_none());
            assert!(router.prepared.is_none());
        }
    }

    #[test]
    fn expiry_is_measured_from_request_and_cannot_be_extended_by_delayed_ready() {
        for (lifetime_ms, now) in [(1, Duration::from_millis(1)), (5000, GRANT_WINDOW)] {
            let (mut router, mut helper) = start(None);
            ready(&mut helper, 73, 1, lifetime_ms);
            assert_eq!(router.tick(now), Some(1));
            assert!(!matches!(
                helper.receive().unwrap(),
                Some(Message::Configure { .. })
            ));
            assert!(router.prepared.is_none());
        }
        let (mut router, _helper) = configured();
        assert!(router.readiness(GRANT_WINDOW).is_none());
        let (event, mut phone) = connected(1);
        assert_eq!(router.route(event, GRANT_WINDOW), Err((1, Error::TimedOut)));
        assert_eq!(phone.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn premature_duplicate_and_stale_arrivals_close_only_owned_descriptors() {
        let (mut router, _helper) = start(None);
        let (event, mut phone) = connected(1);
        assert_eq!(router.route(event, Duration::ZERO), Err((1, Error::Stale)));
        assert_eq!(phone.read(&mut [0]).unwrap(), 0);
        let (mut router, mut helper) = configured();
        let (stale, mut old_phone) = connected(2);
        assert_eq!(
            router.route(stale, Duration::from_secs(1)),
            Err((2, Error::Stale))
        );
        assert_eq!(old_phone.read(&mut [0]).unwrap(), 0);
        assert!(router.readiness(Duration::from_secs(1)).is_some());
        let (event, mut phone) = connected(1);
        router.route(event, Duration::from_secs(1)).unwrap();
        let Some(Message::Arrival { stream, .. }) = helper.receive().unwrap() else {
            panic!()
        };
        router.tick(Duration::from_secs(1));
        let (duplicate, mut duplicate_phone) = connected(1);
        assert!(router.route(duplicate, Duration::from_secs(1)).is_err());
        assert_eq!(duplicate_phone.read(&mut [0]).unwrap(), 0);
        drop(stream);
        assert_eq!(phone.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn disconnect_and_helper_restart_revoke_readiness_without_reconnect() {
        let (mut router, helper) = configured();
        drop(helper);
        assert_eq!(router.tick(Duration::from_secs(2)), Some(1));
        assert!(router.channel.is_none());
        assert!(router.readiness(Duration::from_secs(2)).is_none());
        assert_eq!(router.tick(Duration::from_secs(3)), None);
        assert_eq!(
            router.prepare_for_runtime(1, 73, fixture(), Duration::from_secs(3)),
            Err(Error::Stale)
        );
        assert_eq!(
            router.prepare_for_runtime(2, 0, fixture(), Duration::from_secs(3)),
            Err(Error::Stale)
        );
    }

    #[test]
    fn cancellation_availability_loss_and_stale_close_are_generation_scoped() {
        for event in [
            Event::Availability(false),
            Event::Closed { generation: 1 },
            Event::Failure {
                generation: 0,
                error: livi_native_bluetooth::Error::Transport,
            },
        ] {
            let (mut router, mut helper) = configured();
            router
                .route(Event::Closed { generation: 2 }, Duration::from_secs(1))
                .unwrap();
            assert!(router.readiness(Duration::from_secs(1)).is_some());
            router.route(event, Duration::from_secs(1)).unwrap();
            assert!(router.channel.is_none());
            assert!(matches!(
                helper.receive().unwrap(),
                Some(Message::Cancel { generation: 1 })
            ));
        }
    }

    #[test]
    fn progress_requires_exact_order_attempt_and_generation_and_no_deadline_extension() {
        let (mut router, mut helper) = configured();
        let (event, _phone) = connected(1);
        router.route(event, Duration::from_secs(1)).unwrap();
        let Some(Message::Arrival { stream, .. }) = helper.receive().unwrap() else {
            panic!()
        };
        for stage in [Progress::Wpp, Progress::Tcp] {
            helper
                .send(Message::Progress {
                    generation: 1,
                    attempt: 8,
                    stage,
                })
                .unwrap();
            assert_eq!(router.tick(Duration::from_secs(2)), None);
        }
        assert_eq!(router.progress(), Some((1, 8, Progress::Tcp)));
        assert_eq!(router.tick(Duration::from_secs(100)), None);
        helper
            .send(Message::Progress {
                generation: 1,
                attempt: 9,
                stage: Progress::Finished,
            })
            .unwrap();
        assert_eq!(router.tick(Duration::from_secs(101)), Some(1));
        drop(stream);
    }

    #[test]
    fn missing_ready_and_bootstrap_timeout_drop_fresh_configuration_and_stream() {
        let (mut router, _helper) = start(None);
        assert_eq!(router.tick(GRANT_WINDOW), Some(1));
        let (mut router, mut helper) = configured();
        let (event, _phone) = connected(1);
        router.route(event, Duration::from_secs(1)).unwrap();
        let Some(Message::Arrival { stream, .. }) = helper.receive().unwrap() else {
            panic!()
        };
        router.tick(Duration::from_secs(1));
        helper
            .send(Message::Progress {
                generation: 1,
                attempt: 8,
                stage: Progress::Wpp,
            })
            .unwrap();
        assert_eq!(router.tick(Duration::from_secs(40)), None);
        // WPP at t=40 cannot extend the original t=41 whole-bootstrap limit.
        assert_eq!(router.tick(Duration::from_secs(41)), Some(1));
        drop(stream);
    }

    #[test]
    fn isolated_owner_retains_configure_arrival_protocol_without_authorize() {
        use crate::ipc::Endpoint;
        use std::os::unix::fs::DirBuilderExt;
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(0);
        struct Directory(std::path::PathBuf);
        impl Drop for Directory {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir(&self.0);
            }
        }
        let directory = Directory(std::env::temp_dir().join(format!(
            "livi-isolated-router-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        )));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory.0)
            .unwrap();
        let endpoint = Endpoint::bind_isolated(&directory.0).unwrap();
        let mut router = Router::isolated(directory.0.clone());
        router.prepare(1, fixture(), Duration::ZERO).unwrap();
        assert!(endpoint.accept().unwrap().is_none());
        let (event, mut phone) = connected(1);
        router.route(event, Duration::from_secs(1)).unwrap();
        let mut helper = endpoint.accept().unwrap().unwrap();
        assert!(matches!(
            helper.receive().unwrap(),
            Some(Message::Configure { generation: 1, .. })
        ));
        let Some(Message::Arrival { stream, .. }) = helper.receive().unwrap() else {
            panic!()
        };
        assert_eq!(router.tick(Duration::from_secs(1)), None);
        router.cancel();
        assert!(matches!(
            helper.receive().unwrap(),
            Some(Message::Cancel { generation: 1 })
        ));
        drop(stream);
        assert_eq!(phone.read(&mut [0]).unwrap(), 0);
        drop((router, helper, endpoint));
    }

    #[test]
    fn default_router_never_selects_isolated_compatibility() {
        assert!(!Router::default().isolated_compatibility);
        assert!(!Router::at_endpoint("/tmp/unused-offline".into()).isolated_compatibility);
        assert!(Router::isolated("/tmp/unused-offline".into()).isolated_compatibility);
    }
}
