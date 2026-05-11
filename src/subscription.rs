//! Client subscriptions for brokered Codex thread events.
//!
//! A subscription is local broker state. It does not grant ownership of a
//! thread and it does not imply a slot choice; it only says which connected
//! clients should receive live notifications for a thread.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::mpsc;
use std::sync::Mutex;

use anyhow::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ClientId(u64);

#[derive(Debug, Clone)]
pub(crate) struct ClientSink {
    sender: mpsc::Sender<ClientOutbound>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClientOutbound {
    Text(String),
    Pong(Vec<u8>),
}

#[derive(Debug, Default)]
pub(crate) struct SubscriptionHub {
    inner: Mutex<SubscriptionState>,
}

#[derive(Debug, Default)]
struct SubscriptionState {
    clients: BTreeMap<ClientId, ClientSink>,
    threads: BTreeMap<String, BTreeSet<ClientId>>,
    approval_ready: BTreeMap<String, BTreeSet<ClientId>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FanOutResult {
    pub(crate) subscribers: usize,
    pub(crate) ready: usize,
    pub(crate) sent: usize,
}

impl ClientId {
    pub(crate) fn new(raw: u64) -> Self {
        Self(raw)
    }
}

impl ClientSink {
    pub(crate) fn new(sender: mpsc::Sender<ClientOutbound>) -> Self {
        Self { sender }
    }
}

impl SubscriptionHub {
    pub(crate) fn register_client(&self, client_id: ClientId, sink: ClientSink) -> Result<()> {
        let mut state = self.lock()?;
        state.clients.insert(client_id, sink);
        Ok(())
    }

    pub(crate) fn unregister_client(&self, client_id: ClientId) -> Result<Vec<String>> {
        let mut state = self.lock()?;
        state.clients.remove(&client_id);
        let mut empty_threads = Vec::new();
        for subscribers in state.threads.values_mut() {
            subscribers.remove(&client_id);
        }
        for ready in state.approval_ready.values_mut() {
            ready.remove(&client_id);
        }
        state.threads.retain(|thread_id, subscribers| {
            let keep = !subscribers.is_empty();
            if !keep {
                empty_threads.push(thread_id.clone());
            }
            keep
        });
        let live_threads: BTreeSet<String> = state.threads.keys().cloned().collect();
        state
            .approval_ready
            .retain(|thread_id, ready| !ready.is_empty() && live_threads.contains(thread_id));
        Ok(empty_threads)
    }

    pub(crate) fn subscribe(&self, client_id: ClientId, thread_id: &str) -> Result<()> {
        let mut state = self.lock()?;
        state
            .threads
            .entry(thread_id.to_string())
            .or_default()
            .insert(client_id);
        state
            .approval_ready
            .entry(thread_id.to_string())
            .or_default()
            .insert(client_id);
        Ok(())
    }

    pub(crate) fn subscribe_pending_approvals(
        &self,
        client_id: ClientId,
        thread_id: &str,
    ) -> Result<()> {
        let mut state = self.lock()?;
        state
            .threads
            .entry(thread_id.to_string())
            .or_default()
            .insert(client_id);
        let remove_empty_ready = if let Some(ready) = state.approval_ready.get_mut(thread_id) {
            ready.remove(&client_id);
            ready.is_empty()
        } else {
            false
        };
        if remove_empty_ready {
            state.approval_ready.remove(thread_id);
        }
        Ok(())
    }

    pub(crate) fn mark_approval_ready(&self, client_id: ClientId, thread_id: &str) -> Result<()> {
        let mut state = self.lock()?;
        if state
            .threads
            .get(thread_id)
            .is_some_and(|subscribers| subscribers.contains(&client_id))
        {
            state
                .approval_ready
                .entry(thread_id.to_string())
                .or_default()
                .insert(client_id);
        }
        Ok(())
    }

    pub(crate) fn unsubscribe(&self, client_id: ClientId, thread_id: &str) -> Result<usize> {
        let mut state = self.lock()?;
        let Some(subscribers) = state.threads.get_mut(thread_id) else {
            return Ok(0);
        };
        subscribers.remove(&client_id);
        let remaining = subscribers.len();
        let remove_empty_ready = if let Some(ready) = state.approval_ready.get_mut(thread_id) {
            ready.remove(&client_id);
            ready.is_empty()
        } else {
            false
        };
        if remove_empty_ready {
            state.approval_ready.remove(thread_id);
        }
        if remaining == 0 {
            state.threads.remove(thread_id);
        }
        Ok(remaining)
    }

    pub(crate) fn subscriber_count(&self, thread_id: &str) -> Result<usize> {
        let state = self.lock()?;
        Ok(state.threads.get(thread_id).map(BTreeSet::len).unwrap_or(0))
    }

