//! L7 Codex app-server broker.
//!
//! The broker is the stable cx service endpoint. It accepts the private Codex
//! app-server WebSocket shape from local clients, routes requests by method and
//! thread id, and keeps raw slot workers behind the broker boundary.

mod preflight;
mod ws;

use std::collections::BTreeMap;
use std::net::Shutdown;
use std::net::TcpListener;
use std::net::TcpStream;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use serde_json::json;
use serde_json::Value;

use crate::approval;
use crate::approval::ApprovalBroker;
use crate::approval::ApprovalResponseRoute;
use crate::paths::ManagerPaths;
use crate::rate_limit;
use crate::router;
use crate::router::BrokerRoute;
use crate::subscription::ClientId;
use crate::subscription::ClientOutbound;
use crate::subscription::ClientSink;
use crate::subscription::SubscriptionHub;
use crate::thread_directory::LoadedStatus;
use crate::thread_directory::ThreadDirectory;
use crate::thread_directory::ThreadRecord;
use crate::worker_pool::TurnReservationId;
use crate::worker_pool::WorkerId;
use crate::worker_pool::WorkerPool;
use crate::worker_pool::WorkerPoolConfig;
use crate::worker_pool::WorkerRecord;
use crate::worker_pool::WorkerStatus;

use self::ws::BrokerWebSocket;
use self::ws::WebSocketMessage;
use self::ws::WebSocketReader;
use self::ws::WebSocketWriter;

const BLOCKING_WORKER_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub(crate) struct AppServerBroker {
    listen_url: String,
    readyz_url: String,
    state: Arc<BrokerShared>,
    accept_handle: Option<thread::JoinHandle<()>>,
}

#[derive(Debug)]
struct BrokerShared {
    paths: ManagerPaths,
    worker_pool: Mutex<WorkerPool>,
    worker_links: Mutex<BTreeMap<WorkerId, Arc<WorkerLink>>>,
    directory: ThreadDirectory,
    subscriptions: SubscriptionHub,
    approvals: ApprovalBroker,
    next_client_id: AtomicU64,
    next_turn_reservation_id: AtomicU64,
    shutdown: AtomicBool,
}

#[derive(Debug)]
struct WorkerLink {
    worker_id: WorkerId,
    sender: mpsc::Sender<WorkerOutbound>,
    pending: Mutex<BTreeMap<u64, PendingWorkerRequest>>,
    next_request_id: AtomicU64,
    rate_limit_states: Mutex<BTreeMap<RateLimitTurnKey, rate_limit::TurnSideEffectState>>,
    closed: AtomicBool,
}

#[derive(Debug)]
struct BrokerStartupGuard {
    state: Option<Arc<BrokerShared>>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RateLimitTurnKey {
    thread_id: String,
    turn_id: String,
}

#[derive(Debug, Clone)]
struct PendingWorkerRequest {
    client_id: ClientId,
    client_request_id: Value,
    thread_id: Option<String>,
    turn_reservation: Option<TurnReservation>,
}

#[derive(Debug, Clone)]
struct WorkerRequestContext {
    thread_id: Option<String>,
    turn_reservation: Option<TurnReservation>,
}

#[derive(Debug, Clone)]
struct TurnReservation {
    worker_id: WorkerId,
    reservation_id: TurnReservationId,
}

#[derive(Debug)]
enum WorkerOutbound {
    Text(String),
    Pong(Vec<u8>),
    TextWithAck {
        text: String,
        ack: mpsc::Sender<std::result::Result<(), String>>,
    },
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThreadWorkerPolicy {
    PreferOwner,
    NewTurn,
}

#[derive(Debug, Clone)]
struct WorkerAssignment {
    worker: WorkerRecord,
    turn_reservation: Option<TurnReservation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrokerListenUrl {
    host: String,
    port: u16,
}

impl AppServerBroker {
    pub(crate) fn start(
        paths: ManagerPaths,
        listen_url: &str,
        pool_config: WorkerPoolConfig,
    ) -> Result<Self> {
        let listen = BrokerListenUrl::parse(listen_url)?.resolve()?;
        let stable_listen_url = listen.websocket_url();
        let stable_readyz_url = listen.readyz_url();
        let listener = TcpListener::bind((listen.host.as_str(), listen.port))
            .with_context(|| format!("bind broker {}", stable_listen_url))?;
        listener
            .set_nonblocking(true)
            .context("set broker listener nonblocking")?;
        let mut worker_pool = WorkerPool::new(paths.clone(), pool_config);
        let initial_workers = worker_pool.start_initial()?;
        let state = Arc::new(BrokerShared {
            paths,
            worker_pool: Mutex::new(worker_pool),
            worker_links: Mutex::new(BTreeMap::new()),
            directory: ThreadDirectory::default(),
            subscriptions: SubscriptionHub::default(),
            approvals: ApprovalBroker::default(),
            next_client_id: AtomicU64::new(1),
            next_turn_reservation_id: AtomicU64::new(1),
            shutdown: AtomicBool::new(false),
        });
        let mut startup_guard = BrokerStartupGuard::new(Arc::clone(&state));
        state.directory.seed_from_sessions(&state.paths)?;
        for worker in initial_workers {
            state.ensure_worker_link(&worker)?;
        }

        let accept_state = Arc::clone(&state);
        let accept_handle = thread::spawn(move || {
            if let Err(err) = preflight::run_preflight_reactor(Arc::clone(&accept_state), listener)
            {
                if !accept_state.shutdown.load(Ordering::Acquire) {
                    eprintln!("broker preflight reactor failed: {err:#}");
                }
            }
        });
        startup_guard.disarm();

        Ok(Self {
            listen_url: stable_listen_url,
            readyz_url: stable_readyz_url,
            state,
            accept_handle: Some(accept_handle),
        })
    }

    pub(crate) fn listen_url(&self) -> &str {
        &self.listen_url
    }

    pub(crate) fn readyz_url(&self) -> &str {
        &self.readyz_url
    }

    pub(crate) fn worker_records(&self) -> Result<Vec<WorkerRecord>> {
        let pool = self
            .state
            .worker_pool
            .lock()
            .map_err(|_| anyhow::anyhow!("worker pool lock poisoned"))?;
        Ok(pool.records())
    }

    pub(crate) fn poll_workers(&self) -> Result<()> {
        let usage_request = {
            let mut pool = self
                .state
                .worker_pool
                .lock()
                .map_err(|_| anyhow::anyhow!("worker pool lock poisoned"))?;
            pool.usage_refresh_request(false)?
        };
        if let Some(request) = usage_request {
            match request.query() {
                Ok(results) => {
                    let mut pool = self
                        .state
                        .worker_pool
                        .lock()
                        .map_err(|_| anyhow::anyhow!("worker pool lock poisoned"))?;
                    pool.apply_usage_results(results);
                }
                Err(err) => {
                    eprintln!("broker worker usage refresh failed: {err:#}");
                }
            }
        }
        let events = {
            let mut pool = self
                .state
                .worker_pool
                .lock()
                .map_err(|_| anyhow::anyhow!("worker pool lock poisoned"))?;
            pool.poll_children()?
        };
        if events.is_empty() {
            return Ok(());
        }
        let mut links = self
            .state
            .worker_links
            .lock()
            .map_err(|_| anyhow::anyhow!("worker link lock poisoned"))?;
        let mut failed_links = Vec::new();
        for event in events {
            match event {
                crate::worker_pool::WorkerLifecycleEvent::Exited { worker_id, .. } => {
                    if let Some(link) = links.remove(&worker_id) {
                        failed_links.push(link);
                    }
                    self.state.approvals.cancel_worker(&worker_id)?;
                    self.state.directory.mark_worker_unavailable(&worker_id)?;
                }
                crate::worker_pool::WorkerLifecycleEvent::Started(record) => {
                    drop(links);
                    self.state.ensure_worker_link(&record)?;
                    links = self
                        .state
                        .worker_links
                        .lock()
                        .map_err(|_| anyhow::anyhow!("worker link lock poisoned"))?;
                }
            }
        }
        drop(links);
        for link in failed_links {
            link.close();
            self.state.fail_worker_link_pending(&link)?;
        }
        Ok(())
    }
}

impl BrokerStartupGuard {
    fn new(state: Arc<BrokerShared>) -> Self {
        Self { state: Some(state) }
    }

    fn disarm(&mut self) {
        self.state = None;
    }
}

impl Drop for BrokerStartupGuard {
    fn drop(&mut self) {
        let Some(state) = self.state.take() else {
            return;
        };
        if let Ok(mut pool) = state.worker_pool.lock() {
            pool.shutdown();
        };
    }
}

impl Drop for AppServerBroker {
    fn drop(&mut self) {
        self.state.shutdown.store(true, Ordering::Release);
        if let Ok(stream) = TcpStream::connect(parse_loopback_socket_addr(&self.listen_url)) {
            let _ = stream.shutdown(Shutdown::Both);
        }
        if let Ok(mut pool) = self.state.worker_pool.lock() {
            pool.shutdown();
        }
        if let Some(handle) = self.accept_handle.take() {
            let _ = handle.join();
        }
    }
}

impl BrokerShared {
    fn live(&self) -> bool {
        true
    }

    fn observation_ready(&self) -> bool {
        let Ok(pool) = self.worker_pool.lock() else {
            return false;
        };
        let Ok(links) = self.worker_links.lock() else {
            return false;
        };
        pool.observation_worker_ids()
            .iter()
            .any(|worker_id| links.contains_key(worker_id))
    }

    fn new_turn_capacity_ready(&self) -> bool {
        let Ok(mut pool) = self.worker_pool.lock() else {
            return false;
        };
        let Ok(links) = self.worker_links.lock() else {
            return false;
        };
        pool.new_turn_worker_ids()
            .map(|worker_ids| {
                worker_ids
                    .iter()
                    .any(|worker_id| links.contains_key(worker_id))
            })
            .unwrap_or(false)
    }

    fn next_client_id(&self) -> ClientId {
        ClientId::new(self.next_client_id.fetch_add(1, Ordering::Relaxed))
    }

    fn next_turn_reservation(&self, worker_id: WorkerId) -> TurnReservation {
        TurnReservation {
            worker_id,
            reservation_id: TurnReservationId::new(
                self.next_turn_reservation_id
                    .fetch_add(1, Ordering::Relaxed),
            ),
        }
    }

    fn ensure_worker_link(self: &Arc<Self>, record: &WorkerRecord) -> Result<Arc<WorkerLink>> {
        {
            let links = self
                .worker_links
                .lock()
                .map_err(|_| anyhow::anyhow!("worker link lock poisoned"))?;
            if let Some(link) = links.get(&record.worker_id) {
                return Ok(Arc::clone(link));
            }
        }

        let link = WorkerLink::start(record.clone(), Arc::clone(self))?;
        let mut links = self
            .worker_links
            .lock()
            .map_err(|_| anyhow::anyhow!("worker link lock poisoned"))?;
        if let Some(existing) = links.get(&record.worker_id) {
            link.close();
            return Ok(Arc::clone(existing));
        }
        links.insert(record.worker_id.clone(), Arc::clone(&link));
        Ok(link)
    }

    fn evict_worker_link_if_current(
        &self,
        worker_id: &WorkerId,
        link: &Arc<WorkerLink>,
    ) -> Result<bool> {
        let mut links = self
            .worker_links
            .lock()
            .map_err(|_| anyhow::anyhow!("worker link lock poisoned"))?;
        let should_remove = links
            .get(worker_id)
            .is_some_and(|current| Arc::ptr_eq(current, link));
        if should_remove {
            links.remove(worker_id);
        }
        Ok(should_remove)
    }

    fn fail_worker_link_pending(&self, link: &WorkerLink) -> Result<()> {
        let _cancelled_approvals = self.approvals.cancel_worker(&link.worker_id)?;
        self.worker_pool
            .lock()
            .map_err(|_| anyhow::anyhow!("worker pool lock poisoned"))?
            .clear_turn_reservations(&link.worker_id);
        for pending in link.drain_pending()? {
            if let Some(reservation) = pending.turn_reservation.as_ref() {
                self.release_turn_reservation(reservation)?;
            }
            if let Some(thread_id) = pending.thread_id.as_deref() {
                rollback_client_subscription(self, pending.client_id, thread_id)?;
            }
            let response = error_response(
                pending.client_request_id,
                -32002,
                format!("worker link {} closed before response", link.worker_id),
            );
            let _sent = self
                .subscriptions
                .send_client(pending.client_id, serde_json::to_string(&response)?)?;
        }
        Ok(())
    }

