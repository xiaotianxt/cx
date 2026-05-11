//! First-writer-wins approval routing for brokered workers.
//!
//! Workers still speak the Codex app-server request/response shape. The broker
//! rewrites worker approval request ids into global ids, fans the request out to
//! subscribers, and forwards only the first valid client response back.

use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use serde_json::Value;

use crate::worker_pool::WorkerId;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ApprovalRequestRecord {
    pub(crate) broker_approval_id: String,
    pub(crate) worker_id: WorkerId,
    pub(crate) worker_request_id: Value,
    pub(crate) thread_id: Option<String>,
    pub(crate) request: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ApprovalResponseRoute {
    Forward {
        broker_approval_id: String,
        worker_id: WorkerId,
        worker_request_id: Value,
        response: Value,
    },
    AlreadyHandled,
    Unknown,
}

#[derive(Debug, Default)]
pub(crate) struct ApprovalBroker {
    next_id: AtomicU64,
    state: Mutex<ApprovalState>,
}

#[derive(Debug, Clone, PartialEq)]
struct PendingApproval {
    worker_id: WorkerId,
    worker_request_id: Value,
    thread_id: Option<String>,
    request: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CancelledApproval {
    pub(crate) worker_id: WorkerId,
    pub(crate) worker_request_id: Value,
}

#[derive(Debug, Default)]
struct ApprovalState {
    pending: BTreeMap<String, PendingApproval>,
    claimed: BTreeMap<String, PendingApproval>,
    handled: BTreeMap<String, u64>,
}

const HANDLED_APPROVAL_TTL_SECS: u64 = 5 * 60;
const HANDLED_APPROVAL_CACHE_LIMIT: usize = 1024;

impl ApprovalBroker {
    pub(crate) fn register(
        &self,
        worker_id: WorkerId,
        worker_request_id: Value,
        thread_id: Option<String>,
        mut request: Value,
    ) -> Result<ApprovalRequestRecord> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let broker_approval_id = format!("cx_approval_{id}");
        request["id"] = Value::String(broker_approval_id.clone());
        let pending = PendingApproval {
            worker_id: worker_id.clone(),
            worker_request_id: worker_request_id.clone(),
            thread_id: thread_id.clone(),
            request: request.clone(),
        };
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("approval broker lock poisoned"))?;
        prune_handled_approvals(&mut state, unix_now_secs());
        state.pending.insert(broker_approval_id.clone(), pending);
        Ok(ApprovalRequestRecord {
            broker_approval_id,
            worker_id,
            worker_request_id,
            thread_id,
            request,
        })
    }

    pub(crate) fn pending_requests_for_thread(&self, thread_id: &str) -> Result<Vec<Value>> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("approval broker lock poisoned"))?;
        Ok(state
            .pending
            .values()
            .filter(|approval| approval.thread_id.as_deref() == Some(thread_id))
            .map(|approval| approval.request.clone())
            .collect())
    }

    pub(crate) fn resolve_response(
        &self,
        broker_approval_id: &str,
        response: Value,
    ) -> Result<ApprovalResponseRoute> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("approval broker lock poisoned"))?;
        let now = unix_now_secs();
        prune_handled_approvals(&mut state, now);
        if state.handled.contains_key(broker_approval_id)
            || state.claimed.contains_key(broker_approval_id)
        {
            return Ok(ApprovalResponseRoute::AlreadyHandled);
        }
        let Some(record) = state.pending.remove(broker_approval_id) else {
            return Ok(ApprovalResponseRoute::Unknown);
        };
        state
            .claimed
            .insert(broker_approval_id.to_string(), record.clone());
        Ok(ApprovalResponseRoute::Forward {
            broker_approval_id: broker_approval_id.to_string(),
            worker_id: record.worker_id.clone(),
            worker_request_id: record.worker_request_id.clone(),
            response,
        })
    }

    pub(crate) fn commit_response(&self, broker_approval_id: &str) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("approval broker lock poisoned"))?;
        state.claimed.remove(broker_approval_id);
        state
            .handled
            .insert(broker_approval_id.to_string(), unix_now_secs());
        prune_handled_approvals(&mut state, unix_now_secs());
        Ok(())
    }

    pub(crate) fn restore_response(&self, broker_approval_id: &str) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("approval broker lock poisoned"))?;
        if state.handled.contains_key(broker_approval_id) {
            return Ok(());
        }
        if let Some(record) = state.claimed.remove(broker_approval_id) {
            state.pending.insert(broker_approval_id.to_string(), record);
        }
        Ok(())
    }

    pub(crate) fn cancel(&self, broker_approval_id: &str) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("approval broker lock poisoned"))?;
        state.pending.remove(broker_approval_id);
        state.claimed.remove(broker_approval_id);
        state.handled.remove(broker_approval_id);
        Ok(())
    }

    pub(crate) fn cancel_worker(&self, worker_id: &WorkerId) -> Result<usize> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("approval broker lock poisoned"))?;
        let pending_before = state.pending.len();
        let claimed_before = state.claimed.len();
        state
            .pending
            .retain(|_, approval| &approval.worker_id != worker_id);
        state
            .claimed
            .retain(|_, approval| &approval.worker_id != worker_id);
        Ok(pending_before.saturating_sub(state.pending.len())
            + claimed_before.saturating_sub(state.claimed.len()))
    }

    pub(crate) fn cancel_thread(&self, thread_id: &str) -> Result<Vec<CancelledApproval>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("approval broker lock poisoned"))?;
        let mut cancelled = Vec::new();
        let pending_ids = state
            .pending
            .iter()
            .filter(|(_, approval)| approval.thread_id.as_deref() == Some(thread_id))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in pending_ids {
            if let Some(approval) = state.pending.remove(&id) {
                cancelled.push(CancelledApproval {
                    worker_id: approval.worker_id,
                    worker_request_id: approval.worker_request_id,
                });
            }
        }
        let claimed_ids = state
            .claimed
            .iter()
            .filter(|(_, approval)| approval.thread_id.as_deref() == Some(thread_id))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in claimed_ids {
            if let Some(approval) = state.claimed.remove(&id) {
                cancelled.push(CancelledApproval {
                    worker_id: approval.worker_id,
                    worker_request_id: approval.worker_request_id,
                });
            }
        }
        Ok(cancelled)
    }
}