    pub(crate) fn subscriber_counts(&self) -> Result<BTreeMap<String, usize>> {
        let state = self.lock()?;
        Ok(state
            .threads
            .iter()
            .map(|(thread_id, subscribers)| (thread_id.clone(), subscribers.len()))
            .collect())
    }

    pub(crate) fn send_client(&self, client_id: ClientId, message: String) -> Result<bool> {
        let state = self.lock()?;
        let Some(client) = state.clients.get(&client_id) else {
            return Ok(false);
        };
        Ok(client.sender.send(ClientOutbound::Text(message)).is_ok())
    }

    pub(crate) fn fan_out_thread(&self, thread_id: &str, message: String) -> Result<usize> {
        let state = self.lock()?;
        let Some(subscribers) = state.threads.get(thread_id) else {
            return Ok(0);
        };
        let mut sent = 0usize;
        for client_id in subscribers {
            let Some(client) = state.clients.get(client_id) else {
                continue;
            };
            if client
                .sender
                .send(ClientOutbound::Text(message.clone()))
                .is_ok()
            {
                sent += 1;
            }
        }
        Ok(sent)
    }

    pub(crate) fn fan_out_thread_approvals(
        &self,
        thread_id: &str,
        message: String,
    ) -> Result<FanOutResult> {
        let state = self.lock()?;
        let subscribers = state.threads.get(thread_id).map(BTreeSet::len).unwrap_or(0);
        let Some(ready) = state.approval_ready.get(thread_id) else {
            return Ok(FanOutResult {
                subscribers,
                ready: 0,
                sent: 0,
            });
        };
        let mut sent = 0usize;
        for client_id in ready {
            let Some(client) = state.clients.get(client_id) else {
                continue;
            };
            if client
                .sender
                .send(ClientOutbound::Text(message.clone()))
                .is_ok()
            {
                sent += 1;
            }
        }
        Ok(FanOutResult {
            subscribers,
            ready: ready.len(),
            sent,
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, SubscriptionState>> {
        self.inner
            .lock()
            .map_err(|_| anyhow::anyhow!("subscription hub lock poisoned"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fanout_only_sends_to_thread_subscribers() {
        let hub = SubscriptionHub::default();
        let (left_tx, left_rx) = mpsc::channel();
        let (right_tx, right_rx) = mpsc::channel();
        let left = ClientId::new(1);
        let right = ClientId::new(2);
        hub.register_client(left, ClientSink::new(left_tx)).unwrap();
        hub.register_client(right, ClientSink::new(right_tx))
            .unwrap();
        hub.subscribe(left, "thread-a").unwrap();
        hub.subscribe(right, "thread-b").unwrap();

        assert_eq!(
            hub.fan_out_thread("thread-a", "event".to_string()).unwrap(),
            1
        );
        assert_eq!(
            left_rx.try_recv().unwrap(),
            ClientOutbound::Text("event".to_string())
        );
        assert!(right_rx.try_recv().is_err());
    }

    #[test]
    fn approval_fanout_skips_pending_attach_clients() {
        let hub = SubscriptionHub::default();
        let (pending_tx, pending_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let pending = ClientId::new(1);
        let ready = ClientId::new(2);
        hub.register_client(pending, ClientSink::new(pending_tx))
            .unwrap();
        hub.register_client(ready, ClientSink::new(ready_tx))
            .unwrap();
        hub.subscribe_pending_approvals(pending, "thread-a")
            .unwrap();
        hub.subscribe(ready, "thread-a").unwrap();

        assert_eq!(
            hub.fan_out_thread("thread-a", "event".to_string()).unwrap(),
            2
        );
        assert_eq!(
            pending_rx.try_recv().unwrap(),
            ClientOutbound::Text("event".to_string())
        );
        assert_eq!(
            ready_rx.try_recv().unwrap(),
            ClientOutbound::Text("event".to_string())
        );

        assert_eq!(
            hub.fan_out_thread_approvals("thread-a", "approval".to_string())
                .unwrap(),
            FanOutResult {
                subscribers: 2,
                ready: 1,
                sent: 1,
            }
        );
        assert!(pending_rx.try_recv().is_err());
        assert_eq!(
            ready_rx.try_recv().unwrap(),
            ClientOutbound::Text("approval".to_string())
        );

        hub.mark_approval_ready(pending, "thread-a").unwrap();
        assert_eq!(
            hub.fan_out_thread_approvals("thread-a", "next".to_string())
                .unwrap(),
            FanOutResult {
                subscribers: 2,
                ready: 2,
                sent: 2,
            }
        );
        assert_eq!(
            pending_rx.try_recv().unwrap(),
            ClientOutbound::Text("next".to_string())
        );
        assert_eq!(
            ready_rx.try_recv().unwrap(),
            ClientOutbound::Text("next".to_string())
        );
    }
}