    fn choose_worker_for_thread(self: &Arc<Self>, thread_id: &str) -> Result<WorkerAssignment> {
        self.choose_worker_for_thread_with_policy(thread_id, ThreadWorkerPolicy::PreferOwner)
    }

    fn choose_worker_for_new_turn(self: &Arc<Self>, thread_id: &str) -> Result<WorkerAssignment> {
        self.choose_worker_for_thread_with_policy(thread_id, ThreadWorkerPolicy::NewTurn)
    }

    fn choose_worker_for_thread_with_policy(
        self: &Arc<Self>,
        thread_id: &str,
        policy: ThreadWorkerPolicy,
    ) -> Result<WorkerAssignment> {
        let record = self.directory.record(thread_id)?;
        let previous_owner = record
            .as_ref()
            .and_then(|record| record.owner_worker_id.clone());
        if let Some(owner) = record
            .as_ref()
            .and_then(|record| record.owner_worker_id.clone())
        {
            let owner_worker = {
                let mut pool = self
                    .worker_pool
                    .lock()
                    .map_err(|_| anyhow::anyhow!("worker pool lock poisoned"))?;
                match policy {
                    ThreadWorkerPolicy::PreferOwner => pool.worker(&owner),
                    ThreadWorkerPolicy::NewTurn => {
                        if record
                            .as_ref()
                            .is_some_and(|thread| thread.loaded_status == LoadedStatus::Active)
                        {
                            pool.worker(&owner)
                                .filter(|worker| worker.status == WorkerStatus::Ready)
                        } else {
                            pool.worker_accepts_new_turns(&owner)?
                        }
                    }
                }
                .map(|worker| {
                    self.assignment_for_policy_locked(&mut pool, worker, thread_id, policy)
                })
                .transpose()?
            };
            if let Some(assignment) = owner_worker {
                return Ok(assignment);
            }
        }

        let origin_slot = record
            .as_ref()
            .and_then(|record| record.origin_slot.as_deref());
        let assignment = {
            let mut pool = self
                .worker_pool
                .lock()
                .map_err(|_| anyhow::anyhow!("worker pool lock poisoned"))?;
            let selected = match policy {
                ThreadWorkerPolicy::PreferOwner => pool.choose_observation_worker(origin_slot)?,
                ThreadWorkerPolicy::NewTurn => pool.choose_worker(origin_slot)?,
            }
            .context("no ready worker is available")?;
            self.assignment_for_policy_locked(&mut pool, selected, thread_id, policy)?
        };

        if let Some(record) = record.as_ref() {
            let strict_load = policy == ThreadWorkerPolicy::NewTurn;
            if strict_load || record.path.is_some() {
                let loaded =
                    match self.load_thread_on_worker(&assignment.worker, record, strict_load) {
                        Ok(loaded) => loaded,
                        Err(err) => {
                            self.release_assignment_reservation(&assignment)?;
                            return Err(err);
                        }
                    };
                if loaded {
                    self.unsubscribe_previous_owner(
                        previous_owner.as_ref(),
                        &assignment.worker.worker_id,
                        thread_id,
                    )?;
                }
            }
        }
        Ok(assignment)
    }

    fn assignment_for_policy_locked(
        &self,
        pool: &mut WorkerPool,
        worker: WorkerRecord,
        thread_id: &str,
        policy: ThreadWorkerPolicy,
    ) -> Result<WorkerAssignment> {
        if policy != ThreadWorkerPolicy::NewTurn {
            return Ok(WorkerAssignment {
                worker,
                turn_reservation: None,
            });
        }
        let reservation = self.next_turn_reservation(worker.worker_id.clone());
        pool.reserve_turn_start(&worker.worker_id, thread_id, reservation.reservation_id);
        Ok(WorkerAssignment {
            worker,
            turn_reservation: Some(reservation),
        })
    }

    fn release_assignment_reservation(&self, assignment: &WorkerAssignment) -> Result<()> {
        if let Some(reservation) = assignment.turn_reservation.as_ref() {
            self.release_turn_reservation(reservation)?;
        }
        Ok(())
    }

    fn release_turn_reservation(&self, reservation: &TurnReservation) -> Result<()> {
        self.worker_pool
            .lock()
            .map_err(|_| anyhow::anyhow!("worker pool lock poisoned"))?
            .release_turn_reservation(&reservation.worker_id, reservation.reservation_id);
        Ok(())
    }

    fn load_thread_on_worker(
        &self,
        worker: &WorkerRecord,
        record: &ThreadRecord,
        strict: bool,
    ) -> Result<bool> {
        match resume_thread_on_worker(worker, record) {
            Ok(result) => {
                if let Some(thread) = result.get("thread") {
                    self.directory.upsert_thread_value(
                        worker.worker_id.clone(),
                        &worker.slot,
                        thread,
                    )?;
                    return Ok(true);
                }
            }
            Err(err) if strict => {
                return Err(err).with_context(|| {
                    format!("resume thread {} on {}", record.thread_id, worker.worker_id)
                });
            }
            Err(_) => return Ok(false),
        }

        match read_thread_on_worker(worker, &record.thread_id, false) {
            Ok(thread) => {
                self.directory.upsert_thread_value(
                    worker.worker_id.clone(),
                    &worker.slot,
                    &thread,
                )?;
                Ok(true)
            }
            Err(err) if strict => Err(err).with_context(|| {
                format!(
                    "read resumed thread {} on {}",
                    record.thread_id, worker.worker_id
                )
            }),
            Err(_) => Ok(false),
        }
    }

    fn unsubscribe_previous_owner(
        &self,
        previous_owner: Option<&WorkerId>,
        selected_worker: &WorkerId,
        thread_id: &str,
    ) -> Result<()> {
        let Some(previous_owner) = previous_owner_to_release(previous_owner, selected_worker)
        else {
            return Ok(());
        };
        self.unsubscribe_worker_thread_on(previous_owner, thread_id)
    }

    fn choose_default_worker(self: &Arc<Self>) -> Result<WorkerRecord> {
        self.worker_pool
            .lock()
            .map_err(|_| anyhow::anyhow!("worker pool lock poisoned"))?
            .choose_worker(None)?
            .context("no ready worker is available")
    }

    fn resume_should_use_loaded_owner(
        &self,
        selected_worker: &WorkerId,
        thread_id: &str,
    ) -> Result<bool> {
        let record = self.directory.record(thread_id)?;
        Ok(resume_can_use_loaded_owner(
            record.as_ref(),
            selected_worker,
        ))
    }

    fn send_worker_request(
        self: &Arc<Self>,
        worker: WorkerRecord,
        client_id: ClientId,
        request: Value,
        context: WorkerRequestContext,
    ) -> Result<()> {
        let link = match self.ensure_worker_link(&worker) {
            Ok(link) => link,
            Err(err) => {
                if let Some(reservation) = context.turn_reservation.as_ref() {
                    self.release_turn_reservation(reservation)?;
                }
                return Err(err);
            }
        };
        match link.send_request(client_id, request.clone(), context.clone()) {
            Ok(()) => Ok(()),
            Err(first_err) => {
                let _ = self.evict_worker_link_if_current(&worker.worker_id, &link);
                let reconnected = match self.ensure_worker_link(&worker) {
                    Ok(link) => link,
                    Err(err) => {
                        if let Some(reservation) = context.turn_reservation.as_ref() {
                            self.release_turn_reservation(reservation)?;
                        }
                        return Err(err).with_context(|| {
                            format!(
                                "reconnect worker link {} after send failure: {first_err:#}",
                                worker.worker_id
                            )
                        });
                    }
                };
                match reconnected.send_request(client_id, request, context.clone()) {
                    Ok(()) => Ok(()),
                    Err(err) => {
                        if let Some(reservation) = context.turn_reservation.as_ref() {
                            self.release_turn_reservation(reservation)?;
                        }
                        Err(err).with_context(|| {
                            format!(
                                "reconnect worker link {} after send failure: {first_err:#}",
                                worker.worker_id
                            )
                        })
                    }
                }
            }
        }
    }

    fn send_worker_response(&self, worker_id: &WorkerId, response: Value) -> Result<()> {
        let link = self
            .worker_links
            .lock()
            .map_err(|_| anyhow::anyhow!("worker link lock poisoned"))?
            .get(worker_id)
            .cloned()
            .with_context(|| format!("approval worker link is unavailable: {worker_id}"))?;
        link.send_raw_with_write_ack(response)
    }

    fn enqueue_worker_response(&self, worker_id: &WorkerId, response: Value) -> Result<()> {
        let link = self
            .worker_links
            .lock()
            .map_err(|_| anyhow::anyhow!("worker link lock poisoned"))?
            .get(worker_id)
            .cloned()
            .with_context(|| format!("approval worker link is unavailable: {worker_id}"))?;
        link.send_raw(response)
    }

    fn unsubscribe_worker_thread(&self, thread_id: &str) -> Result<()> {
        let Some(record) = self.directory.record(thread_id)? else {
            return Ok(());
        };
        if !should_unsubscribe_worker_thread(&record) {
            return Ok(());
        }
        let Some(worker_id) = record.owner_worker_id else {
            return Ok(());
        };
        self.unsubscribe_worker_thread_on(&worker_id, thread_id)
    }

    fn release_thread_without_subscribers(&self, thread_id: &str) -> Result<()> {
        self.cancel_thread_approvals(thread_id)?;
        self.unsubscribe_worker_thread(thread_id)
    }

    fn cancel_thread_approvals(&self, thread_id: &str) -> Result<()> {
        for approval in self.approvals.cancel_thread(thread_id)? {
            let response = error_response(
                approval.worker_request_id,
                -32003,
                "approval request cancelled because no broker client is subscribed".to_string(),
            );
            let _ = self.enqueue_worker_response(&approval.worker_id, response);
        }
        Ok(())
    }

    fn unsubscribe_worker_thread_on(&self, worker_id: &WorkerId, thread_id: &str) -> Result<()> {
        let link = self
            .worker_links
            .lock()
            .map_err(|_| anyhow::anyhow!("worker link lock poisoned"))?
            .get(worker_id)
            .cloned();
        if let Some(link) = link {
            link.send_control_request(
                "thread/unsubscribe",
                json!({
                    "threadId": thread_id,
                }),
            )?;
        }
        Ok(())
    }
}

fn previous_owner_to_release<'a>(
    previous_owner: Option<&'a WorkerId>,
    selected_worker: &WorkerId,
) -> Option<&'a WorkerId> {
    previous_owner.filter(|previous_owner| *previous_owner != selected_worker)
}

fn should_unsubscribe_worker_thread(record: &ThreadRecord) -> bool {
    record.loaded_status != LoadedStatus::Active
}

fn resume_can_use_loaded_owner(record: Option<&ThreadRecord>, selected_worker: &WorkerId) -> bool {
    record.is_some_and(|record| {
        record.loaded_status != LoadedStatus::NotLoaded
            && record
                .owner_worker_id
                .as_ref()
                .is_some_and(|owner| owner == selected_worker)
    })
}

fn strip_thread_resume_path(mut request: Value) -> Value {
    if let Some(params) = request.get_mut("params").and_then(Value::as_object_mut) {
        params.remove("path");
    }
    request
}

impl WorkerLink {
    fn start(record: WorkerRecord, state: Arc<BrokerShared>) -> Result<Arc<Self>> {
        let socket = BrokerWebSocket::connect(&record.listen_url, Duration::from_millis(250))?;
        let (reader, writer) = socket.split();
        let (tx, rx) = mpsc::channel::<WorkerOutbound>();
        let link = Arc::new(Self {
            worker_id: record.worker_id.clone(),
            sender: tx,
            pending: Mutex::new(BTreeMap::new()),
            next_request_id: AtomicU64::new(1),
            rate_limit_states: Mutex::new(BTreeMap::new()),
            closed: AtomicBool::new(false),
        });
        let worker_id = record.worker_id.clone();
        let thread_link = Arc::clone(&link);
        let thread_state = Arc::clone(&state);
        thread::spawn(move || {
            run_worker_link_threads(thread_state, thread_link, reader, writer, rx, worker_id);
        });
        if let Err(err) = link.send_raw(initialize_request(0)) {
            link.close();
            return Err(err);
        }
        Ok(link)
    }

