//! Codex app-server worker pool.
//!
//! A worker is a runtime location: slot, generation, process, and listen URL.
//! It is not a thread and it is not the cx service. The broker owns routing;
//! this module owns process lifecycle and capacity metadata.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fmt;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Child;
use std::process::Stdio;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use serde::Serialize;

use crate::paths::ManagerPaths;
use crate::run;
use crate::selector;
use crate::slot;
use crate::target;
use crate::usage::SlotResult;
use crate::usage::SlotStatus;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub(crate) struct WorkerId(String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct TurnReservationId(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum WorkerStatus {
    Ready,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkerRecord {
    pub(crate) worker_id: WorkerId,
    pub(crate) slot: String,
    pub(crate) generation: u64,
    pub(crate) pid: u32,
    pub(crate) listen_url: String,
    pub(crate) readyz_url: String,
    pub(crate) status: WorkerStatus,
    pub(crate) active_turns: usize,
    pub(crate) cooldown_until_unix: Option<u64>,
    pub(crate) started_at_unix: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct WorkerPoolConfig {
    pub(crate) codex_bin: Option<PathBuf>,
    pub(crate) slot: Option<String>,
    pub(crate) target: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct UsageRefreshRequest {
    paths: ManagerPaths,
    slots: Vec<String>,
    timeout: f32,
}

#[derive(Debug)]
pub(crate) struct WorkerPool {
    paths: ManagerPaths,
    config: WorkerPoolConfig,
    workers: BTreeMap<WorkerId, WorkerHandle>,
    slot_workers: BTreeMap<String, WorkerId>,
    candidate_slots: Option<Vec<String>>,
    candidate_slots_refreshed_at: Option<Instant>,
    slot_usage: BTreeMap<String, SlotUsageState>,
    slot_usage_refreshed_at: Option<Instant>,
    slot_retry: BTreeMap<String, SlotRetryState>,
    next_generation: u64,
}

#[derive(Debug)]
struct WorkerHandle {
    record: WorkerRecord,
    child: Child,
    active_turns: BTreeSet<ActiveTurnKey>,
    turn_reservations: BTreeSet<TurnReservationKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ActiveTurnKey {
    thread_id: String,
    turn_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct TurnReservationKey {
    reservation_id: TurnReservationId,
    thread_id: String,
}

#[derive(Debug, Clone)]
struct SlotUsageState {
    selectable: bool,
    observable: bool,
    score: f64,
    index: usize,
}

#[derive(Debug, Clone)]
struct SlotRetryState {
    failures: u32,
    next_retry_at: Instant,
}

const CANDIDATE_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const USAGE_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const WORKER_START_READY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkerLifecycleEvent {
    Started(WorkerRecord),
    Exited {
        worker_id: WorkerId,
        slot: String,
        status: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkerListenUrl {
    host: String,
    port: u16,
}

impl WorkerId {
    pub(crate) fn new(slot: &str, generation: u64) -> Self {
        Self(format!("{slot}:{generation}"))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl TurnReservationId {
    pub(crate) fn new(id: u64) -> Self {
        Self(id)
    }
}

impl fmt::Display for WorkerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl WorkerRecord {
    pub(crate) fn accepts_new_turns(&self) -> bool {
        self.status == WorkerStatus::Ready && !worker_in_cooldown(self)
    }
}

impl WorkerPool {
    pub(crate) fn new(paths: ManagerPaths, config: WorkerPoolConfig) -> Self {
        Self {
            paths,
            config,
            workers: BTreeMap::new(),
            slot_workers: BTreeMap::new(),
            candidate_slots: None,
            candidate_slots_refreshed_at: None,
            slot_usage: BTreeMap::new(),
            slot_usage_refreshed_at: None,
            slot_retry: BTreeMap::new(),
            next_generation: 1,
        }
    }

    pub(crate) fn start_initial(&mut self) -> Result<Vec<WorkerRecord>> {
        let slots = self.candidate_slots()?;
        self.refresh_usage_if_due(true)?;
        if slots.is_empty() {
            anyhow::bail!("no configured slots for worker pool");
        }

        let mut records = Vec::new();
        let mut first_err = None;
        for slot in slots {
            if !self.slot_observable(&slot) {
                continue;
            }
            match self.ensure_slot_worker_for_observation(&slot) {
                Ok(record) => records.push(record),
                Err(err) => {
                    self.record_start_failure(&slot);
                    if self.config.slot.as_deref() == Some(slot.as_str()) {
                        return Err(err);
                    }
                    if first_err.is_none() {
                        first_err = Some(err);
                    }
                }
            }
        }

        if records.is_empty() {
            if let Some(err) = first_err {
                return Err(err).context("start initial worker pool");
            }
            anyhow::bail!("no worker could be started");
        }
        Ok(records)
    }

    pub(crate) fn ensure_slot_worker(&mut self, slot: &str) -> Result<WorkerRecord> {
        if !self.candidate_allows(slot)? {
            anyhow::bail!("slot {slot} is not in the worker pool candidate set");
        }
        if !self.slot_selectable(slot) {
            anyhow::bail!("slot {slot} is not selectable by current usage state");
        }
        if let Some(worker_id) = self.slot_workers.get(slot) {
            if let Some(handle) = self.workers.get(worker_id) {
                if handle.record.status == WorkerStatus::Ready {
                    return Ok(handle.record.clone());
                }
            }
        }
        self.start_worker(slot)
    }

    pub(crate) fn ensure_slot_worker_for_observation(
        &mut self,
        slot: &str,
    ) -> Result<WorkerRecord> {
        if !self.candidate_allows(slot)? {
            anyhow::bail!("slot {slot} is not in the worker pool candidate set");
        }
        if !self.slot_observable(slot) {
            anyhow::bail!("slot {slot} is not observable by current usage state");
        }
        if let Some(worker_id) = self.slot_workers.get(slot) {
            if let Some(handle) = self.workers.get(worker_id) {
                if handle.record.status == WorkerStatus::Ready {
                    return Ok(handle.record.clone());
                }
            }
        }
        self.start_worker(slot)
    }

    pub(crate) fn choose_worker(
        &mut self,
        origin_slot: Option<&str>,
    ) -> Result<Option<WorkerRecord>> {
        let candidate_slots = self.candidate_slots()?;
        let origin_slot = match origin_slot.filter(|slot| *slot != "broker") {
            Some(slot) if self.candidate_allows(slot)? => Some(slot),
            _ => None,
        };
        if let Some(slot) = origin_slot {
            if let Some(worker_id) = self.slot_workers.get(slot) {
                if let Some(handle) = self.workers.get(worker_id) {
                    if handle.record.accepts_new_turns() && self.slot_selectable(slot) {
                        return Ok(Some(handle.record.clone()));
                    }
                }
            }
            if self
                .config
                .slot
                .as_deref()
                .is_none_or(|forced| forced == slot)
            {
                if let Ok(record) = self.ensure_slot_worker(slot) {
                    if record.accepts_new_turns() {
                        return Ok(Some(record));
                    }
                }
            }
        }

        let mut ready = self
            .workers
            .values()
            .filter(|handle| {
                candidate_slots.contains(&handle.record.slot)
                    && handle.record.accepts_new_turns()
                    && self.slot_selectable(&handle.record.slot)
            })
            .map(|handle| handle.record.clone())
            .collect::<Vec<_>>();
        ready.sort_by(|left, right| {
            slot_score(self, right.slot.as_str())
                .partial_cmp(&slot_score(self, left.slot.as_str()))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.active_turns.cmp(&right.active_turns))
                .then_with(|| {
                    slot_index(self, left.slot.as_str()).cmp(&slot_index(self, right.slot.as_str()))
                })
                .then_with(|| left.generation.cmp(&right.generation))
                .then_with(|| left.slot.cmp(&right.slot))
        });
        Ok(ready.into_iter().next())
    }

    pub(crate) fn choose_observation_worker(
        &mut self,
        origin_slot: Option<&str>,
    ) -> Result<Option<WorkerRecord>> {
        let candidate_slots = self.candidate_slots()?;
        let origin_slot = match origin_slot.filter(|slot| *slot != "broker") {
            Some(slot) if self.candidate_allows(slot)? => Some(slot),
            _ => None,
        };
        if let Some(slot) = origin_slot {
            if let Some(worker_id) = self.slot_workers.get(slot) {
                if let Some(handle) = self.workers.get(worker_id) {
                    if handle.record.status == WorkerStatus::Ready && self.slot_observable(slot) {
                        return Ok(Some(handle.record.clone()));
                    }
                }
            }
            if self
                .config
                .slot
                .as_deref()
                .is_none_or(|forced| forced == slot)
            {
                if let Ok(record) = self.ensure_slot_worker_for_observation(slot) {
                    if record.status == WorkerStatus::Ready {
                        return Ok(Some(record));
                    }
                }
            }
        }

        let mut ready = self
            .workers
            .values()
            .filter(|handle| {
                candidate_slots.contains(&handle.record.slot)
                    && handle.record.status == WorkerStatus::Ready
                    && self.slot_observable(&handle.record.slot)
            })
            .map(|handle| handle.record.clone())
            .collect::<Vec<_>>();
        ready.sort_by(|left, right| {
            slot_score(self, right.slot.as_str())
                .partial_cmp(&slot_score(self, left.slot.as_str()))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.active_turns.cmp(&right.active_turns))
                .then_with(|| {
                    slot_index(self, left.slot.as_str()).cmp(&slot_index(self, right.slot.as_str()))
                })
                .then_with(|| left.generation.cmp(&right.generation))
                .then_with(|| left.slot.cmp(&right.slot))
        });
        Ok(ready.into_iter().next())
    }

    pub(crate) fn worker_accepts_new_turns(
        &mut self,
        worker_id: &WorkerId,
    ) -> Result<Option<WorkerRecord>> {
        let candidate_slots = self.candidate_slots()?;
        let Some(handle) = self.workers.get(worker_id) else {
            return Ok(None);
        };
        if candidate_slots.contains(&handle.record.slot)
            && handle.record.accepts_new_turns()
            && self.slot_selectable(&handle.record.slot)
        {
            return Ok(Some(handle.record.clone()));
        }
        Ok(None)
    }

    pub(crate) fn worker(&self, worker_id: &WorkerId) -> Option<WorkerRecord> {
        self.workers
            .get(worker_id)
            .map(|handle| handle.record.clone())
    }

    pub(crate) fn records(&self) -> Vec<WorkerRecord> {
        self.workers
            .values()
            .map(|handle| handle.record.clone())
            .collect()
    }

    pub(crate) fn observation_worker_ids(&self) -> Vec<WorkerId> {
        self.workers
            .values()
            .filter(|handle| {
                handle.record.status == WorkerStatus::Ready
                    && self.slot_observable(&handle.record.slot)
            })
            .map(|handle| handle.record.worker_id.clone())
            .collect()
    }

    pub(crate) fn new_turn_worker_ids(&mut self) -> Result<Vec<WorkerId>> {
        let candidate_slots = self.candidate_slots()?;
        Ok(self
            .workers
            .values()
            .filter(|handle| {
                candidate_slots.contains(&handle.record.slot)
                    && handle.record.accepts_new_turns()
                    && self.slot_selectable(&handle.record.slot)
            })
            .map(|handle| handle.record.worker_id.clone())
            .collect())
    }

    pub(crate) fn shutdown(&mut self) {
        for handle in self.workers.values_mut() {
            let _ = handle.child.kill();
        }
        for handle in self.workers.values_mut() {
            let _ = handle.child.wait();
            handle.record.status = WorkerStatus::Stopped;
        }
        self.workers.clear();
        self.slot_workers.clear();
    }

    pub(crate) fn mark_turn_started(
        &mut self,
        worker_id: &WorkerId,
        thread_id: &str,
        turn_id: Option<&str>,
    ) {
        if let Some(handle) = self.workers.get_mut(worker_id) {
            handle.active_turns.insert(ActiveTurnKey {
                thread_id: thread_id.to_string(),
                turn_id: turn_id.unwrap_or_default().to_string(),
            });
            refresh_worker_load(handle);
        }
    }

    pub(crate) fn mark_turn_completed(
        &mut self,
        worker_id: &WorkerId,
        thread_id: &str,
        turn_id: Option<&str>,
    ) {
        if let Some(handle) = self.workers.get_mut(worker_id) {
            if let Some(turn_id) = turn_id {
                handle.active_turns.remove(&ActiveTurnKey {
                    thread_id: thread_id.to_string(),
                    turn_id: turn_id.to_string(),
                });
            } else {
                handle.active_turns.retain(|key| key.thread_id != thread_id);
            }
            refresh_worker_load(handle);
        }
    }

    pub(crate) fn reserve_turn_start(
        &mut self,
        worker_id: &WorkerId,
        thread_id: &str,
        reservation_id: TurnReservationId,
    ) -> bool {
        let Some(handle) = self.workers.get_mut(worker_id) else {
            return false;
        };
        let inserted = handle.turn_reservations.insert(TurnReservationKey {
            reservation_id,
            thread_id: thread_id.to_string(),
        });
        refresh_worker_load(handle);
        inserted
    }

    pub(crate) fn release_turn_reservation(
        &mut self,
        worker_id: &WorkerId,
        reservation_id: TurnReservationId,
    ) -> bool {
        let Some(handle) = self.workers.get_mut(worker_id) else {
            return false;
        };
        let before = handle.turn_reservations.len();
        handle
            .turn_reservations
            .retain(|reservation| reservation.reservation_id != reservation_id);
        refresh_worker_load(handle);
        before != handle.turn_reservations.len()
    }

    pub(crate) fn clear_turn_reservations(&mut self, worker_id: &WorkerId) -> usize {
        let Some(handle) = self.workers.get_mut(worker_id) else {
            return 0;
        };
        let cleared = handle.turn_reservations.len();
        handle.turn_reservations.clear();
        refresh_worker_load(handle);
        cleared
    }

    pub(crate) fn mark_cooldown(&mut self, worker_id: &WorkerId, duration: Duration) {
        if let Some(handle) = self.workers.get_mut(worker_id) {
            handle.record.cooldown_until_unix = Some(
                unix_now_secs()
                    .saturating_add(duration.as_secs())
                    .max(unix_now_secs()),
            );
        }
    }

    pub(crate) fn poll_children(&mut self) -> Result<Vec<WorkerLifecycleEvent>> {
        let candidate_slots = self.candidate_slots()?;
        let mut events = Vec::new();
        let worker_ids = self.workers.keys().cloned().collect::<Vec<_>>();
        let mut restart_slots = Vec::new();
        let mut stopped_workers = Vec::new();
        for worker_id in worker_ids {
            let Some(handle) = self.workers.get_mut(&worker_id) else {
                continue;
            };
            if let Some(status) = handle
                .child
                .try_wait()
                .with_context(|| format!("poll worker {}", worker_id.as_str()))?
            {
                handle.record.status = WorkerStatus::Stopped;
                let slot = handle.record.slot.clone();
                self.slot_workers.remove(&slot);
                restart_slots.push(slot.clone());
                stopped_workers.push(worker_id.clone());
                events.push(WorkerLifecycleEvent::Exited {
                    worker_id,
                    slot,
                    status: status.to_string(),
                });
            }
        }
        for worker_id in stopped_workers {
            self.workers.remove(&worker_id);
        }
        let mut attempted_slots = BTreeSet::new();
        for slot in restart_slots {
            if !candidate_slots.contains(&slot) || !self.slot_observable(&slot) {
                continue;
            }
            if !self.slot_retry_ready(&slot, Instant::now()) {
                continue;
            }
            attempted_slots.insert(slot.clone());
            match self.start_worker(&slot) {
                Ok(record) => {
                    self.record_start_success(&slot);
                    events.push(WorkerLifecycleEvent::Started(record));
                }
                Err(err) => {
                    self.record_start_failure(&slot);
                    eprintln!("failed to restart worker for slot {slot}: {err:#}");
                }
            }
        }
        events.extend(self.retire_non_candidate_idle_workers(&candidate_slots));
        if let Some(slot) = self.missing_candidate_slots()?.into_iter().next() {
            if attempted_slots.contains(&slot) {
                return Ok(events);
            }
            match self.start_worker(slot.as_str()) {
                Ok(record) => {
                    self.record_start_success(&slot);
                    events.push(WorkerLifecycleEvent::Started(record));
                }
                Err(err) => {
                    self.record_start_failure(&slot);
                    eprintln!("failed to start missing worker for slot {slot}: {err:#}");
                }
            }
        }
        Ok(events)
    }

    fn start_worker(&mut self, slot: &str) -> Result<WorkerRecord> {
        let real_codex = run::resolve_codex_bin(self.config.codex_bin.as_deref())?;
        let target = target::load_optional_target(&self.paths, self.config.target.as_deref())?;
        let upstream = WorkerListenUrl {
            host: String::from("127.0.0.1"),
            port: 0,
        }
        .resolve()?;
        let upstream_listen_url = upstream.websocket_url();
        let codex_args = vec![
            OsString::from("app-server"),
            OsString::from("--listen"),
            OsString::from(upstream_listen_url.clone()),
        ];
        let spec = run::build_slot_command_spec(
            &self.paths,
            real_codex,
            slot,
            target.as_ref(),
            codex_args,
        )?;
        let mut command = spec.into_command();
        command
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        let generation = self.next_generation;
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .context("worker generation overflow")?;
        let worker_id = WorkerId::new(slot, generation);
        let mut child = command
            .spawn()
            .with_context(|| format!("spawn app-server worker {worker_id}"))?;
        if let Err(err) = wait_for_worker_ready(
            &upstream.readyz_url(),
            WORKER_START_READY_TIMEOUT,
            &mut child,
        ) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(err).with_context(|| format!("start app-server worker {worker_id}"));
        }

        let record = WorkerRecord {
            worker_id: worker_id.clone(),
            slot: slot.to_string(),
            generation,
            pid: child.id(),
            listen_url: upstream_listen_url,
            readyz_url: upstream.readyz_url(),
            status: WorkerStatus::Ready,
            active_turns: 0,
            cooldown_until_unix: None,
            started_at_unix: unix_now_secs(),
        };
        self.slot_workers
            .insert(slot.to_string(), worker_id.clone());
        self.workers.insert(
            worker_id,
            WorkerHandle {
                record: record.clone(),
                child,
                active_turns: BTreeSet::new(),
                turn_reservations: BTreeSet::new(),
            },
        );
        Ok(record)
    }

    fn candidate_slots(&mut self) -> Result<Vec<String>> {
        let stale = self
            .candidate_slots_refreshed_at
            .is_none_or(|refreshed| refreshed.elapsed() >= CANDIDATE_REFRESH_INTERVAL);
        if !stale {
            if let Some(slots) = self.candidate_slots.as_ref() {
                return Ok(slots.clone());
            }
        }
        let slots = self.compute_candidate_slots()?;
        let changed = self.candidate_slots.as_ref() != Some(&slots);
        self.candidate_slots = Some(slots.clone());
        self.candidate_slots_refreshed_at = Some(Instant::now());
        if changed {
            self.slot_usage_refreshed_at = None;
        }
        self.slot_usage.retain(|slot, _| slots.contains(slot));
        self.slot_retry.retain(|slot, _| slots.contains(slot));
        Ok(slots)
    }

    fn refresh_usage_if_due(&mut self, force: bool) -> Result<()> {
        if let Some(request) = self.usage_refresh_request(force)? {
            let results = request.query()?;
            self.apply_usage_results(results);
        }
        Ok(())
    }

    pub(crate) fn usage_refresh_request(
        &mut self,
        force: bool,
    ) -> Result<Option<UsageRefreshRequest>> {
        let stale = force
            || self
                .slot_usage_refreshed_at
                .is_none_or(|refreshed| refreshed.elapsed() >= USAGE_REFRESH_INTERVAL);
        if !stale {
            return Ok(None);
        }
        let slots = self.candidate_slots()?;
        self.slot_usage_refreshed_at = Some(Instant::now());
        if slots.is_empty() {
            self.slot_usage.clear();
            return Ok(None);
        }
        Ok(Some(UsageRefreshRequest {
            paths: self.paths.clone(),
            slots,
            timeout: run::usage_timeout(),
        }))
    }

    pub(crate) fn apply_usage_results(&mut self, results: Vec<SlotResult>) {
        self.slot_usage = results
            .into_iter()
            .map(|result| (result.slot.clone(), SlotUsageState::from_result(result)))
            .collect();
        self.slot_usage_refreshed_at = Some(Instant::now());
    }

    fn candidate_allows(&mut self, slot: &str) -> Result<bool> {
        Ok(self
            .candidate_slots()?
            .iter()
            .any(|candidate| candidate == slot))
    }

    fn missing_candidate_slots(&mut self) -> Result<Vec<String>> {
        let now = Instant::now();
        Ok(self
            .candidate_slots()?
            .into_iter()
            .filter(|slot| !self.slot_workers.contains_key(slot))
            .filter(|slot| self.slot_observable(slot))
            .filter(|slot| self.slot_retry_ready(slot, now))
            .collect())
    }

    fn slot_selectable(&self, slot: &str) -> bool {
        if let Some(state) = self.slot_usage.get(slot) {
            return state.selectable;
        }
        self.slot_usage_refreshed_at.is_none()
    }

    fn slot_observable(&self, slot: &str) -> bool {
        if let Some(state) = self.slot_usage.get(slot) {
            return state.observable;
        }
        self.slot_usage_refreshed_at.is_none()
    }

    fn record_start_success(&mut self, slot: &str) {
        self.slot_retry.remove(slot);
    }

    fn record_start_failure(&mut self, slot: &str) {
        let retry = self
            .slot_retry
            .entry(slot.to_string())
            .or_insert(SlotRetryState {
                failures: 0,
                next_retry_at: Instant::now(),
            });
        retry.failures = retry.failures.saturating_add(1);
        let delay = retry_delay(retry.failures);
        retry.next_retry_at = Instant::now() + delay;
    }

    fn slot_retry_ready(&self, slot: &str, now: Instant) -> bool {
        self.slot_retry
            .get(slot)
            .is_none_or(|retry| retry.next_retry_at <= now)
    }

    fn retire_non_candidate_idle_workers(
        &mut self,
        candidate_slots: &[String],
    ) -> Vec<WorkerLifecycleEvent> {
        let retired = self
            .workers
            .iter()
            .filter(|(_, handle)| {
                !candidate_slots.contains(&handle.record.slot)
                    && handle.active_turns.is_empty()
                    && handle.turn_reservations.is_empty()
            })
            .map(|(worker_id, _)| worker_id.clone())
            .collect::<Vec<_>>();
        let mut events = Vec::new();
        for worker_id in retired {
            let Some(mut handle) = self.workers.remove(&worker_id) else {
                continue;
            };
            self.slot_workers.remove(&handle.record.slot);
            let _ = handle.child.kill();
            let status = handle
                .child
                .wait()
                .map(|status| status.to_string())
                .unwrap_or_else(|err| format!("retired: {err}"));
            events.push(WorkerLifecycleEvent::Exited {
                worker_id,
                slot: handle.record.slot,
                status,
            });
        }
        events
    }

    fn compute_candidate_slots(&self) -> Result<Vec<String>> {
        if let Some(slot) = self.config.slot.as_ref() {
            return Ok(vec![slot.clone()]);
        }
        if let Some(target_name) = self.config.target.as_deref() {
            return target::load_target(&self.paths, target_name)?.slots_or_rotation(&self.paths);
        }
        slot::load_rotation(&self.paths)
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl SlotUsageState {
    fn from_result(result: SlotResult) -> Self {
        Self {
            selectable: result.is_available() || result.is_transient(),
            observable: result.status != SlotStatus::Missing,
            score: result.score,
            index: result.index,
        }
    }
}

impl UsageRefreshRequest {
    pub(crate) fn query(self) -> Result<Vec<SlotResult>> {
        selector::query_slots(&self.paths, &self.slots, self.timeout)
    }
}

fn slot_score(pool: &WorkerPool, slot: &str) -> f64 {
    pool.slot_usage
        .get(slot)
        .map(|state| state.score)
        .unwrap_or(0.0)
}

fn slot_index(pool: &WorkerPool, slot: &str) -> usize {
    pool.slot_usage
        .get(slot)
        .map(|state| state.index)
        .unwrap_or(usize::MAX)
}

fn retry_delay(failures: u32) -> Duration {
    let exponent = failures.saturating_sub(1).min(5);
    Duration::from_secs(2_u64.saturating_pow(exponent))
}

fn refresh_worker_load(handle: &mut WorkerHandle) {
    handle.record.active_turns = handle
        .active_turns
        .len()
        .saturating_add(handle.turn_reservations.len());
}

impl WorkerListenUrl {
    fn resolve(self) -> Result<Self> {
        if self.port != 0 {
            return Ok(self);
        }
        let listener = TcpListener::bind(("127.0.0.1", 0)).context("reserve worker port")?;
        let port = listener.local_addr().context("read worker port")?.port();
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

fn wait_for_worker_ready(readyz_url: &str, timeout: Duration, child: &mut Child) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .context("build worker readyz client")?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().context("poll app-server worker")? {
            anyhow::bail!("app-server worker exited before ready with {status}");
        }
        if let Ok(response) = client.get(readyz_url).send() {
            if response.status().is_success() {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for app-server worker ready endpoint");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn worker_in_cooldown(record: &WorkerRecord) -> bool {
    record
        .cooldown_until_unix
        .is_some_and(|cooldown| cooldown > unix_now_secs())
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn temp_paths(name: &str) -> ManagerPaths {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "cx-worker-pool-test-{name}-{}-{unique}",
            std::process::id()
        ));
        ManagerPaths::from_roots(root.join("codex"), root.join("profile-manager"))
    }

    fn make_api_key_slot(paths: &ManagerPaths, slot: &str) {
        fs::create_dir_all(paths.slot_home(slot)).unwrap();
        fs::write(
            paths.slot_home(slot).join("auth.json"),
            r#"{"OPENAI_API_KEY":"test-key"}"#,
        )
        .unwrap();
    }

    #[test]
    fn origin_slot_outside_target_does_not_extend_candidates() {
        let paths = temp_paths("origin-outside-target");
        fs::create_dir_all(&paths.targets_dir).unwrap();
        fs::write(paths.target_file("research"), "slots = [\"dia4\"]\n").unwrap();
        let mut pool = WorkerPool::new(
            paths.clone(),
            WorkerPoolConfig {
                codex_bin: None,
                slot: None,
                target: Some(String::from("research")),
            },
        );

        let selected = pool.choose_worker(Some("outside")).unwrap();
        let err = pool.ensure_slot_worker("outside").unwrap_err();

        assert!(selected.is_none());
        assert!(pool.records().is_empty());
        assert!(format!("{err:#}").contains("candidate set"));

        let _ = fs::remove_dir_all(paths.manager_dir);
    }

    #[test]
    fn missing_candidate_slots_remain_desired_after_partial_start_failure() {
        let paths = temp_paths("missing-candidates");
        fs::create_dir_all(&paths.targets_dir).unwrap();
        fs::write(
            paths.target_file("research"),
            "slots = [\"dia4\", \"dia5\"]\n",
        )
        .unwrap();
        make_api_key_slot(&paths, "dia4");
        make_api_key_slot(&paths, "dia5");
        let mut pool = WorkerPool::new(
            paths.clone(),
            WorkerPoolConfig {
                codex_bin: None,
                slot: None,
                target: Some(String::from("research")),
            },
        );

        assert_eq!(
            pool.missing_candidate_slots().unwrap(),
            vec![String::from("dia4"), String::from("dia5")]
        );

        let _ = fs::remove_dir_all(paths.manager_dir);
    }

    #[test]
    fn candidate_refresh_does_not_drop_live_slot_mapping() {
        let paths = temp_paths("candidate-refresh-keeps-live-worker");
        fs::create_dir_all(&paths.targets_dir).unwrap();
        fs::write(
            paths.target_file("research"),
            "slots = [\"dia4\", \"dia5\"]\n",
        )
        .unwrap();
        let mut pool = WorkerPool::new(
            paths.clone(),
            WorkerPoolConfig {
                codex_bin: None,
                slot: None,
                target: Some(String::from("research")),
            },
        );

        assert_eq!(
            pool.candidate_slots().unwrap(),
            vec![String::from("dia4"), String::from("dia5")]
        );
        pool.slot_workers
            .insert(String::from("dia5"), WorkerId::new("dia5", 1));
        fs::write(paths.target_file("research"), "slots = [\"dia4\"]\n").unwrap();
        pool.candidate_slots_refreshed_at =
            Some(Instant::now() - CANDIDATE_REFRESH_INTERVAL - Duration::from_secs(1));

        assert_eq!(pool.candidate_slots().unwrap(), vec![String::from("dia4")]);
        assert!(pool.slot_workers.contains_key("dia5"));

        let _ = fs::remove_dir_all(paths.manager_dir);
    }

    #[test]
    fn slot_usage_state_filters_exhausted_slots() {
        let exhausted = SlotUsageState::from_result(SlotResult::new(
            "dia4",
            0,
            crate::usage::SlotStatus::Exhausted,
            -1.0,
            "done",
        ));
        let transient = SlotUsageState::from_result(SlotResult::new(
            "dia5",
            1,
            crate::usage::SlotStatus::Error,
            -1.0,
            "network",
        ));

        assert!(!exhausted.selectable);
        assert!(exhausted.observable);
        assert!(transient.selectable);
        assert!(transient.observable);
    }

    #[test]
    fn turn_reservations_count_as_worker_load_until_released() {
        let mut handle = WorkerHandle {
            record: WorkerRecord {
                worker_id: WorkerId::new("dia4", 1),
                slot: String::from("dia4"),
                generation: 1,
                pid: 123,
                listen_url: String::from("ws://127.0.0.1:1"),
                readyz_url: String::from("http://127.0.0.1:1/readyz"),
                status: WorkerStatus::Ready,
                active_turns: 0,
                cooldown_until_unix: None,
                started_at_unix: 1,
            },
            child: std::process::Command::new("true").spawn().unwrap(),
            active_turns: BTreeSet::new(),
            turn_reservations: BTreeSet::new(),
        };

        handle.turn_reservations.insert(TurnReservationKey {
            reservation_id: TurnReservationId::new(1),
            thread_id: String::from("thread-1"),
        });
        refresh_worker_load(&mut handle);
        assert_eq!(handle.record.active_turns, 1);

        handle.active_turns.insert(ActiveTurnKey {
            thread_id: String::from("thread-1"),
            turn_id: String::from("turn-1"),
        });
        refresh_worker_load(&mut handle);
        assert_eq!(handle.record.active_turns, 2);

        handle
            .turn_reservations
            .retain(|reservation| reservation.reservation_id != TurnReservationId::new(1));
        refresh_worker_load(&mut handle);
        assert_eq!(handle.record.active_turns, 1);

        handle.turn_reservations.insert(TurnReservationKey {
            reservation_id: TurnReservationId::new(2),
            thread_id: String::from("thread-2"),
        });
        refresh_worker_load(&mut handle);
        assert_eq!(handle.record.active_turns, 2);
        handle.turn_reservations.clear();
        refresh_worker_load(&mut handle);
        assert_eq!(handle.record.active_turns, 1);
    }

    #[test]
    fn exhausted_candidate_slot_stays_observable_for_existing_threads() {
        let paths = temp_paths("exhausted-observable");
        fs::create_dir_all(&paths.targets_dir).unwrap();
        fs::write(paths.target_file("research"), "slots = [\"dia4\"]\n").unwrap();
        let mut pool = WorkerPool::new(
            paths.clone(),
            WorkerPoolConfig {
                codex_bin: None,
                slot: None,
                target: Some(String::from("research")),
            },
        );
        pool.slot_usage.insert(
            String::from("dia4"),
            SlotUsageState::from_result(SlotResult::new(
                "dia4",
                0,
                crate::usage::SlotStatus::Exhausted,
                -1.0,
                "limit reached",
            )),
        );
        pool.slot_usage_refreshed_at = Some(Instant::now());

        assert!(!pool.slot_selectable("dia4"));
        assert!(pool.slot_observable("dia4"));
        assert_eq!(pool.missing_candidate_slots().unwrap(), vec!["dia4"]);

        let _ = fs::remove_dir_all(paths.manager_dir);
    }

    #[test]
    fn retry_delay_uses_capped_exponential_backoff() {
        assert_eq!(retry_delay(1), Duration::from_secs(1));
        assert_eq!(retry_delay(2), Duration::from_secs(2));
        assert_eq!(retry_delay(10), Duration::from_secs(32));
    }
}
