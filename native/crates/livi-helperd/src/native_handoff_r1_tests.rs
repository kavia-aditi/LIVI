// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;
use livi_native_handoff::router::Router;
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;
use tokio::task::{AbortHandle, JoinError};

tokio::task_local! {
    static TASKS: TestTasks;
    static HELPER_ID: TaskId;
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Exit {
    Completed,
    Cancelled,
    Panicked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum TaskRole {
    Helper = 0,
    Listener = 1,
    Worker = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TaskId {
    scenario: &'static str,
    boundary: u8,
    incarnation: &'static str,
    role: TaskRole,
}

impl TaskId {
    fn new(
        scenario: &'static str,
        boundary: u8,
        incarnation: &'static str,
        role: TaskRole,
    ) -> Self {
        Self { scenario, boundary, incarnation, role }
    }

    fn worker(self) -> Self {
        Self { role: TaskRole::Worker, ..self }
    }
}

struct TaskRecord {
    id: TaskId,
    abort: AbortHandle,
    // This observer owns and awaits the original JoinHandle, even if the
    // scenario drops its result receiver during panic/cancellation.
    observer: JoinHandle<Exit>,
}

#[derive(Clone, Default)]
struct TestTasks(Arc<Mutex<Vec<TaskRecord>>>);

struct TestTask<T> {
    abort: AbortHandle,
    result: oneshot::Receiver<Result<T, JoinError>>,
}

impl<T> TestTask<T> {
    fn abort(&self) {
        self.abort.abort();
    }

    fn is_finished(&self) -> bool {
        self.abort.is_finished()
    }

    async fn join(&mut self) -> Result<T, JoinError> {
        (&mut self.result).await.expect("task observer must deliver its join result")
    }
}

impl<T> Drop for TestTask<T> {
    fn drop(&mut self) {
        // Join ownership stays in TestTasks, never in this result receiver.
        self.abort.abort();
    }
}

impl TestTasks {
    fn track<T: Send + 'static>(
        &self,
        id: TaskId,
        task: JoinHandle<T>,
    ) -> TestTask<T> {
        let abort = task.abort_handle();
        let (tx, result) = oneshot::channel();
        let observer = tokio::spawn(async move {
            let result = task.await;
            let exit = match &result {
                Ok(_) => Exit::Completed,
                Err(error) if error.is_cancelled() => Exit::Cancelled,
                Err(_) => Exit::Panicked,
            };
            let _ = tx.send(result);
            exit
        });
        self.0.lock().unwrap().push(TaskRecord {
            id,
            abort: abort.clone(),
            observer,
        });
        TestTask { abort, result }
    }

    fn spawn<F>(&self, id: TaskId, future: F) -> TestTask<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.track(id, tokio::spawn(
            TASKS.scope(self.clone(), HELPER_ID.scope(id, future)),
        ))
    }

    async fn drain(&self, failed_scenario: bool) -> (Vec<String>, BTreeMap<TaskId, usize>) {
        let mut failures = Vec::new();
        let mut registrations = BTreeMap::new();
        // A helper can register its worker while it is being torn down. Drain
        // repeatedly until all parents and any subsequently registered children
        // have terminated.
        loop {
            let records = std::mem::take(&mut *self.0.lock().unwrap());
            if records.is_empty() {
                break;
            }
            for mut record in records {
                *registrations.entry(record.id).or_insert(0) += 1;
                if failed_scenario {
                    record.abort.abort();
                }
                let result = match tokio::time::timeout(
                    Duration::from_secs(2), &mut record.observer,
                ).await {
                    Ok(result) => result,
                    Err(_) => {
                        failures.push(format!("{:?} exceeded join detection deadline", record.id));
                        record.abort.abort();
                        // Do not abort the observer: it owns the original join.
                        // This final cooperative wait is intentionally unbounded:
                        // retain join ownership, not a hard teardown guarantee.
                        (&mut record.observer).await
                    }
                };
                match result {
                    Ok(Exit::Completed | Exit::Cancelled) => {}
                    Ok(Exit::Panicked) => failures.push(format!("{:?} panicked", record.id)),
                    Err(_) => failures.push(format!("{:?} join observer failed", record.id)),
                }
            }
        }
        (failures, registrations)
    }
}

// Only these component scenarios install TASKS. Other existing tests retain the
// original direct worker-handle behavior. Production never compiles this type.
pub(crate) struct WorkerTask {
    direct: Option<JoinHandle<std::io::Result<()>>>,
    tracked: Option<TestTask<std::io::Result<()>>>,
}

impl WorkerTask {
    pub(crate) fn new(task: JoinHandle<std::io::Result<()>>) -> Self {
        let tasks = TASKS.try_with(Clone::clone).ok();
        match tasks {
            Some(tasks) => {
                // Missing context is an unexpected identity, not a panic that
                // could drop the original handle before its join is retained.
                let id = HELPER_ID.try_with(|id| id.worker()).unwrap_or(
                    TaskId::new("missing-helper-context", 0, "unscoped", TaskRole::Worker),
                );
                Self { direct: None, tracked: Some(tasks.track(id, task)) }
            }
            None => Self { direct: Some(task), tracked: None },
        }
    }

    pub(crate) fn abort(&self) {
        if let Some(task) = &self.direct {
            task.abort();
        }
        if let Some(task) = &self.tracked {
            task.abort();
        }
    }

    pub(crate) fn is_finished(&self) -> bool {
        match &self.tracked {
            Some(task) => task.is_finished(),
            None => self.direct.as_ref().unwrap().is_finished(),
        }
    }

    pub(crate) async fn join(&mut self) -> Result<std::io::Result<()>, JoinError> {
        match &mut self.tracked {
            Some(task) => task.join().await,
            None => self.direct.as_mut().unwrap().await,
        }
    }
}

fn registration_failures(
    expected_tasks: &[TaskId],
    registrations: &BTreeMap<TaskId, usize>,
) -> Vec<String> {
    let mut failures = Vec::new();
    let expected: BTreeSet<_> = expected_tasks.iter().copied().collect();
    for id in &expected {
        let count = expected_tasks.iter().filter(|candidate| *candidate == id).count();
        if count != 1 {
            failures.push(format!("duplicate expected task identity: {id:?} ({count} expectations)"));
        }
    }
    let actual: BTreeSet<_> = registrations.keys().copied().collect();
    for id in expected.difference(&actual) {
        failures.push(format!("missing task registration: {id:?}"));
    }
    for id in actual.difference(&expected) {
        failures.push(format!("unexpected task registration: {id:?}"));
    }
    for (id, count) in registrations {
        if *count != 1 {
            failures.push(format!("duplicate task registration: {id:?} ({count} registrations)"));
        }
    }
    failures
}

async fn complete_case(
    tasks: &TestTasks,
    expected_tasks: &[TaskId],
    mut scenario: JoinHandle<()>,
) -> (Result<(), JoinError>, bool, Vec<String>) {
    let (result, timed_out) = match tokio::time::timeout(
        Duration::from_secs(30), &mut scenario,
    ).await {
        Ok(result) => (result, false),
        Err(_) => {
            scenario.abort();
            // Retain and await the scenario handle before draining its children.
            // This cooperative wait is not bounded by the detection timeout.
            (scenario.await, true)
        }
    };
    let (mut failures, registrations) = tasks.drain(timed_out || result.is_err()).await;
    failures.extend(registration_failures(expected_tasks, &registrations));
    (result, timed_out, failures)
}

async fn run_case<F, Fut>(expected_tasks: Vec<TaskId>, case: F)
where
    F: FnOnce(TestTasks) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let expected: BTreeSet<_> = expected_tasks.iter().copied().collect();
    assert_eq!(expected.len(), expected_tasks.len(), "duplicate expected task identity");
    let tasks = TestTasks::default();
    let scenario = tokio::spawn(TASKS.scope(tasks.clone(), case(tasks.clone())));
    let (result, timed_out, failures) = complete_case(&tasks, &expected_tasks, scenario).await;
    assert!(failures.is_empty(), "task cleanup failures: {failures:?}");
    assert!(!timed_out, "component scenario timed out after explicit teardown");
    if let Err(error) = result {
        if error.is_panic() {
            std::panic::resume_unwind(error.into_panic());
        }
        panic!("component scenario was unexpectedly cancelled");
    }
}

