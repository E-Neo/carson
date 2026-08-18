use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone)]
pub struct SseItem {
    pub event: String,
    pub data: serde_json::Value,
}

/// Fan-out hub: each session has a set of live SSE subscribers (one per tab/stream).
/// Every `send` broadcasts to all of them and prunes closed channels.
pub struct Hub {
    sessions: Mutex<HashMap<u64, Vec<UnboundedSender<SseItem>>>>,
}

impl Hub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            sessions: Mutex::new(HashMap::new()),
        })
    }

    pub fn register(&self, session_id: u64, tx: UnboundedSender<SseItem>) {
        self.sessions
            .lock()
            .unwrap()
            .entry(session_id)
            .or_default()
            .push(tx);
    }

    pub fn unregister(&self, session_id: u64, tx: &UnboundedSender<SseItem>) {
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(txs) = sessions.get_mut(&session_id) {
            txs.retain(|t| !t.same_channel(tx));
        }
        if sessions
            .get(&session_id)
            .map(|v| v.is_empty())
            .unwrap_or(false)
        {
            sessions.remove(&session_id);
        }
    }

    pub fn alive(&self, session_id: u64) -> bool {
        self.sessions
            .lock()
            .unwrap()
            .get(&session_id)
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    }

    /// Broadcast to every live subscriber, dropping closed channels, and report
    /// whether at least one subscriber received the item.
    pub fn send(&self, session_id: u64, item: SseItem) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        let Some(txs) = sessions.get_mut(&session_id) else {
            return false;
        };
        txs.retain(|tx| tx.send(item.clone()).is_ok());
        !txs.is_empty()
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
        hub.register(1, tx.clone());
        assert!(hub.alive(1));
        assert!(hub.send(1, item("chunk")));
        let received = rx.try_recv().unwrap();
        assert_eq!(received.event, "chunk");
        assert_eq!(
            sse_frame(&received),
            "event: chunk\ndata: {\"k\":\"v\"}\n\n"
        );
        hub.unregister(1, &tx);
        assert!(!hub.alive(1));
        assert!(!hub.send(1, item("chunk")));
    }

    #[test]
    fn send_broadcasts_to_all_subscribers() {
        let hub = Hub::new();
        let (tx_a, mut rx_a) = tokio::sync::mpsc::unbounded_channel();
        let (tx_b, mut rx_b) = tokio::sync::mpsc::unbounded_channel();
        hub.register(3, tx_a);
        hub.register(3, tx_b);
        assert!(hub.send(3, item("chunk")));
        assert_eq!(rx_a.try_recv().unwrap().event, "chunk");
        assert_eq!(rx_b.try_recv().unwrap().event, "chunk");
    }

    #[test]
    fn unregister_removes_only_matching_channel() {
        let hub = Hub::new();
        let (tx_a, _rx_a) = tokio::sync::mpsc::unbounded_channel();
        let (tx_b, mut rx_b) = tokio::sync::mpsc::unbounded_channel();
        hub.register(5, tx_a.clone());
        hub.register(5, tx_b.clone());
        hub.unregister(5, &tx_a);
        assert!(hub.alive(5));
        assert!(hub.send(5, item("chunk")));
        assert_eq!(rx_b.try_recv().unwrap().event, "chunk");
        hub.unregister(5, &tx_b);
        assert!(!hub.alive(5));
    }

    #[test]
    fn send_to_unregistered_session_fails() {
        let hub = Hub::new();
        assert!(!hub.send(42, item("chunk")));
    }

    #[test]
    fn send_prunes_dropped_receivers() {
        let hub = Hub::new();
        let (tx_a, mut rx_a) = tokio::sync::mpsc::unbounded_channel();
        let (tx_b, rx_b) = tokio::sync::mpsc::unbounded_channel();
        hub.register(9, tx_a);
        hub.register(9, tx_b);
        drop(rx_b);
        assert!(hub.send(9, item("chunk")));
        assert_eq!(rx_a.try_recv().unwrap().event, "chunk");
        assert!(hub.alive(9));
    }
}
