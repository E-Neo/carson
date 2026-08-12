use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone)]
pub struct SseItem {
    pub event: String,
    pub data: serde_json::Value,
}

pub struct Hub {
    sessions: Mutex<HashMap<u64, UnboundedSender<SseItem>>>,
}

impl Hub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            sessions: Mutex::new(HashMap::new()),
        })
    }

    pub fn register(&self, session_id: u64, tx: UnboundedSender<SseItem>) {
        self.sessions.lock().unwrap().insert(session_id, tx);
    }

    pub fn unregister(&self, session_id: u64) {
        self.sessions.lock().unwrap().remove(&session_id);
    }

    pub fn alive(&self, session_id: u64) -> bool {
        self.sessions.lock().unwrap().contains_key(&session_id)
    }

    pub fn send(&self, session_id: u64, item: SseItem) -> bool {
        let sessions = self.sessions.lock().unwrap();
        sessions
            .get(&session_id)
            .map(|tx| tx.send(item).is_ok())
            .unwrap_or(false)
    }
}

pub fn sse_frame(item: &SseItem) -> String {
    format!("event: {}\ndata: {}\n\n", item.event, item.data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(event: &str) -> SseItem {
        SseItem {
            event: event.into(),
            data: json!({"k": "v"}),
        }
    }

    #[test]
    fn register_send_alive_unregister() {
        let hub = Hub::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(!hub.alive(1));
        hub.register(1, tx);
        assert!(hub.alive(1));
        assert!(hub.send(1, item("chunk")));
        let received = rx.try_recv().unwrap();
        assert_eq!(received.event, "chunk");
        assert_eq!(
            sse_frame(&received),
            "event: chunk\ndata: {\"k\":\"v\"}\n\n"
        );
        hub.unregister(1);
        assert!(!hub.alive(1));
        assert!(!hub.send(1, item("chunk")));
    }

    #[test]
    fn send_to_unregistered_session_fails() {
        let hub = Hub::new();
        assert!(!hub.send(42, item("chunk")));
    }

    #[test]
    fn send_fails_after_receiver_dropped() {
        let hub = Hub::new();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        hub.register(7, tx);
        drop(rx);
        assert!(!hub.send(7, item("chunk")));
    }
}