    fn close(&self) -> bool {
        let first_close = !self.closed.swap(true, Ordering::AcqRel);
        if first_close {
            let _ = self.sender.send(WorkerOutbound::Shutdown);
        }
        first_close
    }

    fn ensure_open(&self) -> Result<()> {
        if self.closed.load(Ordering::Acquire) {
            anyhow::bail!("worker link {} is closed", self.worker_id);
        }
        Ok(())
    }

    fn cleanup_after_close(&self) -> bool {
        self.close()
    }

    fn send_outbound(&self, outbound: WorkerOutbound) -> Result<()> {
        self.ensure_open()?;
        self.sender
            .send(outbound)
            .context("send message to worker link")
    }

    fn send_outbound_with_pending_cleanup(
        &self,
        worker_request_id: u64,
        outbound: WorkerOutbound,
    ) -> Result<()> {
        if let Err(err) = self.send_outbound(outbound) {
            let _ = self
                .pending
                .lock()
                .map_err(|_| anyhow::anyhow!("worker pending lock poisoned"))?
                .remove(&worker_request_id);
            return Err(err);
        }
        Ok(())
    }

    fn fail_ack_on_closed(
        &self,
        ack: &mpsc::Sender<std::result::Result<(), String>>,
    ) -> Result<()> {
        if self.closed.load(Ordering::Acquire) {
            let message = format!("worker link {} is closed", self.worker_id);
            let _ = ack.send(Err(message.clone()));
            anyhow::bail!(message);
        }
        Ok(())
    }

    fn send_raw_outbound(&self, text: String) -> Result<()> {
        self.send_outbound(WorkerOutbound::Text(text))
    }

    fn send_acked_outbound(
        &self,
        text: String,
        ack: mpsc::Sender<std::result::Result<(), String>>,
    ) -> Result<()> {
        self.fail_ack_on_closed(&ack)?;
        self.send_outbound(WorkerOutbound::TextWithAck { text, ack })
    }

    fn complete_shutdown(state: &Arc<BrokerShared>, link: &Arc<WorkerLink>, worker_id: &WorkerId) {
        if !link.cleanup_after_close() {
            return;
        }
        let _ = state.evict_worker_link_if_current(worker_id, link);
        let _ = state.fail_worker_link_pending(link);
    }
}

fn run_worker_link_threads(
    state: Arc<BrokerShared>,
    link: Arc<WorkerLink>,
    reader: WebSocketReader,
    writer: WebSocketWriter,
    rx: mpsc::Receiver<WorkerOutbound>,
    worker_id: WorkerId,
) {
    let writer_worker_id = worker_id.clone();
    let writer_handle = thread::spawn(move || {
        let result = worker_writer_loop(writer, rx);
        if let Err(err) = &result {
            eprintln!("worker link {writer_worker_id} writer stopped: {err:#}");
        }
        result
    });

    let reader_result = worker_reader_loop(Arc::clone(&state), Arc::clone(&link), reader);
    if let Err(err) = &reader_result {
        eprintln!("worker link {worker_id} reader stopped: {err:#}");
    }
    WorkerLink::complete_shutdown(&state, &link, &worker_id);
    match writer_handle.join() {
        Ok(Ok(())) => {}
        Ok(Err(_err)) => {}
        Err(_panic) => {
            eprintln!("worker link {worker_id} writer thread panicked");
        }
    }
}

fn worker_reader_loop(
    state: Arc<BrokerShared>,
    link: Arc<WorkerLink>,
    mut reader: WebSocketReader,
) -> Result<()> {
    loop {
        match reader.read_text_blocking()? {
            WebSocketMessage::Text(text) => handle_worker_text(&state, &link, text)?,
            WebSocketMessage::Ping(payload) => link.send_outbound(WorkerOutbound::Pong(payload))?,
            WebSocketMessage::Closed => {
                let _ = reader.shutdown(Shutdown::Both);
                return Ok(());
            }
        }
    }
}

fn worker_writer_loop(
    mut writer: WebSocketWriter,
    rx: mpsc::Receiver<WorkerOutbound>,
) -> Result<()> {
    let result = loop {
        match rx.recv() {
            Ok(WorkerOutbound::Shutdown) => break Ok(()),
            Ok(outbound) => {
                if let Err(err) = send_worker_outbound(&mut writer, outbound) {
                    break Err(err);
                }
            }
            Err(mpsc::RecvError) => break Ok(()),
        }
    };
    let _ = writer.shutdown(Shutdown::Both);
    result
}

impl WorkerLink {
    fn send_request(
        &self,
        client_id: ClientId,
        mut request: Value,
        context: WorkerRequestContext,
    ) -> Result<()> {
        let client_request_id = request
            .get("id")
            .cloned()
            .context("client request omitted id")?;
        let worker_request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        request["id"] = json!(worker_request_id);
        self.pending
            .lock()
            .map_err(|_| anyhow::anyhow!("worker pending lock poisoned"))?
            .insert(
                worker_request_id,
                PendingWorkerRequest {
                    client_id,
                    client_request_id,
                    thread_id: context.thread_id,
                    turn_reservation: context.turn_reservation,
                },
            );
        let text = serde_json::to_string(&request).context("encode worker message")?;
        self.send_outbound_with_pending_cleanup(worker_request_id, WorkerOutbound::Text(text))
    }

    fn send_raw(&self, value: Value) -> Result<()> {
        let text = serde_json::to_string(&value).context("encode worker message")?;
        self.send_raw_outbound(text)
    }

    fn send_raw_with_write_ack(&self, value: Value) -> Result<()> {
        let text = serde_json::to_string(&value).context("encode worker message")?;
        let (ack_tx, ack_rx) = mpsc::channel();
        self.send_acked_outbound(text, ack_tx)?;
        match ack_rx.recv_timeout(BLOCKING_WORKER_REQUEST_TIMEOUT) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => anyhow::bail!("write acked message to worker failed: {err}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                anyhow::bail!("timed out waiting for worker write acknowledgement")
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("worker link closed before write acknowledgement")
            }
        }
    }

    fn send_control_request(&self, method: &str, params: Value) -> Result<()> {
        let worker_request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        self.send_raw(json!({
            "id": worker_request_id,
            "method": method,
            "params": params,
        }))
    }

    fn clear_rate_limit_state(&self, thread_id: &str, turn_id: Option<&str>) -> Result<()> {
        let mut states = self
            .rate_limit_states
            .lock()
            .map_err(|_| anyhow::anyhow!("worker rate-limit lock poisoned"))?;
        if let Some(turn_id) = turn_id {
            states.remove(&RateLimitTurnKey {
                thread_id: thread_id.to_string(),
                turn_id: turn_id.to_string(),
            });
        } else {
            states.retain(|key, _| key.thread_id != thread_id);
        }
        Ok(())
    }

    fn drain_pending(&self) -> Result<Vec<PendingWorkerRequest>> {
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| anyhow::anyhow!("worker pending lock poisoned"))?;
        Ok(std::mem::take(&mut *pending).into_values().collect())
    }
}

fn send_worker_outbound(socket: &mut WebSocketWriter, outbound: WorkerOutbound) -> Result<()> {
    match outbound {
        WorkerOutbound::Text(text) => socket.send_text(&text),
        WorkerOutbound::Pong(payload) => socket.send_pong(&payload),
        WorkerOutbound::TextWithAck { text, ack } => match socket.send_text(&text) {
            Ok(()) => {
                let _ = ack.send(Ok(()));
                Ok(())
            }
            Err(err) => {
                let message = format!("{err:#}");
                let _ = ack.send(Err(message.clone()));
                anyhow::bail!(message);
            }
        },
        WorkerOutbound::Shutdown => Ok(()),
    }
}

fn handle_worker_text(
    state: &Arc<BrokerShared>,
    link: &Arc<WorkerLink>,
    text: String,
) -> Result<()> {
    let message = serde_json::from_str::<Value>(&text).context("decode worker message")?;
    if let Some(signal) = inspect_rate_limit_fragment(link, &message, &text)? {
        let mut pool = state
            .worker_pool
            .lock()
            .map_err(|_| anyhow::anyhow!("worker pool lock poisoned"))?;
        let cooldown = if signal.safe_to_continue {
            Duration::from_secs(10 * 60)
        } else {
            Duration::from_secs(30 * 60)
        };
        pool.mark_cooldown(&link.worker_id, cooldown);
    }

    if is_response_message(&message) {
        return handle_worker_response(state, link, message);
    }
    if is_request_message(&message) {
        return handle_worker_request(state, link, message);
    }
    handle_worker_notification(state, link, message, text)
}

fn inspect_rate_limit_fragment(
    link: &WorkerLink,
    message: &Value,
    text: &str,
) -> Result<Option<rate_limit::StreamRateLimitSignal>> {
    let Some(key) = rate_limit_turn_key(message) else {
        return Ok(None);
    };
    let mut states = link
        .rate_limit_states
        .lock()
        .map_err(|_| anyhow::anyhow!("worker rate-limit lock poisoned"))?;
    let turn_state = states.entry(key).or_default();
    Ok(rate_limit::inspect_stream_fragment(text, turn_state))
}

fn rate_limit_turn_key(message: &Value) -> Option<RateLimitTurnKey> {
    Some(RateLimitTurnKey {
        thread_id: router::message_thread_id(message)?,
        turn_id: router::message_turn_id(message)?,
    })
}

fn handle_worker_response(
    state: &Arc<BrokerShared>,
    link: &Arc<WorkerLink>,
    mut response: Value,
) -> Result<()> {
    let Some(worker_request_id) = response.get("id").and_then(Value::as_u64) else {
        return Ok(());
    };
    let pending = link
        .pending
        .lock()
        .map_err(|_| anyhow::anyhow!("worker pending lock poisoned"))?
        .remove(&worker_request_id);
    let Some(pending) = pending else {
        return Ok(());
    };
    if let Some(result) = response.get("result") {
        update_directory_from_result(state, &link.worker_id, result)?;
    }
    let is_error = response.get("error").is_some();
    if !is_error {
        mark_pending_turn_started_from_response(state, link, &pending, &response)?;
    }
    if let Some(reservation) = pending.turn_reservation.as_ref() {
        state.release_turn_reservation(reservation)?;
    }
    if is_error {
        if let Some(thread_id) = pending.thread_id.as_deref() {
            rollback_client_subscription(state, pending.client_id, thread_id)?;
        }
    }
    response["id"] = pending.client_request_id;
    state
        .subscriptions
        .send_client(pending.client_id, serde_json::to_string(&response)?)?;
    Ok(())
}

fn mark_pending_turn_started_from_response(
    state: &Arc<BrokerShared>,
    link: &WorkerLink,
    pending: &PendingWorkerRequest,
    response: &Value,
) -> Result<()> {
    if pending.turn_reservation.is_none() {
        return Ok(());
    }
    let Some(thread_id) = pending.thread_id.as_deref() else {
        return Ok(());
    };
    let Some(turn_id) = router::message_turn_id(response) else {
        return Ok(());
    };
    let accepted = state.directory.mark_turn_started(
        thread_id,
        link.worker_id.clone(),
        Some(turn_id.clone()),
    )?;
    if accepted {
        state
            .worker_pool
            .lock()
            .map_err(|_| anyhow::anyhow!("worker pool lock poisoned"))?
            .mark_turn_started(&link.worker_id, thread_id, Some(&turn_id));
    }
    Ok(())
}

fn handle_worker_request(
    state: &Arc<BrokerShared>,
    link: &Arc<WorkerLink>,
    mut request: Value,
) -> Result<()> {
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if !approval::is_approval_request_method(&method) {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        return link.send_raw(error_response(
            id,
            -32601,
            format!("unsupported worker request: {method}"),
        ));
    }

    let thread_id = router::message_thread_id(&request);
    let worker_request_id = request.get("id").cloned().unwrap_or(Value::Null);
    let approval =
        state
            .approvals
            .register(link.worker_id.clone(), worker_request_id, thread_id.clone())?;
    let broker_approval_id = approval.broker_approval_id.clone();
    request["id"] = Value::String(broker_approval_id.clone());
    let text = serde_json::to_string(&request)?;
    let sent = match thread_id.as_deref() {
        Some(thread_id) => state.subscriptions.fan_out_thread(thread_id, text)?,
        None => 0,
    };
    if sent == 0 {
        state.approvals.cancel(&broker_approval_id)?;
        let id = approval.worker_request_id;
        link.send_raw(error_response(
            id,
            -32000,
            "approval request has no subscribed broker client".to_string(),
        ))?;
    }
    Ok(())
}