async fn task_result<T>(task: &mut TestTask<T>) -> Result<T, JoinError> {
    match tokio::time::timeout(Duration::from_secs(2), &mut task.result).await {
        Ok(result) => result.expect("task observer must deliver its join result"),
        Err(_) => {
            task.abort();
            // Cooperative final join, not a hard-bounded cleanup deadline.
            let result = task.join().await;
            assert!(
                matches!(result, Err(ref error) if error.is_cancelled()),
                "timed-out task must terminate after explicit abort",
            );
            panic!("task timed out; aborted and joined before reporting failure");
        }
    }
}

async fn joined(mut task: TestTask<Result<(), Error>>) {
    assert_eq!(task_result(&mut task).await.unwrap(), Err(Error::Disconnected));
}

async fn cancelled<T>(mut task: TestTask<T>) {
    task.abort();
    assert!(matches!(
        task_result(&mut task).await,
        Err(error) if error.is_cancelled()
    ));
}

// Observe protocol state, never sleep to choose an interleaving. Router time
// stays fixed: these scenarios do not establish authorization/deadline expiry.
async fn ready(router: &mut Router, runtime: u64) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            assert_eq!(router.tick(Duration::ZERO), None);
            if let Some(readiness) = router.readiness(Duration::ZERO) {
                assert_eq!(readiness.runtime, runtime);
                break;
            }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
}

