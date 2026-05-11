//! Method-level routing for the cx app-server broker.
//!
//! This module is deliberately small: it extracts stable routing keys from the
//! private Codex app-server JSON-RPC envelope and leaves scheduling policy to
//! the broker and worker pool.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BrokerRoute {
    Initialize,
    ThreadList,
    ThreadRead { thread_id: String },
    ThreadResume { thread_id: String },
    ThreadStart,
    ThreadUnsubscribe { thread_id: String },
    TurnStart { thread_id: String },
    TurnSteer { thread_id: String, turn_id: String },
    TurnInterrupt { thread_id: String, turn_id: String },
    WorkerByThread { thread_id: String },
    WorkerDefault,
}

pub(crate) fn route_request(method: &str, params: Option<&Value>) -> BrokerRoute {
    match method {
        "initialize" => BrokerRoute::Initialize,
        "thread/list" => BrokerRoute::ThreadList,
        "thread/read" => params
            .and_then(thread_id)
            .map(|thread_id| BrokerRoute::ThreadRead { thread_id })
            .unwrap_or(BrokerRoute::WorkerDefault),
        "thread/resume" => params
            .and_then(thread_id)
            .map(|thread_id| BrokerRoute::ThreadResume { thread_id })
            .unwrap_or(BrokerRoute::WorkerDefault),
        "thread/start" => BrokerRoute::ThreadStart,
        "thread/unsubscribe" => params
            .and_then(thread_id)
            .map(|thread_id| BrokerRoute::ThreadUnsubscribe { thread_id })
            .unwrap_or(BrokerRoute::WorkerDefault),
        "turn/start" => params
            .and_then(thread_id)
            .map(|thread_id| BrokerRoute::TurnStart { thread_id })
            .unwrap_or(BrokerRoute::WorkerDefault),
        "turn/steer" => {
            let thread_id = params.and_then(thread_id);
            let turn_id = params.and_then(expected_turn_id);
            match (thread_id, turn_id) {
                (Some(thread_id), Some(turn_id)) => BrokerRoute::TurnSteer { thread_id, turn_id },
                (Some(thread_id), None) => BrokerRoute::WorkerByThread { thread_id },
                _ => BrokerRoute::WorkerDefault,
            }
        }
        "turn/interrupt" => {
            let thread_id = params.and_then(thread_id);
            let turn_id = params.and_then(turn_id);
            match (thread_id, turn_id) {
                (Some(thread_id), Some(turn_id)) => {
                    BrokerRoute::TurnInterrupt { thread_id, turn_id }
                }
                (Some(thread_id), None) => BrokerRoute::WorkerByThread { thread_id },
                _ => BrokerRoute::WorkerDefault,
            }
        }
        _ => params
            .and_then(thread_id)
            .map(|thread_id| BrokerRoute::WorkerByThread { thread_id })
            .unwrap_or(BrokerRoute::WorkerDefault),
    }
}

pub(crate) fn message_thread_id(message: &Value) -> Option<String> {
    message
        .get("params")
        .and_then(thread_id)
        .or_else(|| message.get("result").and_then(result_thread_id))
}

pub(crate) fn message_turn_id(message: &Value) -> Option<String> {
    message
        .get("params")
        .and_then(turn_id)
        .or_else(|| message.get("params").and_then(notification_turn_id))
        .or_else(|| message.get("result").and_then(result_turn_id))
}

fn result_thread_id(result: &Value) -> Option<String> {
    result
        .get("thread")
        .and_then(|thread| thread.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn result_turn_id(result: &Value) -> Option<String> {
    result
        .get("turn")
        .and_then(|turn| turn.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn thread_id(params: &Value) -> Option<String> {
    params
        .get("threadId")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn turn_id(params: &Value) -> Option<String> {
    params
        .get("turnId")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn expected_turn_id(params: &Value) -> Option<String> {
    params
        .get("expectedTurnId")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn notification_turn_id(params: &Value) -> Option<String> {
    params
        .get("turn")
        .and_then(|turn| turn.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn routes_turn_interrupt_by_thread_and_turn() {
        let route = route_request(
            "turn/interrupt",
            Some(&json!({"threadId": "t1", "turnId": "u1"})),
        );

        assert_eq!(
            route,
            BrokerRoute::TurnInterrupt {
                thread_id: "t1".to_string(),
                turn_id: "u1".to_string(),
            }
        );
    }

    #[test]
    fn extracts_thread_id_from_notifications() {
        let message = json!({
            "method": "item/agentMessage/delta",
            "params": {"threadId": "thread-1", "turnId": "turn-1"}
        });

        assert_eq!(message_thread_id(&message), Some("thread-1".to_string()));
        assert_eq!(message_turn_id(&message), Some("turn-1".to_string()));
    }
}