fn handle_worker_notification(
    state: &Arc<BrokerShared>,
    link: &Arc<WorkerLink>,
    message: Value,
    text: String,
) -> Result<()> {
    let thread_id = router::message_thread_id(&message);
    let turn_id = router::message_turn_id(&message);
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if let Some(thread_id) = thread_id.as_deref() {
        match method {
            "turn/started" | "task/started" => {
                let accepted = state.directory.mark_turn_started(
                    thread_id,
                    link.worker_id.clone(),
                    turn_id.clone(),
                )?;
                if accepted {
                    state
                        .worker_pool
                        .lock()
                        .map_err(|_| anyhow::anyhow!("worker pool lock poisoned"))?
                        .mark_turn_started(&link.worker_id, thread_id, turn_id.as_deref());
                }
            }
            "turn/completed" => {
                state
                    .directory
                    .mark_turn_completed(thread_id, turn_id.as_deref())?;
                link.clear_rate_limit_state(thread_id, turn_id.as_deref())?;
                if state.subscriptions.subscriber_count(thread_id)? == 0 {
                    let _ = state.release_thread_without_subscribers(thread_id);
                }
                state
                    .worker_pool
                    .lock()
                    .map_err(|_| anyhow::anyhow!("worker pool lock poisoned"))?
                    .mark_turn_completed(&link.worker_id, thread_id, turn_id.as_deref());
            }
            _ => {}
        }
        let _sent = state.subscriptions.fan_out_thread(thread_id, text)?;
    }
    Ok(())
}

fn start_client_connection(state: Arc<BrokerShared>, socket: BrokerWebSocket) {
    thread::spawn(move || {
        if let Err(err) = run_client_connection(state, socket) {
            eprintln!("broker client connection failed: {err:#}");
        }
    });
}

fn run_client_connection(state: Arc<BrokerShared>, socket: BrokerWebSocket) -> Result<()> {
    let client_id = state.next_client_id();
    let (tx, rx) = mpsc::channel();
    state
        .subscriptions
        .register_client(client_id, ClientSink::new(tx.clone()))?;
    client_connection_loop(state, client_id, socket, tx, rx)
}