async fn eof(phone: &mut UnixStream) {
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), phone.read(&mut [0]))
            .await.unwrap().unwrap(),
        0,
    );
}

async fn routed(router: &mut Router, generation: u64) -> UnixStream {
    let (stream, phone) = StdStream::pair().unwrap();
    phone.set_nonblocking(true).unwrap();
    router.route_arrival(
        generation, config().phone(), config().phone(), stream.into(), Duration::ZERO,
    ).unwrap();
    let mut phone = UnixStream::from_std(phone).unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), packet(&mut phone)).await.unwrap(),
        wpp::wifi_version_request(config().channel()),
    );
    // Receipt proves worker startup; this tick drives its queued adoption ACK.
    assert_eq!(router.tick(Duration::ZERO), None);
    phone
}

async fn sentinel_round_trip(a: &mut UnixStream, b: &mut UnixStream) {
    a.write_all(b"s").await.unwrap();
    let mut byte = [0];
    tokio::time::timeout(Duration::from_secs(2), b.read_exact(&mut byte))
        .await.unwrap().unwrap();
    assert_eq!(byte, [b's']);
}

#[test]
fn registration_diagnostics_detect_each_identity_defect() {
    let expected = TaskId::new("identity-diagnostics", 0, "expected", TaskRole::Worker);
    let other = TaskId::new("identity-diagnostics", 1, "unexpected", TaskRole::Worker);
    assert_eq!(
        registration_failures(&[expected], &BTreeMap::new()),
        vec![format!("missing task registration: {expected:?}")],
    );
    assert_eq!(
        registration_failures(&[expected], &BTreeMap::from([(expected, 2)])),
        vec![format!("duplicate task registration: {expected:?} (2 registrations)")],
    );
    assert_eq!(
        registration_failures(&[], &BTreeMap::from([(other, 1)])),
        vec![format!("unexpected task registration: {other:?}")],
    );
    // Same aggregate worker count, but neither identity may mask the other.
    assert_eq!(
        registration_failures(&[expected], &BTreeMap::from([(other, 1)])),
        vec![
            format!("missing task registration: {expected:?}"),
            format!("unexpected task registration: {other:?}"),
        ],
    );
    assert_eq!(
        registration_failures(&[expected, expected], &BTreeMap::from([(expected, 1)])),
        vec![format!("duplicate expected task identity: {expected:?} (2 expectations)")],
    );
    assert!(registration_failures(&[expected], &BTreeMap::from([(expected, 1)])).is_empty());
}