fn prune_handled_approvals(state: &mut ApprovalState, now: u64) {
    state
        .handled
        .retain(|_, handled_at| now.saturating_sub(*handled_at) <= HANDLED_APPROVAL_TTL_SECS);
    while state.handled.len() > HANDLED_APPROVAL_CACHE_LIMIT {
        let Some(oldest_id) = state
            .handled
            .iter()
            .min_by_key(|(_, handled_at)| *handled_at)
            .map(|(id, _)| id.clone())
        else {
            break;
        };
        state.handled.remove(&oldest_id);
    }
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

pub(crate) fn is_approval_request_method(method: &str) -> bool {
    matches!(
        method,
        "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/permissions/requestApproval"
            | "execCommandApproval"
            | "applyPatchApproval"
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn approval_request(worker_request_id: Value, thread_id: Option<&str>) -> Value {
        let mut params = serde_json::Map::new();
        if let Some(thread_id) = thread_id {
            params.insert("threadId".to_string(), Value::String(thread_id.to_string()));
        }
        json!({
            "id": worker_request_id,
            "method": "item/commandExecution/requestApproval",
            "params": Value::Object(params),
        })
    }

    fn register_approval(
        broker: &ApprovalBroker,
        worker_id: WorkerId,
        worker_request_id: Value,
        thread_id: Option<&str>,
    ) -> ApprovalRequestRecord {
        broker
            .register(
                worker_id,
                worker_request_id.clone(),
                thread_id.map(str::to_string),
                approval_request(worker_request_id, thread_id),
            )
            .unwrap()
    }

    #[test]
    fn first_response_wins() {
        let broker = ApprovalBroker::default();
        let record = register_approval(
            &broker,
            WorkerId::new("dia4", 1),
            json!(7),
            Some("thread-1"),
        );

        let first = broker
            .resolve_response(&record.broker_approval_id, json!({"result": "ok"}))
            .unwrap();
        let second = broker
            .resolve_response(&record.broker_approval_id, json!({"result": "late"}))
            .unwrap();
        broker.commit_response(&record.broker_approval_id).unwrap();
        let third = broker
            .resolve_response(&record.broker_approval_id, json!({"result": "later"}))
            .unwrap();

        assert!(matches!(first, ApprovalResponseRoute::Forward { .. }));
        assert_eq!(second, ApprovalResponseRoute::AlreadyHandled);
        assert_eq!(third, ApprovalResponseRoute::AlreadyHandled);
    }

    #[test]
    fn restore_response_allows_retry_after_worker_send_failure() {
        let broker = ApprovalBroker::default();
        let record = register_approval(
            &broker,
            WorkerId::new("dia4", 1),
            json!(7),
            Some("thread-1"),
        );

        let first = broker
            .resolve_response(&record.broker_approval_id, json!({"result": "ok"}))
            .unwrap();
        broker.restore_response(&record.broker_approval_id).unwrap();
        let retry = broker
            .resolve_response(&record.broker_approval_id, json!({"result": "retry"}))
            .unwrap();

        assert!(matches!(first, ApprovalResponseRoute::Forward { .. }));
        assert!(matches!(retry, ApprovalResponseRoute::Forward { .. }));
    }

    #[test]
    fn cancel_removes_unrouted_approval() {
        let broker = ApprovalBroker::default();
        let record = register_approval(
            &broker,
            WorkerId::new("dia4", 1),
            json!(7),
            Some("thread-1"),
        );

        broker.cancel(&record.broker_approval_id).unwrap();
        let route = broker
            .resolve_response(&record.broker_approval_id, json!({"result": "late"}))
            .unwrap();

        assert_eq!(route, ApprovalResponseRoute::Unknown);
    }

    #[test]
    fn cancel_worker_removes_pending_and_claimed_approvals() {
        let broker = ApprovalBroker::default();
        let worker = WorkerId::new("dia4", 1);
        let other_worker = WorkerId::new("dia5", 1);
        let pending = register_approval(&broker, worker.clone(), json!(1), Some("thread-1"));
        let claimed = register_approval(&broker, worker.clone(), json!(2), Some("thread-2"));
        let other = register_approval(&broker, other_worker, json!(3), Some("thread-3"));

        assert!(matches!(
            broker
                .resolve_response(&claimed.broker_approval_id, json!({"result": "ok"}))
                .unwrap(),
            ApprovalResponseRoute::Forward { .. }
        ));

        assert_eq!(broker.cancel_worker(&worker).unwrap(), 2);
        assert_eq!(
            broker
                .resolve_response(&pending.broker_approval_id, json!({}))
                .unwrap(),
            ApprovalResponseRoute::Unknown
        );
        assert_eq!(
            broker
                .resolve_response(&claimed.broker_approval_id, json!({}))
                .unwrap(),
            ApprovalResponseRoute::Unknown
        );
        assert!(matches!(
            broker
                .resolve_response(&other.broker_approval_id, json!({}))
                .unwrap(),
            ApprovalResponseRoute::Forward { .. }
        ));
    }

    #[test]
    fn cancel_thread_removes_pending_and_claimed_approvals_for_that_thread() {
        let broker = ApprovalBroker::default();
        let worker = WorkerId::new("dia4", 1);
        let pending = register_approval(&broker, worker.clone(), json!(1), Some("thread-1"));
        let claimed = register_approval(&broker, worker.clone(), json!(2), Some("thread-1"));
        let other = register_approval(&broker, worker, json!(3), Some("thread-2"));

        assert!(matches!(
            broker
                .resolve_response(&claimed.broker_approval_id, json!({"result": "ok"}))
                .unwrap(),
            ApprovalResponseRoute::Forward { .. }
        ));

        let cancelled = broker.cancel_thread("thread-1").unwrap();

        assert_eq!(cancelled.len(), 2);
        assert_eq!(
            broker
                .resolve_response(&pending.broker_approval_id, json!({}))
                .unwrap(),
            ApprovalResponseRoute::Unknown
        );
        assert_eq!(
            broker
                .resolve_response(&claimed.broker_approval_id, json!({}))
                .unwrap(),
            ApprovalResponseRoute::Unknown
        );
        assert!(matches!(
            broker
                .resolve_response(&other.broker_approval_id, json!({}))
                .unwrap(),
            ApprovalResponseRoute::Forward { .. }
        ));
    }
}