fn client_connection_loop(
    state: Arc<BrokerShared>,
    client_id: ClientId,
    socket: BrokerWebSocket,
    tx: mpsc::Sender<ClientOutbound>,
    rx: mpsc::Receiver<ClientOutbound>,
) -> Result<()> {
    let (reader, writer) = socket.split();
    let writer_handle = thread::spawn(move || client_writer_loop(writer, rx));
    let reader_result = client_reader_loop(Arc::clone(&state), client_id, reader, tx);
    cleanup_client(&state, client_id);
    let writer_result = match writer_handle.join() {
        Ok(result) => result,
        Err(_panic) => anyhow::bail!("broker client writer thread panicked"),
    };
    match (reader_result, writer_result) {
        (Err(err), _) => Err(err),
        (Ok(()), Err(err)) => Err(err),
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn client_reader_loop(
    state: Arc<BrokerShared>,
    client_id: ClientId,
    mut reader: WebSocketReader,
    tx: mpsc::Sender<ClientOutbound>,
) -> Result<()> {
    loop {
        match reader.read_text_blocking() {
            Ok(WebSocketMessage::Text(text)) => handle_client_text(&state, client_id, text)?,
            Ok(WebSocketMessage::Ping(payload)) => {
                let _ = tx.send(ClientOutbound::Pong(payload));
            }
            Ok(WebSocketMessage::Closed) => {
                let _ = reader.shutdown(Shutdown::Both);
                return Ok(());
            }
            Err(err) => {
                let _ = reader.shutdown(Shutdown::Both);
                return Err(err);
            }
        }
    }
}

fn client_writer_loop(
    mut writer: WebSocketWriter,
    rx: mpsc::Receiver<ClientOutbound>,
) -> Result<()> {
    let result = loop {
        match rx.recv() {
            Ok(ClientOutbound::Text(text)) => {
                if let Err(err) = writer.send_text(&text) {
                    break Err(err);
                }
            }
            Ok(ClientOutbound::Pong(payload)) => {
                if let Err(err) = writer.send_pong(&payload) {
                    break Err(err);
                }
            }
            Err(mpsc::RecvError) => break Ok(()),
        }
    };
    let _ = writer.shutdown(Shutdown::Both);
    result
}

fn cleanup_client(state: &BrokerShared, client_id: ClientId) {
    if let Ok(empty_threads) = state.subscriptions.unregister_client(client_id) {
        for thread_id in empty_threads {
            let _ = state.release_thread_without_subscribers(&thread_id);
        }
    }
}

fn handle_client_text(state: &Arc<BrokerShared>, client_id: ClientId, text: String) -> Result<()> {
    let message = serde_json::from_str::<Value>(&text).context("decode client message")?;
    if is_response_message(&message) {
        return handle_client_response(state, client_id, message);
    }
    if !is_request_message(&message) {
        return Ok(());
    }

    let id = message.get("id").cloned().unwrap_or(Value::Null);
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let route = router::route_request(&method, message.get("params"));
    let rollback_thread = route_subscription_thread(&route);
    if let Err(err) = handle_client_request(state, client_id, message, id.clone(), route) {
        if let Some(thread_id) = rollback_thread.as_deref() {
            rollback_client_subscription(state, client_id, thread_id)?;
        }
        send_jsonrpc_error_to_client(state, client_id, id, -32000, format!("{err:#}"))?;
    }
    Ok(())
}

fn handle_client_request(
    state: &Arc<BrokerShared>,
    client_id: ClientId,
    message: Value,
    id: Value,
    route: BrokerRoute,
) -> Result<()> {
    let params = message.get("params");
    match route {
        BrokerRoute::Initialize => {
            let response = success_response(id, broker_initialize_result(&state.paths));
            state
                .subscriptions
                .send_client(client_id, serde_json::to_string(&response)?)?;
        }
        BrokerRoute::ThreadList => {
            let response = success_response(id, aggregate_thread_list(state, params)?);
            state
                .subscriptions
                .send_client(client_id, serde_json::to_string(&response)?)?;
        }
        BrokerRoute::ThreadUnsubscribe { thread_id } => {
            let remaining = state.subscriptions.unsubscribe(client_id, &thread_id)?;
            if remaining == 0 {
                let _ = state.release_thread_without_subscribers(&thread_id);
            }
            let response = success_response(id, json!({}));
            state
                .subscriptions
                .send_client(client_id, serde_json::to_string(&response)?)?;
        }
        BrokerRoute::ThreadRead { thread_id } | BrokerRoute::WorkerByThread { thread_id } => {
            state.subscriptions.subscribe(client_id, &thread_id)?;
            let assignment = state.choose_worker_for_thread(&thread_id)?;
            state.send_worker_request(
                assignment.worker,
                client_id,
                message,
                WorkerRequestContext {
                    thread_id: Some(thread_id),
                    turn_reservation: None,
                },
            )?;
        }
        BrokerRoute::ThreadResume { thread_id } => {
            state.subscriptions.subscribe(client_id, &thread_id)?;
            let assignment = state.choose_worker_for_thread(&thread_id)?;
            let request = if state
                .resume_should_use_loaded_owner(&assignment.worker.worker_id, &thread_id)?
            {
                strip_thread_resume_path(message)
            } else {
                message
            };
            state.send_worker_request(
                assignment.worker,
                client_id,
                request,
                WorkerRequestContext {
                    thread_id: Some(thread_id),
                    turn_reservation: None,
                },
            )?;
        }
        BrokerRoute::TurnStart { thread_id } => {
            state.subscriptions.subscribe(client_id, &thread_id)?;
            let assignment = state.choose_worker_for_new_turn(&thread_id)?;
            state.send_worker_request(
                assignment.worker,
                client_id,
                message,
                WorkerRequestContext {
                    thread_id: Some(thread_id),
                    turn_reservation: assignment.turn_reservation,
                },
            )?;
        }
        BrokerRoute::TurnSteer { thread_id, turn_id }
        | BrokerRoute::TurnInterrupt { thread_id, turn_id } => {
            state.subscriptions.subscribe(client_id, &thread_id)?;
            let worker = if let Some(owner) = state.directory.owner_worker(&thread_id)? {
                state
                    .worker_pool
                    .lock()
                    .map_err(|_| anyhow::anyhow!("worker pool lock poisoned"))?
                    .worker(&owner)
                    .with_context(|| format!("worker for turn {turn_id} is unavailable"))?
            } else {
                state.choose_worker_for_thread(&thread_id)?.worker
            };
            state.send_worker_request(
                worker,
                client_id,
                message,
                WorkerRequestContext {
                    thread_id: Some(thread_id),
                    turn_reservation: None,
                },
            )?;
        }
        BrokerRoute::ThreadStart | BrokerRoute::WorkerDefault => {
            let worker = state.choose_default_worker()?;
            state.send_worker_request(
                worker,
                client_id,
                message,
                WorkerRequestContext {
                    thread_id: None,
                    turn_reservation: None,
                },
            )?;
        }
    }
    Ok(())
}

fn route_subscription_thread(route: &BrokerRoute) -> Option<String> {
    match route {
        BrokerRoute::ThreadRead { thread_id }
        | BrokerRoute::ThreadResume { thread_id }
        | BrokerRoute::TurnStart { thread_id }
        | BrokerRoute::TurnSteer { thread_id, .. }
        | BrokerRoute::TurnInterrupt { thread_id, .. }
        | BrokerRoute::WorkerByThread { thread_id } => Some(thread_id.clone()),
        BrokerRoute::Initialize
        | BrokerRoute::ThreadList
        | BrokerRoute::ThreadStart
        | BrokerRoute::ThreadUnsubscribe { .. }
        | BrokerRoute::WorkerDefault => None,
    }
}

fn rollback_client_subscription(
    state: &BrokerShared,
    client_id: ClientId,
    thread_id: &str,
) -> Result<()> {
    let remaining = state.subscriptions.unsubscribe(client_id, thread_id)?;
    if remaining == 0 {
        let _ = state.release_thread_without_subscribers(thread_id);
    }
    Ok(())
}

fn send_jsonrpc_error_to_client(
    state: &Arc<BrokerShared>,
    client_id: ClientId,
    id: Value,
    code: i64,
    message: String,
) -> Result<()> {
    let response = error_response(id, code, message);
    let _sent = state
        .subscriptions
        .send_client(client_id, serde_json::to_string(&response)?)?;
    Ok(())
}

fn handle_client_response(
    state: &Arc<BrokerShared>,
    client_id: ClientId,
    mut response: Value,
) -> Result<()> {
    let Some(id) = response
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Ok(());
    };
    match state.approvals.resolve_response(&id, response.clone())? {
        ApprovalResponseRoute::Forward {
            broker_approval_id,
            worker_id,
            worker_request_id,
            response: _,
        } => {
            response["id"] = worker_request_id;
            if let Err(err) = state.send_worker_response(&worker_id, response) {
                state.approvals.restore_response(&broker_approval_id)?;
                return Err(err);
            }
            state.approvals.commit_response(&broker_approval_id)?;
        }
        ApprovalResponseRoute::AlreadyHandled => {
            let stale =
                error_response(Value::String(id), -32001, "approval already handled".into());
            let _ = state
                .subscriptions
                .send_client(client_id, serde_json::to_string(&stale)?)?;
        }
        ApprovalResponseRoute::Unknown => {
            let stale = error_response(
                Value::String(id),
                -32001,
                "approval is no longer pending".into(),
            );
            let _ = state
                .subscriptions
                .send_client(client_id, serde_json::to_string(&stale)?)?;
        }
    }
    Ok(())
}

fn aggregate_thread_list(state: &Arc<BrokerShared>, params: Option<&Value>) -> Result<Value> {
    let page = ThreadListPageRequest::from_params(params);
    let worker_params = worker_thread_list_params(params, &page);
    let workers = state
        .worker_pool
        .lock()
        .map_err(|_| anyhow::anyhow!("worker pool lock poisoned"))?
        .records();
    let mut by_id = BTreeMap::<String, Value>::new();
    for WorkerListResult { worker, result } in collect_worker_thread_lists(
        workers
            .into_iter()
            .filter(|worker| worker.status == WorkerStatus::Ready)
            .collect(),
        worker_params,
    ) {
        let result = match result {
            Ok(result) => result,
            Err(err) => {
                eprintln!(
                    "broker thread/list skipped worker {} slot={}: {err:#}",
                    worker.worker_id, worker.slot
                );
                continue;
            }
        };
        if let Some(data) = result.get("data").and_then(Value::as_array) {
            for thread in data {
                if thread.get("id").and_then(Value::as_str).is_some() {
                    state.directory.upsert_thread_value(
                        worker.worker_id.clone(),
                        &worker.slot,
                        thread,
                    )?;
                    merge_thread_summary(&mut by_id, thread.clone());
                }
            }
        }
    }

    for record in state.directory.records()? {
        let synthetic = synthetic_thread_summary(&record);
        if thread_matches_list_filter(&synthetic, params) {
            merge_thread_summary(&mut by_id, synthetic);
        }
    }
    annotate_broker_subscriber_counts(state, &mut by_id)?;
    let mut data = by_id.into_values().collect::<Vec<_>>();
    data.sort_by(|left, right| {
        right
            .get("updatedAt")
            .and_then(Value::as_i64)
            .cmp(&left.get("updatedAt").and_then(Value::as_i64))
    });
    let total = data.len();
    let start = page.offset.min(total);
    let end = page
        .limit
        .map(|limit| start.saturating_add(limit).min(total))
        .unwrap_or(total);
    let next_cursor = page
        .limit
        .and_then(|_| (end < total).then(|| broker_cursor(end)));
    let backwards_cursor = page
        .limit
        .and_then(|limit| (start > 0).then(|| broker_cursor(start.saturating_sub(limit))));
    let data = data
        .into_iter()
        .skip(start)
        .take(end - start)
        .collect::<Vec<_>>();
    Ok(json!({
        "data": data,
        "nextCursor": next_cursor,
        "backwardsCursor": backwards_cursor
    }))
}

fn annotate_broker_subscriber_counts(
    state: &BrokerShared,
    threads: &mut BTreeMap<String, Value>,
) -> Result<()> {
    let subscriber_counts = state.subscriptions.subscriber_counts()?;
    for thread in threads.values_mut() {
        let subscriber_count = thread
            .get("id")
            .and_then(Value::as_str)
            .and_then(|thread_id| subscriber_counts.get(thread_id).copied())
            .unwrap_or(0);
        if let Some(object) = thread.as_object_mut() {
            object.insert("brokerSubscriberCount".to_string(), json!(subscriber_count));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ThreadListPageRequest {
    offset: usize,
    limit: Option<usize>,
}

impl ThreadListPageRequest {
    fn from_params(params: Option<&Value>) -> Self {
        let limit = params
            .and_then(|params| params.get("limit"))
            .and_then(Value::as_u64)
            .and_then(|limit| usize::try_from(limit).ok())
            .filter(|limit| *limit > 0);
        let offset = params
            .and_then(|params| params.get("cursor"))
            .and_then(Value::as_str)
            .and_then(parse_broker_cursor)
            .unwrap_or(0);
        Self { offset, limit }
    }

    fn worker_limit(self) -> Option<u32> {
        let limit = self.limit?;
        let wanted = self.offset.saturating_add(limit).saturating_add(1);
        Some(wanted.min(u32::MAX as usize) as u32)
    }
}

fn worker_thread_list_params(
    params: Option<&Value>,
    page: &ThreadListPageRequest,
) -> Option<Value> {
    let mut params = params.cloned()?;
    if let Some(object) = params.as_object_mut() {
        object.remove("cursor");
        if let Some(limit) = page.worker_limit() {
            object.insert(String::from("limit"), json!(limit));
        }
    }
    Some(params)
}

fn parse_broker_cursor(cursor: &str) -> Option<usize> {
    cursor
        .strip_prefix("broker:")
        .or_else(|| cursor.strip_prefix("offset:"))
        .unwrap_or(cursor)
        .parse::<usize>()
        .ok()
}

fn broker_cursor(offset: usize) -> String {
    format!("broker:{offset}")
}

fn thread_matches_list_filter(thread: &Value, params: Option<&Value>) -> bool {
    let Some(params) = params else {
        return true;
    };
    if let Some(cwd) = params.get("cwd").and_then(Value::as_str) {
        if thread.get("cwd").and_then(Value::as_str) != Some(cwd) {
            return false;
        }
    }
    if let Some(archived) = params.get("archived").and_then(Value::as_bool) {
        let thread_archived = thread
            .get("archived")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if thread_archived != archived {
            return false;
        }
    }
    if let Some(source_kinds) = params.get("sourceKinds").and_then(Value::as_array) {
        let source_kind = thread_source_kind(thread);
        let matches_source = source_kinds
            .iter()
            .filter_map(Value::as_str)
            .any(|kind| kind == source_kind);
        if !matches_source {
            return false;
        }
    }
    if let Some(search_term) = params
        .get("searchTerm")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|term| !term.is_empty())
    {
        let needle = search_term.to_ascii_lowercase();
        let searchable = [
            thread.get("id").and_then(Value::as_str),
            thread.get("name").and_then(Value::as_str),
            thread.get("preview").and_then(Value::as_str),
            thread.get("cwd").and_then(Value::as_str),
            thread.get("path").and_then(Value::as_str),
        ];
        if !searchable
            .into_iter()
            .flatten()
            .any(|value| value.to_ascii_lowercase().contains(&needle))
        {
            return false;
        }
    }
    true
}

fn thread_source_kind(thread: &Value) -> &str {
    let Some(source) = thread.get("source") else {
        return "unknown";
    };
    if let Some(source) = source.as_str() {
        return source;
    }
    if source.get("custom").is_some() {
        return "custom";
    }
    if source.get("subAgent").is_some() {
        return "subAgent";
    }
    "unknown"
}

#[derive(Debug)]
struct WorkerListResult {
    worker: WorkerRecord,
    result: Result<Value, String>,
}

fn collect_worker_thread_lists(
    workers: Vec<WorkerRecord>,
    params: Option<Value>,
) -> Vec<WorkerListResult> {
    let worker_count = workers.len();
    if worker_count == 0 {
        return Vec::new();
    }
    let deadline = Instant::now() + BLOCKING_WORKER_REQUEST_TIMEOUT;
    let (tx, rx) = mpsc::channel::<WorkerListResult>();
    for worker in workers {
        let tx = tx.clone();
        let params = params.clone();
        thread::spawn(move || {
            let result = blocking_worker_request_until(&worker, "thread/list", params, deadline)
                .map_err(|err| format!("{err:#}"));
            let _ = tx.send(WorkerListResult { worker, result });
        });
    }
    drop(tx);

    let mut results = Vec::new();
    while results.len() < worker_count {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        match rx.recv_timeout(remaining) {
            Ok(result) => results.push(result),
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    if results.len() < worker_count {
        eprintln!(
            "broker thread/list timed out after {}/{} workers",
            results.len(),
            worker_count
        );
    }
    results
}

fn merge_thread_summary(by_id: &mut BTreeMap<String, Value>, thread: Value) {
    let Some(thread_id) = thread.get("id").and_then(Value::as_str).map(str::to_string) else {
        return;
    };
    by_id
        .entry(thread_id)
        .and_modify(|existing| {
            if should_replace_thread_summary(existing, &thread) {
                *existing = thread.clone();
            }
        })
        .or_insert(thread);
}

fn should_replace_thread_summary(existing: &Value, incoming: &Value) -> bool {
    let existing_priority = thread_summary_priority(existing);
    let incoming_priority = thread_summary_priority(incoming);
    incoming_priority > existing_priority
        || (incoming_priority == existing_priority
            && should_replace_same_priority_thread_summary(existing, incoming))
}

fn should_replace_same_priority_thread_summary(existing: &Value, incoming: &Value) -> bool {
    let existing_updated_at = thread_summary_updated_at(existing);
    let incoming_updated_at = thread_summary_updated_at(incoming);
    incoming_updated_at > existing_updated_at
        || (incoming_updated_at == existing_updated_at
            && thread_summary_quality(incoming) > thread_summary_quality(existing))
}

fn thread_summary_priority(thread: &Value) -> u8 {
    match thread
        .get("status")
        .and_then(|status| status.get("type"))
        .and_then(Value::as_str)
    {
        Some("active") => 2,
        Some("notLoaded") => 0,
        _ => 1,
    }
}

fn thread_summary_updated_at(thread: &Value) -> i64 {
    thread.get("updatedAt").and_then(Value::as_i64).unwrap_or(0)
}

fn thread_summary_quality(thread: &Value) -> u8 {
    if is_synthetic_thread_summary_value(thread) {
        0
    } else {
        1
    }
}

fn is_synthetic_thread_summary_value(thread: &Value) -> bool {
    thread
        .get("preview")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .is_empty()
        && thread
            .get("turns")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
        && thread
            .get("source")
            .and_then(|source| source.get("custom"))
            .and_then(|custom| custom.get("name"))
            .and_then(Value::as_str)
            == Some("cx")
}

fn blocking_worker_request(
    worker: &WorkerRecord,
    method: &str,
    params: Option<Value>,
) -> Result<Value> {
    blocking_worker_request_until(
        worker,
        method,
        params,
        Instant::now() + BLOCKING_WORKER_REQUEST_TIMEOUT,
    )
}

fn blocking_worker_request_until(
    worker: &WorkerRecord,
    method: &str,
    params: Option<Value>,
    deadline: Instant,
) -> Result<Value> {
    let mut socket = BrokerWebSocket::connect(&worker.listen_url, Duration::from_millis(500))?;
    socket.send_text(&serde_json::to_string(&initialize_request(1))?)?;
    read_blocking_response(&mut socket, 1, deadline)?;
    let request = json!({
        "id": 2,
        "method": method,
        "params": params.unwrap_or(Value::Null),
    });
    socket.send_text(&serde_json::to_string(&request)?)?;
    read_blocking_response(&mut socket, 2, deadline)
}

fn resume_thread_on_worker(
    worker: &WorkerRecord,
    record: &crate::thread_directory::ThreadRecord,
) -> Result<Value> {
    blocking_worker_request(
        worker,
        "thread/resume",
        Some(json!({
            "threadId": record.thread_id,
            "path": record.path,
            "cwd": record.cwd,
            "excludeTurns": false,
        })),
    )
}

fn read_thread_on_worker(
    worker: &WorkerRecord,
    thread_id: &str,
    include_turns: bool,
) -> Result<Value> {
    let result = blocking_worker_request(
        worker,
        "thread/read",
        Some(json!({
            "threadId": thread_id,
            "includeTurns": include_turns,
        })),
    )?;
    result
        .get("thread")
        .cloned()
        .context("thread/read response omitted thread")
}

fn read_blocking_response(
    socket: &mut BrokerWebSocket,
    expected_id: u64,
    deadline: Instant,
) -> Result<Value> {
    loop {
        let now = Instant::now();
        if now >= deadline {
            anyhow::bail!("timed out waiting for worker response id {expected_id}");
        }
        socket.set_read_timeout(Some(deadline.saturating_duration_since(now)))?;
        match socket.read_text_blocking() {
            Ok(Some(text)) => {
                let message = serde_json::from_str::<Value>(&text)?;
                if message.get("id").and_then(Value::as_u64) != Some(expected_id) {
                    continue;
                }
                if let Some(error) = message.get("error") {
                    anyhow::bail!("worker request failed: {error}");
                }
                return message
                    .get("result")
                    .cloned()
                    .context("worker response omitted result");
            }
            Ok(None) => anyhow::bail!("worker websocket closed"),
            Err(err) if is_timeout_error(&err) => {
                anyhow::bail!("timed out waiting for worker response id {expected_id}");
            }
            Err(err) => return Err(err),
        }
    }
}

fn is_timeout_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|err| {
            matches!(
                err.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            )
        })
    })
}

fn update_directory_from_result(
    state: &Arc<BrokerShared>,
    worker_id: &WorkerId,
    result: &Value,
) -> Result<()> {
    if result.get("thread").is_none() && result.get("data").and_then(Value::as_array).is_none() {
        return Ok(());
    }
    let worker = state
        .worker_pool
        .lock()
        .map_err(|_| anyhow::anyhow!("worker pool lock poisoned"))?
        .worker(worker_id)
        .with_context(|| format!("unknown worker {worker_id}"))?;
    if let Some(thread) = result.get("thread") {
        state
            .directory
            .upsert_thread_value(worker_id.clone(), &worker.slot, thread)?;
    }
    if let Some(data) = result.get("data").and_then(Value::as_array) {
        for thread in data {
            state
                .directory
                .upsert_thread_value(worker_id.clone(), &worker.slot, thread)?;
        }
    }
    Ok(())
}

fn synthetic_thread_summary(record: &crate::thread_directory::ThreadRecord) -> Value {
    let status = match record.loaded_status {
        LoadedStatus::NotLoaded => "notLoaded",
        LoadedStatus::Loaded => "idle",
        LoadedStatus::Active => "active",
    };
    json!({
        "id": record.thread_id,
        "sessionId": record.codex_session_id,
        "cxSessionId": record.cx_session_id,
        "path": record.path,
        "name": record.title,
        "preview": "",
        "cwd": record.cwd,
        "source": {"custom": {"name": "cx"}},
        "status": {"type": status},
        "createdAt": record.updated_at_unix as i64,
        "updatedAt": record.updated_at_unix as i64,
        "turns": [],
    })
}

fn broker_initialize_result(paths: &ManagerPaths) -> Value {
    json!({
        "userAgent": format!("cx-broker/{}", env!("CARGO_PKG_VERSION")),
        "codexHome": paths.base_codex_home.display().to_string(),
        "platformFamily": std::env::consts::FAMILY,
        "platformOs": std::env::consts::OS,
    })
}

fn initialize_request(id: u64) -> Value {
    json!({
        "id": id,
        "method": "initialize",
        "params": {
            "clientInfo": {
                "name": "cx-broker",
                "version": env!("CARGO_PKG_VERSION")
            },
            "capabilities": {
                "experimentalApi": true
            }
        }
    })
}

fn success_response(id: Value, result: Value) -> Value {
    json!({
        "id": id,
        "result": result,
    })
}

fn error_response(id: Value, code: i64, message: String) -> Value {
    json!({
        "id": id,
        "error": {
            "code": code,
            "message": message,
        }
    })
}

fn is_request_message(message: &Value) -> bool {
    message.get("id").is_some() && message.get("method").and_then(Value::as_str).is_some()
}

fn is_response_message(message: &Value) -> bool {
    message.get("id").is_some() && message.get("method").is_none()
}

fn health_response_bytes(ready: bool) -> &'static [u8] {
    if ready {
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
    } else {
        b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 12\r\nConnection: close\r\n\r\nunavailable"
    }
}