// On a current-thread runtime one manual poll reaches readiness Pending without
// polling the newly spawned listener. No sleep chooses this cancellation point.
async fn hold_before_listener_ready(
    tasks: TestTasks,
    id: TaskId,
    registered: oneshot::Sender<AbortHandle>,
    panic_after_registration: bool,
) {
    use livi_aa::server::admission::offline_test_support::supervised_shared_listener;
    let mut fixture = Box::pin(supervised_shared_listener(
        93,
        |_, _, _| panic!("unexpected offline session"),
        |worker| {
            let abort = worker.abort_handle();
            let ticket = tasks.track(id, worker);
            registered.send(abort).expect("registration observer");
            ticket
        },
        |mut ticket: TestTask<()>| async move {
            ticket.abort();
            let _ = ticket.join().await;
        },
    ));
    std::future::poll_fn(|cx| {
        assert!(fixture.as_mut().poll(cx).is_pending(), "fixture must await readiness");
        std::task::Poll::Ready(())
    }).await;
    assert!(!tasks.0.lock().unwrap().is_empty(), "join must already be supervised");
    if panic_after_registration {
        panic!("intentional scenario panic after listener registration");
    }
    std::future::pending::<()>().await;
    drop(fixture);
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_pre_ready_fixture_retains_listener_join() {
    let tasks = TestTasks::default();
    let id = TaskId::new("fixture-drop", 0, "pre-ready", TaskRole::Listener);
    let (tx, rx) = oneshot::channel();
    let mut scenario = Box::pin(hold_before_listener_ready(tasks.clone(), id, tx, false));
    std::future::poll_fn(|cx| {
        assert!(scenario.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    }).await;
    let abort = rx.await.unwrap();
    drop(scenario);
    let (failures, registrations) = tasks.drain(false).await;
    assert!(failures.is_empty(), "{failures:?}");
    assert!(registration_failures(&[id], &registrations).is_empty());
    assert!(abort.is_finished(), "original listener was joined");
    assert!(tasks.0.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn scenario_panic_and_cancellation_drain_registered_listener() {
    for panic_scenario in [false, true] {
        let tasks = TestTasks::default();
        let incarnation = if panic_scenario { "panic" } else { "cancel" };
        let id = TaskId::new("scenario-exit", 0, incarnation, TaskRole::Listener);
        let (tx, rx) = oneshot::channel();
        let scenario = tokio::spawn(TASKS.scope(
            tasks.clone(),
            hold_before_listener_ready(tasks.clone(), id, tx, panic_scenario),
        ));
        let abort = tokio::time::timeout(Duration::from_secs(2), rx).await.unwrap().unwrap();
        if !panic_scenario {
            // Registration is deterministic; the listener may have become
            // ready by the time this parent resumes to request cancellation.
            scenario.abort();
        }
        // Uses the same completion/drain path as the component scenario runner.
        let (result, timed_out, failures) = complete_case(&tasks, &[id], scenario).await;
        let error = result.expect_err("injected scenario exit");
        assert_eq!(error.is_panic(), panic_scenario);
        assert_eq!(error.is_cancelled(), !panic_scenario);
        assert!(!timed_out);
        assert!(failures.is_empty(), "{failures:?}");
        assert!(abort.is_finished());
        assert!(tasks.0.lock().unwrap().is_empty());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn aborted_listener_readiness_timeout_runs_supervised_stop() {
    use livi_aa::server::admission::offline_test_support::supervised_shared_listener;
    let tasks = TestTasks::default();
    let id = TaskId::new("readiness-timeout", 0, "aborted-before-poll", TaskRole::Listener);
    let (stopped_tx, stopped_rx) = oneshot::channel();
    let result = supervised_shared_listener(
        94,
        |_, _, _| panic!("unexpected offline session"),
        |worker| {
            worker.abort();
            tasks.track(id, worker)
        },
        |mut ticket: TestTask<()>| async move {
            ticket.abort();
            assert!(ticket.join().await.unwrap_err().is_cancelled());
            stopped_tx.send(()).expect("stop observer");
        },
    ).await;
    assert!(matches!(result, Err(Failure::NotReady)));
    stopped_rx.await.expect("readiness failure must invoke supervised stop");
    let (failures, registrations) = tasks.drain(false).await;
    assert!(failures.is_empty(), "{failures:?}");
    assert!(registration_failures(&[id], &registrations).is_empty());
    // This covers the existing readiness timeout, not every Failed-state branch
    // and not a hard upper bound on cooperative final joining.
}

#[tokio::test(flavor = "current_thread")]
async fn completed_and_panicked_nested_workers_keep_their_identity_and_join() {
    for panic_worker in [false, true] {
        let tasks = TestTasks::default();
        let incarnation = if panic_worker { "panic" } else { "complete" };
        let id = TaskId::new("worker-exit", 0, incarnation, TaskRole::Helper);
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let mut helper = tasks.spawn(id, async move {
            let worker = tokio::spawn(async move {
                assert!(!panic_worker, "intentional nested worker panic");
                Ok::<(), std::io::Error>(())
            });
            let abort = worker.abort_handle();
            let worker = WorkerTask::new(worker);
            started_tx.send(abort).expect("worker observer");
            release_rx.await.expect("release helper");
            // Its result receiver is deliberately never consumed.
            drop(worker);
        });
        let abort = started_rx.await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !abort.is_finished() {
                tokio::task::yield_now().await;
            }
        }).await.unwrap();
        release_tx.send(()).unwrap();
        task_result(&mut helper).await.unwrap();
        drop(helper);
        let (failures, registrations) = tasks.drain(false).await;
        assert!(registration_failures(&[id, id.worker()], &registrations).is_empty());
        let expected = if panic_worker {
            vec![format!("{:?} panicked", id.worker())]
        } else {
            Vec::new()
        };
        assert_eq!(failures, expected);
        assert!(abort.is_finished());
        assert!(tasks.0.lock().unwrap().is_empty());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn dropped_worker_ticket_and_missing_context_are_still_joined() {
    let tasks = TestTasks::default();
    let id = TaskId::new("ticket-drop", 0, "pending", TaskRole::Worker);
    let task = tokio::spawn(std::future::pending::<()>());
    let abort = task.abort_handle();
    drop(tasks.track(id, task));
    let (failures, registrations) = tasks.drain(false).await;
    assert!(failures.is_empty(), "{failures:?}");
    assert!(registration_failures(&[id], &registrations).is_empty());
    assert!(abort.is_finished());

    let expected = TaskId::new("missing-context", 0, "expected", TaskRole::Worker);
    let fallback = TaskId::new("missing-helper-context", 0, "unscoped", TaskRole::Worker);
    let abort = TASKS.scope(tasks.clone(), async {
        // No HELPER_ID scope: this must not panic and lose the original handle.
        let task = tokio::spawn(std::future::pending::<std::io::Result<()>>());
        let abort = task.abort_handle();
        drop(WorkerTask::new(task));
        abort
    }).await;
    let (failures, registrations) = tasks.drain(false).await;
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(
        registration_failures(&[expected], &registrations),
        vec![
            format!("missing task registration: {expected:?}"),
            format!("unexpected task registration: {fallback:?}"),
        ],
    );
    assert!(abort.is_finished());
    assert!(tasks.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn helper_recreation_revokes_old_runtime_and_requires_fresh_prepare() {
    use livi_aa::server::admission::offline_test_support::supervised_shared_listener;

    const SCENARIO: &str = "helper-recreation";
    let mut expected = Vec::new();
    for boundary in 0..3 {
        expected.extend(["old", "replacement"].map(|incarnation|
            TaskId::new(SCENARIO, boundary, incarnation, TaskRole::Listener)));
        if boundary != 0 {
            expected.push(TaskId::new(SCENARIO, boundary, "old", TaskRole::Helper));
        }
        if boundary == 2 {
            expected.push(TaskId::new(SCENARIO, boundary, "old", TaskRole::Worker));
        }
        expected.extend([TaskRole::Helper, TaskRole::Worker].map(|role|
            TaskId::new(SCENARIO, boundary, "replacement", role)));
    }
    run_case(expected, |tasks| async move {
        // Task/channel/listener recreation, not executable/OS process restart.
        // Covers loss before Ready, after Ready, and after descriptor adoption.
        for boundary in 0..3 {
            let id = |incarnation, role| TaskId::new(SCENARIO, boundary, incarnation, role);
            let directory = TestDirectory::new();
            let endpoint = Endpoint::bind_isolated(&directory.0).unwrap();
            let (events_a, mut received_a) = tokio::sync::mpsc::unbounded_channel();
            let (control_a, network_a, _, listener_a) = supervised_shared_listener(
                91,
                move |_, _, _| { let _ = events_a.send(()); },
                |worker| tasks.track(id("old", TaskRole::Listener), worker),
                |mut ticket: TestTask<()>| async move { ticket.abort(); let _ = ticket.join().await; },
            ).await.unwrap();
            let mut router = Router::at_endpoint(directory.0.clone());
            router.prepare_for_runtime(1, 91, config(), Duration::ZERO).unwrap();
            let old_channel = endpoint.accept().unwrap().unwrap();
            let mut old_phone = None;
            if boundary == 0 {
                drop(old_channel);
            } else {
                let handler = tasks.spawn(id("old", TaskRole::Helper), serve_channel(
                    old_channel, WiredPhones::default(),
                    NativeTcp::offline_shared(control_a.clone(), network_a),
                ));
                ready(&mut router, 91).await;
                if boundary == 2 {
                    old_phone = Some(routed(&mut router, 1).await);
                }
                cancelled(handler).await;
            }
            cancelled(listener_a).await;
            assert!(matches!(
                received_a.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
            ));
            drop(endpoint);
            if let Some(phone) = &mut old_phone {
                eof(phone).await;
            }
            assert_eq!(router.tick(Duration::ZERO), Some(1));
            assert!(router.readiness(Duration::ZERO).is_none());
            assert!(router.progress().is_none());

            let endpoint = Endpoint::bind_isolated(&directory.0).unwrap();
            let (events_b, mut received_b) = tokio::sync::mpsc::unbounded_channel();
            let (control_b, network_b, _, listener_b) = supervised_shared_listener(
                92,
                move |_, _, _| { let _ = events_b.send(()); },
                |worker| tasks.track(id("replacement", TaskRole::Listener), worker),
                |mut ticket: TestTask<()>| async move { ticket.abort(); let _ = ticket.join().await; },
            ).await.unwrap();
            for _ in 0..3 {
                assert_eq!(router.tick(Duration::ZERO), None);
                assert!(endpoint.accept().unwrap().is_none());
            }
            assert_eq!(
                router.prepare_for_runtime(1, 92, config(), Duration::ZERO),
                Err(Error::Stale),
            );
            router.prepare_for_runtime(2, 92, config(), Duration::ZERO).unwrap();
            let handler = tasks.spawn(id("replacement", TaskRole::Helper), serve_channel(
                endpoint.accept().unwrap().unwrap(), WiredPhones::default(),
                NativeTcp::offline_shared(control_b.clone(), network_b),
            ));
            ready(&mut router, 92).await;

            let (stale, stale_phone) = StdStream::pair().unwrap();
            stale_phone.set_nonblocking(true).unwrap();
            assert_eq!(
                router.route_arrival(
                    1, config().phone(), config().phone(), stale.into(), Duration::ZERO,
                ),
                Err((1, Error::Stale)),
            );
            eof(&mut UnixStream::from_std(stale_phone).unwrap()).await;
            assert_eq!(router.readiness(Duration::ZERO).unwrap().runtime, 92);

            let mut phone = routed(&mut router, 2).await;
            phone.write_all(&version("offline-instance", "offline-serial")).await.unwrap();
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(2), packet(&mut phone)).await.unwrap(),
                wpp::wifi_start_request("192.168.1.20", 5277),
            );
            phone.write_all(&wpp::frame(wpp::MSG_WIFI_CONNECT_STATUS, &[8, 0])).await.unwrap();
            eof(&mut phone).await;
            assert_eq!(control_b.status().1, Some(Failure::IdentityUnverified));
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if router.tick(Duration::ZERO) == Some(2) { break; }
                    tokio::task::yield_now().await;
                }
            }).await.unwrap();
            assert!(router.progress().is_none());
            router.cancel();
            joined(handler).await;
            cancelled(listener_b).await;
            assert!(matches!(
                received_b.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
            ));
            drop((router, endpoint));
            assert!(!directory.0.join("bridge.sock").exists());
            assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 0);
        }
    }).await;
}

#[tokio::test]
async fn router_recreation_requires_new_channel_authorization() {
    const SCENARIO: &str = "router-recreation";
    let expected = vec![
        TaskId::new(SCENARIO, 0, "old", TaskRole::Helper),
        TaskId::new(SCENARIO, 0, "old", TaskRole::Worker),
        TaskId::new(SCENARIO, 1, "unauthorized-replay", TaskRole::Helper),
        TaskId::new(SCENARIO, 2, "fresh", TaskRole::Helper),
        TaskId::new(SCENARIO, 2, "fresh", TaskRole::Worker),
    ];
    run_case(expected, |tasks| async move {
        // Graceful Router drop/recreation, not abrupt app process death.
        let directory = TestDirectory::new();
        let endpoint = Endpoint::bind_isolated(&directory.0).unwrap();
        // Keeps authorization enforcement, but uses a test-only offline permit.
        let native = NativeTcp::offline_authorized();
        let mut old = Router::at_endpoint(directory.0.clone());
        old.prepare_for_runtime(1, 1, config(), Duration::ZERO).unwrap();
        let handler = tasks.spawn(TaskId::new(SCENARIO, 0, "old", TaskRole::Helper), serve_channel(
            endpoint.accept().unwrap().unwrap(), WiredPhones::default(), native.clone(),
        ));
        ready(&mut old, 1).await;
        let old_grant = old.readiness(Duration::ZERO).unwrap().grant;
        let mut old_phone = routed(&mut old, 1).await;
        drop(old);
        joined(handler).await;
        eof(&mut old_phone).await;

        let mut fresh = Router::at_endpoint(directory.0.clone());
        assert_eq!(fresh.tick(Duration::ZERO), None);
        assert!(fresh.readiness(Duration::ZERO).is_none());
        assert!(endpoint.accept().unwrap().is_none());
        let (stream, phone) = StdStream::pair().unwrap();
        phone.set_nonblocking(true).unwrap();
        assert_eq!(
            fresh.route_arrival(
                1, config().phone(), config().phone(), stream.into(), Duration::ZERO,
            ),
            Err((1, Error::Stale)),
        );
        eof(&mut UnixStream::from_std(phone).unwrap()).await;
        assert!(endpoint.accept().unwrap().is_none());

        // Readiness is channel-local; configuration without Authorize fails.
        let mut replay = Channel::connect_isolated(&directory.0).unwrap();
        replay.send(Message::Configure { generation: 1, config: config() }).unwrap();
        let mut rejected = tasks.spawn(
            TaskId::new(SCENARIO, 1, "unauthorized-replay", TaskRole::Helper), serve_channel(
            endpoint.accept().unwrap().unwrap(), WiredPhones::default(), native.clone(),
        ));
        assert_eq!(
            task_result(&mut rejected).await.unwrap(),
            Err(Error::Configuration),
        );
        assert_eq!(replay.receive().err(), Some(Error::Disconnected));
        drop(replay);

        fresh.prepare_for_runtime(1, 1, config(), Duration::ZERO).unwrap();
        let handler = tasks.spawn(TaskId::new(SCENARIO, 2, "fresh", TaskRole::Helper), serve_channel(
            endpoint.accept().unwrap().unwrap(), WiredPhones::default(), native,
        ));
        ready(&mut fresh, 1).await;
        assert_ne!(fresh.readiness(Duration::ZERO).unwrap().grant, old_grant);
        let mut phone = routed(&mut fresh, 1).await;
        fresh.cancel();
        joined(handler).await;
        eof(&mut phone).await;
        drop((fresh, endpoint));
        assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 0);
    }).await;
}

#[tokio::test]
async fn adopted_transport_cancel_channel_eof_and_helper_abort_are_scoped() {
    const SCENARIO: &str = "adopted-transport";
    let expected = (0..4).flat_map(|boundary|
        [TaskRole::Helper, TaskRole::Worker].map(|role|
            TaskId::new(SCENARIO, boundary, "owner", role))
    ).collect();
    run_case(expected, |tasks| async move {
        // Ownership fixture intentionally bypasses authorization. Authorization
        // enforcement is covered above, not by this cancellation matrix.
        for boundary in 0..4 {
            let (a, b) = StdStream::pair().unwrap();
            let mut tx = Channel::from_stream(a).unwrap();
            let handler = tasks.spawn(TaskId::new(SCENARIO, boundary, "owner", TaskRole::Helper), serve_channel(
                Channel::from_stream(b).unwrap(), WiredPhones::default(), NativeTcp::offline(),
            ));
            tx.send(Message::Configure { generation: 1, config: config() }).unwrap();
            let (stream, phone) = StdStream::pair().unwrap();
            phone.set_nonblocking(true).unwrap();
            let mut phone = UnixStream::from_std(phone).unwrap();
            tx.send(Message::Arrival {
                generation: 1, peer: config().phone(), local: config().phone(),
                stream: stream.into(),
            }).unwrap();
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(2), packet(&mut phone)).await.unwrap(),
                wpp::wifi_version_request(config().channel()),
            );
            if boundary != 0 {
                assert!(tx.poll_adoption().unwrap());
            }
            // Unrelated socket control, not another admitted helper session.
            let (mut sentinel_a, mut sentinel_b) = UnixStream::pair().unwrap();
            if boundary <= 1 {
                tx.send(Message::Cancel { generation: 0xFFFF }).unwrap();
                phone.write_all(&version("offline-instance", "offline-serial")).await.unwrap();
                assert_eq!(
                    tokio::time::timeout(Duration::from_secs(2), packet(&mut phone)).await.unwrap(),
                    wpp::wifi_start_request("192.168.1.20", 5277),
                );
                tx.send(Message::Cancel { generation: 1 }).unwrap();
                // Before ACK polling this can be sender-side shutdown; only the
                // post-ACK case independently observes helper-side cancellation.
                eof(&mut phone).await;
                drop(tx);
                joined(handler).await;
            } else if boundary == 2 {
                drop(tx);
                joined(handler).await;
                eof(&mut phone).await;
            } else {
                cancelled(handler).await;
                eof(&mut phone).await;
                assert_eq!(tx.receive().err(), Some(Error::Disconnected));
            }
            sentinel_round_trip(&mut sentinel_a, &mut sentinel_b).await;
            drop(sentinel_a);
            eof(&mut sentinel_b).await;
        }
    }).await;
}