fn parse_loopback_socket_addr(listen_url: &str) -> String {
    listen_url
        .strip_prefix("ws://")
        .unwrap_or("127.0.0.1:0")
        .to_string()
}

impl BrokerListenUrl {
    fn parse(raw: &str) -> Result<Self> {
        let Some(rest) = raw.strip_prefix("ws://") else {
            anyhow::bail!("broker --listen only supports ws:// loopback URLs");
        };
        if rest.contains('/') {
            anyhow::bail!("broker --listen must not include a path");
        }
        let Some((host, port)) = rest.rsplit_once(':') else {
            anyhow::bail!("broker --listen requires host:port");
        };
        if !matches!(host, "127.0.0.1" | "localhost") {
            anyhow::bail!("broker --listen must bind a loopback host");
        }
        Ok(Self {
            host: host.to_string(),
            port: port
                .parse::<u16>()
                .with_context(|| format!("invalid broker port: {port}"))?,
        })
    }

    fn resolve(self) -> Result<Self> {
        if self.port != 0 {
            return Ok(self);
        }
        let listener = TcpListener::bind(("127.0.0.1", 0)).context("reserve broker port")?;
        let port = listener.local_addr().context("read broker port")?.port();
        drop(listener);
        Ok(Self { port, ..self })
    }

    fn websocket_url(&self) -> String {
        format!("ws://{}:{}", self.host, self.port)
    }

    fn readyz_url(&self) -> String {
        format!("http://{}:{}/readyz", self.host, self.port)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::io::Read;
    use std::io::Write;

    use serde_json::json;
    use serde_json::Value;

    use super::*;

    fn worker(cooldown_until_unix: Option<u64>) -> WorkerRecord {
        WorkerRecord {
            worker_id: WorkerId::new("dia4", 1),
            slot: String::from("dia4"),
            generation: 1,
            pid: 123,
            listen_url: String::from("ws://127.0.0.1:17654"),
            readyz_url: String::from("http://127.0.0.1:17654/readyz"),
            status: WorkerStatus::Ready,
            active_turns: 0,
            cooldown_until_unix,
            started_at_unix: 1_800_000_000,
        }
    }

    fn test_link(worker_id: WorkerId) -> Arc<WorkerLink> {
        let (link, _rx) = test_link_with_rx(worker_id);
        link
    }

    fn test_link_with_rx(worker_id: WorkerId) -> (Arc<WorkerLink>, mpsc::Receiver<WorkerOutbound>) {
        let (tx, rx) = mpsc::channel();
        let link = Arc::new(WorkerLink {
            worker_id,
            sender: tx,
            pending: Mutex::new(BTreeMap::new()),
            next_request_id: AtomicU64::new(1),
            rate_limit_states: Mutex::new(BTreeMap::new()),
            closed: AtomicBool::new(false),
        });
        (link, rx)
    }

    fn test_state() -> Arc<BrokerShared> {
        let root = std::env::temp_dir().join(format!("cx-broker-test-{}", std::process::id()));
        let paths = ManagerPaths::from_roots(root.join("codex"), root.join("profile-manager"));
        Arc::new(BrokerShared {
            paths: paths.clone(),
            worker_pool: Mutex::new(WorkerPool::new(
                paths,
                WorkerPoolConfig {
                    codex_bin: None,
                    slot: None,
                    target: None,
                },
            )),
            worker_links: Mutex::new(BTreeMap::new()),
            directory: ThreadDirectory::default(),
            subscriptions: SubscriptionHub::default(),
            approvals: ApprovalBroker::default(),
            next_client_id: AtomicU64::new(1),
            next_turn_reservation_id: AtomicU64::new(1),
            shutdown: AtomicBool::new(false),
        })
    }

    fn worker_with_url(listen_url: String) -> WorkerRecord {
        let mut record = worker(None);
        let port = listen_url
            .rsplit_once(':')
            .and_then(|(_, port)| port.parse::<u16>().ok())
            .unwrap_or(0);
        record.listen_url = listen_url;
        record.readyz_url = format!("http://127.0.0.1:{port}/readyz");
        record
    }

    fn listen_ws_url() -> (TcpListener, String) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, format!("ws://127.0.0.1:{port}"))
    }

    fn accept_test_websocket(listener: TcpListener) -> Result<BrokerWebSocket> {
        let (mut stream, _addr) = listener.accept().context("accept test websocket")?;
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .context("set test websocket handshake timeout")?;
        let request = ws::read_http_head(&mut stream)?;
        let response = BrokerWebSocket::accept_response(&request)?;
        stream.write_all(response.as_bytes())?;
        BrokerWebSocket::finish_accept_with_buffer(stream, Vec::new())
    }

    fn recv_json_timeout(rx: &mpsc::Receiver<Value>) -> Value {
        rx.recv_timeout(Duration::from_secs(2))
            .expect("timed out waiting for websocket message")
    }

    fn recv_client_text(rx: &mpsc::Receiver<ClientOutbound>) -> String {
        match rx
            .recv_timeout(Duration::from_secs(2))
            .expect("timed out waiting for client message")
        {
            ClientOutbound::Text(text) => text,
            ClientOutbound::Pong(_) => panic!("expected client text message, got pong"),
        }
    }

    fn masked_client_text_frame(text: &str) -> Vec<u8> {
        let payload = text.as_bytes();
        assert!(payload.len() < 126);
        let mask = [1_u8, 2, 3, 4];
        let mut frame = vec![0x81, 0x80 | payload.len() as u8];
        frame.extend_from_slice(&mask);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ mask[index % mask.len()]),
        );
        frame
    }

    fn read_server_text_frame(stream: &mut TcpStream) -> String {
        let mut header = [0_u8; 2];
        stream.read_exact(&mut header).unwrap();
        assert_eq!(header[0] & 0x0f, 0x1);
        assert_eq!(header[1] & 0x80, 0);
        let len = match header[1] & 0x7f {
            len if len < 126 => usize::from(len),
            126 => {
                let mut extended = [0_u8; 2];
                stream.read_exact(&mut extended).unwrap();
                usize::from(u16::from_be_bytes(extended))
            }
            127 => {
                let mut extended = [0_u8; 8];
                stream.read_exact(&mut extended).unwrap();
                usize::try_from(u64::from_be_bytes(extended)).unwrap()
            }
            _ => unreachable!(),
        };
        let mut payload = vec![0_u8; len];
        stream.read_exact(&mut payload).unwrap();
        String::from_utf8(payload).unwrap()
    }

    fn try_recv_client_text(rx: &mpsc::Receiver<ClientOutbound>) -> String {
        match rx.try_recv().expect("missing client message") {
            ClientOutbound::Text(text) => text,
            ClientOutbound::Pong(_) => panic!("expected client text message, got pong"),
        }
    }

    fn thread(status: LoadedStatus) -> ThreadRecord {
        ThreadRecord {
            thread_id: String::from("thread-1"),
            cx_session_id: None,
            codex_session_id: None,
            cwd: String::from("/repo"),
            path: Some(String::from("/rollout.jsonl")),
            title: None,
            origin_slot: Some(String::from("dia4")),
            owner_worker_id: Some(WorkerId::new("dia4", 1)),
            loaded_status: status,
            active_turn_id: None,
            active_turn_ids: BTreeSet::new(),
            updated_at_unix: 10,
        }
    }

    fn directory_thread(thread_id: &str, updated_at_unix: u64) -> ThreadRecord {
        ThreadRecord {
            thread_id: thread_id.to_string(),
            cx_session_id: None,
            codex_session_id: None,
            cwd: String::from("/repo"),
            path: Some(format!("/{thread_id}.jsonl")),
            title: Some(thread_id.to_string()),
            origin_slot: Some(String::from("dia4")),
            owner_worker_id: None,
            loaded_status: LoadedStatus::NotLoaded,
            active_turn_id: None,
            active_turn_ids: BTreeSet::new(),
            updated_at_unix,
        }
    }

    #[test]
    fn cooldown_worker_does_not_accept_new_turns() {
        let worker = worker(Some(u64::MAX));

        assert!(!worker.accepts_new_turns());
    }

    #[test]
    fn directory_active_summary_replaces_worker_not_loaded_summary() {
        let mut by_id = BTreeMap::new();
        merge_thread_summary(
            &mut by_id,
            json!({
                "id": "thread-1",
                "status": {"type": "notLoaded"},
                "updatedAt": 20
            }),
        );
        merge_thread_summary(
            &mut by_id,
            synthetic_thread_summary(&thread(LoadedStatus::Active)),
        );

        assert_eq!(
            by_id["thread-1"]
                .get("status")
                .and_then(|status| status.get("type"))
                .and_then(Value::as_str),
            Some("active")
        );
    }

    #[test]
    fn equal_priority_thread_summary_uses_newer_updated_at() {
        let mut by_id = BTreeMap::new();
        merge_thread_summary(
            &mut by_id,
            json!({
                "id": "thread-1",
                "name": "old",
                "status": {"type": "idle"},
                "updatedAt": 10
            }),
        );
        merge_thread_summary(
            &mut by_id,
            json!({
                "id": "thread-1",
                "name": "new",
                "status": {"type": "idle"},
                "updatedAt": 20
            }),
        );
        merge_thread_summary(
            &mut by_id,
            json!({
                "id": "thread-1",
                "name": "stale",
                "status": {"type": "idle"},
                "updatedAt": 5
            }),
        );

        assert_eq!(
            by_id["thread-1"].get("name").and_then(Value::as_str),
            Some("new")
        );
    }

    #[test]
    fn synthetic_thread_summary_does_not_replace_real_summary_at_same_timestamp() {
        let mut by_id = BTreeMap::new();
        merge_thread_summary(
            &mut by_id,
            json!({
                "id": "thread-1",
                "preview": "real preview",
                "source": "cli",
                "turns": [{"id": "turn-1"}],
                "status": {"type": "idle"},
                "updatedAt": 10
            }),
        );
        merge_thread_summary(
            &mut by_id,
            synthetic_thread_summary(&thread(LoadedStatus::Loaded)),
        );

        assert_eq!(
            by_id["thread-1"].get("preview").and_then(Value::as_str),
            Some("real preview")
        );
    }

    #[test]
    fn synthetic_thread_summary_respects_thread_list_filters() {
        let summary = synthetic_thread_summary(&thread(LoadedStatus::Loaded));

        assert!(thread_matches_list_filter(
            &summary,
            Some(&json!({"cwd": "/repo", "archived": false}))
        ));
        assert!(!thread_matches_list_filter(
            &summary,
            Some(&json!({"cwd": "/other", "archived": false}))
        ));
        assert!(!thread_matches_list_filter(
            &summary,
            Some(&json!({"cwd": "/repo", "sourceKinds": ["cli", "vscode"]}))
        ));
        assert!(thread_matches_list_filter(
            &summary,
            Some(&json!({"searchTerm": "thread-1"}))
        ));
        assert!(!thread_matches_list_filter(
            &summary,
            Some(&json!({"searchTerm": "missing"}))
        ));
    }

    #[test]
    fn aggregate_thread_list_applies_limit_and_broker_cursor() {
        let state = test_state();
        state.directory.upsert(directory_thread("new", 30)).unwrap();
        state
            .directory
            .upsert(directory_thread("middle", 20))
            .unwrap();
        state.directory.upsert(directory_thread("old", 10)).unwrap();

        let first = aggregate_thread_list(&state, Some(&json!({"limit": 2}))).unwrap();
        let first_ids = first
            .get("data")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter_map(|thread| thread.get("id").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert_eq!(first_ids, vec!["new", "middle"]);
        assert_eq!(
            first.get("nextCursor").and_then(Value::as_str),
            Some("broker:2")
        );

        let second =
            aggregate_thread_list(&state, Some(&json!({"limit": 2, "cursor": "broker:2"})))
                .unwrap();
        let second_ids = second
            .get("data")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter_map(|thread| thread.get("id").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert_eq!(second_ids, vec!["old"]);
        assert!(second.get("nextCursor").is_none_or(Value::is_null));
        assert_eq!(
            second.get("backwardsCursor").and_then(Value::as_str),
            Some("broker:0")
        );
    }

    #[test]
    fn aggregate_thread_list_includes_broker_subscriber_count() {
        let state = test_state();
        let client_id = ClientId::new(7);
        let (tx, _rx) = mpsc::channel();
        state
            .subscriptions
            .register_client(client_id, ClientSink::new(tx))
            .unwrap();
        state
            .subscriptions
            .subscribe(client_id, "thread-1")
            .unwrap();
        state
            .directory
            .upsert(directory_thread("thread-1", 30))
            .unwrap();
        state
            .directory
            .upsert(directory_thread("thread-2", 20))
            .unwrap();

        let page = aggregate_thread_list(&state, Some(&json!({"cwd": "/repo"}))).unwrap();
        let data = page.get("data").and_then(Value::as_array).unwrap();
        let subscribed = data
            .iter()
            .find(|thread| thread.get("id").and_then(Value::as_str) == Some("thread-1"))
            .unwrap();
        let idle = data
            .iter()
            .find(|thread| thread.get("id").and_then(Value::as_str) == Some("thread-2"))
            .unwrap();

        assert_eq!(
            subscribed
                .get("brokerSubscriberCount")
                .and_then(Value::as_u64),
            Some(1)
        );
        assert_eq!(
            idle.get("brokerSubscriberCount").and_then(Value::as_u64),
            Some(0)
        );
    }

    #[test]
    fn slow_preflight_connection_does_not_block_readyz_response() {
        let state = test_state();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let reactor_state = Arc::clone(&state);
        let reactor = thread::spawn(move || {
            preflight::run_preflight_reactor_with_limits(
                reactor_state,
                listener,
                preflight::PreflightLimits::new_for_test(8, 4),
            )
        });

        let mut slow_clients = Vec::new();
        for _ in 0..8 {
            let mut stream = TcpStream::connect(addr).unwrap();
            stream.write_all(b"GET /readyz").unwrap();
            slow_clients.push(stream);
        }
        thread::sleep(Duration::from_millis(50));

        let mut ready_client = TcpStream::connect(addr).unwrap();
        let started = Instant::now();
        ready_client
            .write_all(
                format!("GET /readyz HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .unwrap();
        ready_client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut response = String::new();
        ready_client.read_to_string(&mut response).unwrap();

        assert!(response.starts_with("HTTP/1.1 503 Service Unavailable"));
        assert!(started.elapsed() < Duration::from_millis(100));

        state.shutdown.store(true, Ordering::Release);
        drop(slow_clients);
        if let Ok(stream) = TcpStream::connect(addr) {
            let _ = stream.shutdown(Shutdown::Both);
        }
        reactor.join().unwrap().unwrap();
    }

    #[test]
    fn preflight_preserves_websocket_frame_sent_with_http_head() {
        let state = test_state();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let reactor_state = Arc::clone(&state);
        let reactor =
            thread::spawn(move || preflight::run_preflight_reactor(reactor_state, listener));
        let request = format!(
            "GET / HTTP/1.1\r\n\
             Host: {addr}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n\
             \r\n"
        );
        let initialize = json!({
            "id": 1,
            "method": "initialize",
            "params": {}
        })
        .to_string();
        let mut bytes = request.into_bytes();
        bytes.extend(masked_client_text_frame(&initialize));

        let mut client = TcpStream::connect(addr).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        client.write_all(&bytes).unwrap();
        let response = ws::read_http_head(&mut client).unwrap();
        assert!(ws::request_line(&response).starts_with("HTTP/1.1 101 "));
        let text = read_server_text_frame(&mut client);
        let message = serde_json::from_str::<Value>(&text).unwrap();

        assert_eq!(message.get("id").and_then(Value::as_i64), Some(1));
        assert!(message.get("result").is_some());

        state.shutdown.store(true, Ordering::Release);
        drop(client);
        if let Ok(stream) = TcpStream::connect(addr) {
            let _ = stream.shutdown(Shutdown::Both);
        }
        reactor.join().unwrap().unwrap();
    }

    #[test]
    fn broker_cursor_expands_worker_limit_for_deep_pages() {
        let params = worker_thread_list_params(
            Some(&json!({"limit": 25, "cursor": "broker:1500"})),
            &ThreadListPageRequest::from_params(Some(&json!({
                "limit": 25,
                "cursor": "broker:1500"
            }))),
        )
        .unwrap();

        assert_eq!(params.get("cursor"), None);
        assert_eq!(params.get("limit").and_then(Value::as_u64), Some(1526));
    }

    #[test]
    fn migrated_thread_releases_only_previous_different_owner() {
        let previous = WorkerId::new("dia4", 1);
        let selected = WorkerId::new("dia5", 2);

        assert_eq!(
            previous_owner_to_release(Some(&previous), &selected),
            Some(&previous)
        );
        assert_eq!(previous_owner_to_release(Some(&selected), &selected), None);
        assert_eq!(previous_owner_to_release(None, &selected), None);
    }

    #[test]
    fn active_threads_keep_worker_subscription_without_clients() {
        assert!(!should_unsubscribe_worker_thread(&thread(
            LoadedStatus::Active
        )));
        assert!(should_unsubscribe_worker_thread(&thread(
            LoadedStatus::Loaded
        )));
        assert!(should_unsubscribe_worker_thread(&thread(
            LoadedStatus::NotLoaded
        )));
    }

    #[test]
    fn client_writer_sends_without_socket_read_timeout() {
        let (listener, url) = listen_ws_url();
        let (writer_tx, writer_rx) = mpsc::channel();
        let server = thread::spawn(move || -> Result<()> {
            let socket = accept_test_websocket(listener)?;
            let (_reader, writer) = socket.split();
            let (tx, rx) = mpsc::channel();
            writer_tx.send(tx).unwrap();
            client_writer_loop(writer, rx)
        });
        let mut client = BrokerWebSocket::connect(&url, Duration::from_secs(1)).unwrap();
        let tx = writer_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let started = Instant::now();
        tx.send(ClientOutbound::Text("client-message".to_string()))
            .unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let text = client.read_text_blocking().unwrap().unwrap();

        assert_eq!(text, "client-message");
        assert!(started.elapsed() < Duration::from_millis(100));
        drop(tx);
        drop(client);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn worker_writer_sends_request_without_reader_timeout() {
        let (listener, url) = listen_ws_url();
        let (messages_tx, messages_rx) = mpsc::channel();
        let server = thread::spawn(move || -> Result<()> {
            let mut socket = accept_test_websocket(listener)?;
            socket.set_read_timeout(Some(Duration::from_secs(2)))?;
            for _ in 0..2 {
                let text = socket
                    .read_text_blocking()?
                    .context("test worker websocket closed")?;
                messages_tx
                    .send(serde_json::from_str::<Value>(&text)?)
                    .unwrap();
            }
            Ok(())
        });
        let state = test_state();
        let link = WorkerLink::start(worker_with_url(url), state).unwrap();
        let initialize = recv_json_timeout(&messages_rx);
        assert_eq!(
            initialize.get("method").and_then(Value::as_str),
            Some("initialize")
        );

        let started = Instant::now();
        link.send_request(
            ClientId::new(7),
            json!({"id": "client-request", "method": "thread/read", "params": {"threadId": "thread-1"}}),
            WorkerRequestContext {
                thread_id: Some(String::from("thread-1")),
                turn_reservation: None,
            },
        )
        .unwrap();
        let request = recv_json_timeout(&messages_rx);

        assert!(started.elapsed() < Duration::from_millis(100));
        assert_eq!(request.get("id").and_then(Value::as_u64), Some(1));
        assert_eq!(
            request.get("method").and_then(Value::as_str),
            Some("thread/read")
        );
        link.close();
        server.join().unwrap().unwrap();
    }

    #[test]
    fn worker_close_reports_pending_client_request() {
        let (listener, url) = listen_ws_url();
        let (messages_tx, messages_rx) = mpsc::channel();
        let server = thread::spawn(move || -> Result<()> {
            let mut socket = accept_test_websocket(listener)?;
            socket.set_read_timeout(Some(Duration::from_secs(2)))?;
            for _ in 0..2 {
                let text = socket
                    .read_text_blocking()?
                    .context("test worker websocket closed")?;
                messages_tx
                    .send(serde_json::from_str::<Value>(&text)?)
                    .unwrap();
            }
            Ok(())
        });
        let state = test_state();
        let client_id = ClientId::new(7);
        let (tx, rx) = mpsc::channel();
        state
            .subscriptions
            .register_client(client_id, ClientSink::new(tx))
            .unwrap();
        let record = worker_with_url(url);
        state.ensure_worker_link(&record).unwrap();
        let _initialize = recv_json_timeout(&messages_rx);

        state
            .send_worker_request(
                record,
                client_id,
                json!({"id": "client-request", "method": "thread/start", "params": {}}),
                WorkerRequestContext {
                    thread_id: None,
                    turn_reservation: None,
                },
            )
            .unwrap();
        let _request = recv_json_timeout(&messages_rx);

        let response = serde_json::from_str::<Value>(&recv_client_text(&rx)).unwrap();
        assert_eq!(response.get("id"), Some(&json!("client-request")));
        assert_eq!(
            response.pointer("/error/code").and_then(Value::as_i64),
            Some(-32002)
        );
        server.join().unwrap().unwrap();
    }

    #[test]
    fn approval_fanout_forwards_only_first_client_response() {
        let (listener, url) = listen_ws_url();
        let (worker_tx, worker_rx) = mpsc::channel();
        let server = thread::spawn(move || -> Result<()> {
            let mut socket = accept_test_websocket(listener)?;
            socket.set_read_timeout(Some(Duration::from_secs(2)))?;
            loop {
                match socket.read_text_blocking() {
                    Ok(Some(text)) => {
                        worker_tx
                            .send(serde_json::from_str::<Value>(&text)?)
                            .unwrap();
                    }
                    Ok(None) => return Ok(()),
                    Err(err) if is_timeout_error(&err) => return Ok(()),
                    Err(err) => return Err(err),
                }
            }
        });
        let state = test_state();
        let record = worker_with_url(url);
        let link = state.ensure_worker_link(&record).unwrap();
        let _initialize = recv_json_timeout(&worker_rx);
        state
            .worker_links
            .lock()
            .unwrap()
            .insert(record.worker_id.clone(), Arc::clone(&link));
        let client_a = ClientId::new(1);
        let client_b = ClientId::new(2);
        let (tx_a, rx_a) = mpsc::channel();
        let (tx_b, rx_b) = mpsc::channel();
        state
            .subscriptions
            .register_client(client_a, ClientSink::new(tx_a))
            .unwrap();
        state
            .subscriptions
            .register_client(client_b, ClientSink::new(tx_b))
            .unwrap();
        state.subscriptions.subscribe(client_a, "thread-1").unwrap();
        state.subscriptions.subscribe(client_b, "thread-1").unwrap();

        handle_worker_request(
            &state,
            &link,
            json!({
                "id": 77,
                "method": "item/commandExecution/requestApproval",
                "params": {"threadId": "thread-1", "command": "true"}
            }),
        )
        .unwrap();
        let approval_a = serde_json::from_str::<Value>(&recv_client_text(&rx_a)).unwrap();
        let approval_b = serde_json::from_str::<Value>(&recv_client_text(&rx_b)).unwrap();
        let approval_id = approval_a.get("id").and_then(Value::as_str).unwrap();
        assert_eq!(
            approval_b.get("id").and_then(Value::as_str),
            Some(approval_id)
        );

        handle_client_text(
            &state,
            client_a,
            json!({"id": approval_id, "result": {"decision": "approved"}}).to_string(),
        )
        .unwrap();
        let forwarded = recv_json_timeout(&worker_rx);
        assert_eq!(forwarded.get("id"), Some(&json!(77)));

        handle_client_text(
            &state,
            client_b,
            json!({"id": approval_id, "result": {"decision": "late"}}).to_string(),
        )
        .unwrap();
        let stale = serde_json::from_str::<Value>(&recv_client_text(&rx_b)).unwrap();
        assert_eq!(
            stale.pointer("/error/code").and_then(Value::as_i64),
            Some(-32001)
        );
        assert!(worker_rx.recv_timeout(Duration::from_millis(150)).is_err());
        link.close();
        server.join().unwrap().unwrap();
    }

    #[test]
    fn client_disconnect_cleans_subscription_without_releasing_active_thread() {
        let state = test_state();
        let worker_id = WorkerId::new("dia4", 1);
        let (link, worker_rx) = test_link_with_rx(worker_id.clone());
        state
            .worker_links
            .lock()
            .unwrap()
            .insert(worker_id, Arc::clone(&link));
        state
            .directory
            .upsert(thread(LoadedStatus::Active))
            .unwrap();
        let client_id = ClientId::new(7);
        let (tx, _rx) = mpsc::channel();
        state
            .subscriptions
            .register_client(client_id, ClientSink::new(tx))
            .unwrap();
        state
            .subscriptions
            .subscribe(client_id, "thread-1")
            .unwrap();

        cleanup_client(&state, client_id);

        assert_eq!(state.subscriptions.subscriber_count("thread-1").unwrap(), 0);
        assert_eq!(
            state
                .directory
                .record("thread-1")
                .unwrap()
                .unwrap()
                .loaded_status,
            LoadedStatus::Active
        );
        assert!(worker_rx.try_recv().is_err());
    }

    #[test]
    fn loaded_owner_resume_uses_owner_worker() {
        let selected = WorkerId::new("dia4", 1);
        let other = WorkerId::new("dia5", 1);

        assert!(resume_can_use_loaded_owner(
            Some(&thread(LoadedStatus::Loaded)),
            &selected
        ));
        assert!(resume_can_use_loaded_owner(
            Some(&thread(LoadedStatus::Active)),
            &selected
        ));
        assert!(!resume_can_use_loaded_owner(
            Some(&thread(LoadedStatus::NotLoaded)),
            &selected
        ));
        assert!(!resume_can_use_loaded_owner(
            Some(&thread(LoadedStatus::Loaded)),
            &other
        ));
        assert!(!resume_can_use_loaded_owner(None, &selected));
    }

    #[test]
    fn loaded_owner_resume_strips_stale_path_before_forwarding() {
        let request = json!({
            "id": 42,
            "method": "thread/resume",
            "params": {
                "threadId": "thread-1",
                "path": "/stale.jsonl",
                "cwd": "/repo",
                "excludeTurns": false
            }
        });

        let stripped = strip_thread_resume_path(request);

        assert_eq!(
            stripped
                .get("params")
                .and_then(|params| params.get("threadId"))
                .and_then(Value::as_str),
            Some("thread-1")
        );
        assert!(stripped
            .get("params")
            .and_then(|params| params.get("path"))
            .is_none());
    }

    #[test]
    fn evict_worker_link_only_removes_current_link() {
        let state = test_state();
        let worker_id = WorkerId::new("dia4", 1);
        let current = test_link(worker_id.clone());
        let stale = test_link(worker_id.clone());
        state
            .worker_links
            .lock()
            .unwrap()
            .insert(worker_id.clone(), Arc::clone(&current));

        assert!(!state
            .evict_worker_link_if_current(&worker_id, &stale)
            .unwrap());
        assert!(state
            .evict_worker_link_if_current(&worker_id, &current)
            .unwrap());
        assert!(!state.worker_links.lock().unwrap().contains_key(&worker_id));
    }

    #[test]
    fn closed_worker_link_reports_pending_requests_to_clients() {
        let state = test_state();
        let link = test_link(WorkerId::new("dia4", 1));
        let client_id = ClientId::new(42);
        let (tx, rx) = mpsc::channel();
        state
            .subscriptions
            .register_client(client_id, ClientSink::new(tx))
            .unwrap();
        link.pending.lock().unwrap().insert(
            7,
            PendingWorkerRequest {
                client_id,
                client_request_id: json!("client-request"),
                thread_id: Some(String::from("thread-1")),
                turn_reservation: None,
            },
        );

        state.fail_worker_link_pending(&link).unwrap();

        let response = serde_json::from_str::<Value>(&try_recv_client_text(&rx)).unwrap();
        assert_eq!(response.get("id"), Some(&json!("client-request")));
        assert_eq!(
            response.pointer("/error/message").and_then(Value::as_str),
            Some("worker link dia4:1 closed before response")
        );
        assert!(link.pending.lock().unwrap().is_empty());
    }

    #[test]
    fn request_failure_returns_jsonrpc_error_without_closing_client() {
        let state = test_state();
        let client_id = ClientId::new(7);
        let (tx, rx) = mpsc::channel();
        state
            .subscriptions
            .register_client(client_id, ClientSink::new(tx))
            .unwrap();

        handle_client_text(
            &state,
            client_id,
            json!({"id": "request-1", "method": "thread/start", "params": {}}).to_string(),
        )
        .unwrap();

        let response = serde_json::from_str::<Value>(&try_recv_client_text(&rx)).unwrap();
        assert_eq!(response.get("id"), Some(&json!("request-1")));
        assert!(response.get("error").is_some());
    }

    #[test]
    fn failed_thread_request_rolls_back_local_subscription() {
        let state = test_state();
        let client_id = ClientId::new(7);
        let (tx, _rx) = mpsc::channel();
        state
            .subscriptions
            .register_client(client_id, ClientSink::new(tx))
            .unwrap();

        handle_client_text(
            &state,
            client_id,
            json!({
                "id": "request-1",
                "method": "thread/read",
                "params": {"threadId": "thread-1"}
            })
            .to_string(),
        )
        .unwrap();

        assert_eq!(state.subscriptions.subscriber_count("thread-1").unwrap(), 0);
    }

    #[test]
    fn worker_error_response_rolls_back_pending_thread_subscription() {
        let state = test_state();
        let link = test_link(WorkerId::new("dia4", 1));
        let client_id = ClientId::new(7);
        let (tx, rx) = mpsc::channel();
        state
            .subscriptions
            .register_client(client_id, ClientSink::new(tx))
            .unwrap();
        state
            .subscriptions
            .subscribe(client_id, "thread-1")
            .unwrap();
        link.pending.lock().unwrap().insert(
            5,
            PendingWorkerRequest {
                client_id,
                client_request_id: json!("client-request"),
                thread_id: Some(String::from("thread-1")),
                turn_reservation: None,
            },
        );

        handle_worker_response(
            &state,
            &link,
            json!({
                "id": 5,
                "error": {"code": -32000, "message": "worker rejected request"}
            }),
        )
        .unwrap();

        assert_eq!(state.subscriptions.subscriber_count("thread-1").unwrap(), 0);
        let response = serde_json::from_str::<Value>(&try_recv_client_text(&rx)).unwrap();
        assert_eq!(response.get("id"), Some(&json!("client-request")));
        assert!(response.get("error").is_some());
    }

    #[test]
    fn turn_start_response_converts_reservation_to_active_turn() {
        let state = test_state();
        let link = test_link(WorkerId::new("dia4", 1));
        let client_id = ClientId::new(7);
        let (tx, rx) = mpsc::channel();
        state
            .subscriptions
            .register_client(client_id, ClientSink::new(tx))
            .unwrap();
        state
            .directory
            .upsert(thread(LoadedStatus::Loaded))
            .unwrap();
        link.pending.lock().unwrap().insert(
            5,
            PendingWorkerRequest {
                client_id,
                client_request_id: json!("client-request"),
                thread_id: Some(String::from("thread-1")),
                turn_reservation: Some(TurnReservation {
                    worker_id: link.worker_id.clone(),
                    reservation_id: TurnReservationId::new(1),
                }),
            },
        );

        handle_worker_response(
            &state,
            &link,
            json!({
                "id": 5,
                "result": {"turn": {"id": "turn-1"}}
            }),
        )
        .unwrap();

        let record = state.directory.record("thread-1").unwrap().unwrap();
        assert_eq!(record.loaded_status, LoadedStatus::Active);
        assert_eq!(record.active_turn_id.as_deref(), Some("turn-1"));
        assert!(record.active_turn_ids.contains("turn-1"));
        let response = serde_json::from_str::<Value>(&try_recv_client_text(&rx)).unwrap();
        assert_eq!(response.get("id"), Some(&json!("client-request")));
    }

    #[test]
    fn rate_limit_state_is_scoped_to_thread_turn() {
        let link = test_link(WorkerId::new("dia4", 1));
        let turn_a_user = json!({
            "method": "event",
            "params": {"threadId": "thread-1", "turnId": "turn-a"},
            "payload": {"type": "user_message"}
        });
        let turn_a_tool = json!({
            "method": "event",
            "params": {"threadId": "thread-1", "turnId": "turn-a"},
            "payload": {"type": "exec_command_begin"}
        });
        let turn_b_user = json!({
            "method": "event",
            "params": {"threadId": "thread-1", "turnId": "turn-b"},
            "payload": {"type": "user_message"}
        });
        let turn_b_limit = json!({
            "method": "event",
            "params": {"threadId": "thread-1", "turnId": "turn-b"},
            "payload": {
                "type": "token_count",
                "info": {"rate_limits": {"primary": {"used_percent": 100.0}}}
            }
        });

        inspect_rate_limit_fragment(&link, &turn_a_user, &turn_a_user.to_string()).unwrap();
        inspect_rate_limit_fragment(&link, &turn_a_tool, &turn_a_tool.to_string()).unwrap();
        inspect_rate_limit_fragment(&link, &turn_b_user, &turn_b_user.to_string()).unwrap();
        let signal =
            inspect_rate_limit_fragment(&link, &turn_b_limit, &turn_b_limit.to_string()).unwrap();

        assert_eq!(
            signal,
            Some(rate_limit::StreamRateLimitSignal {
                safe_to_continue: true
            })
        );
    }
}
