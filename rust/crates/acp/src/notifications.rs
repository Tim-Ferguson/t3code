//! The source ACP stream consumes one shared sliding queue, rather than a
//! broadcast subscription. Reading a notification removes it for every reader.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tokio::sync::Notify;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "_tag")]
pub enum IncomingNotification {
    SessionUpdate { method: String, params: Value },
    ElicitationComplete { method: String, params: Value },
    ExtNotification { method: String, params: Value },
}

#[derive(Default)]
pub(crate) struct NotificationQueue {
    values: Mutex<VecDeque<IncomingNotification>>,
    available: Notify,
}
impl NotificationQueue {
    pub(crate) fn offer(&self, notification: IncomingNotification) {
        let mut values = self.values.lock().unwrap();
        if values.len() == 32 {
            values.pop_front();
        }
        values.push_back(notification);
        drop(values);
        self.available.notify_one();
    }
}

#[derive(Clone)]
pub struct NotificationStream(pub(crate) Arc<NotificationQueue>);
impl NotificationStream {
    /// Wait for the next queued notification. ACP input termination does not
    /// end this source-compatible raw stream; callers own stream cancellation.
    pub async fn recv(&self) -> IncomingNotification {
        loop {
            let available = self.0.available.notified();
            tokio::pin!(available);
            // Register before checking the queue, including when several
            // concurrent consumers share it, so an offer cannot lose a wakeup.
            available.as_mut().enable();
            if let Some(value) = self.try_recv() {
                return value;
            }
            available.await;
        }
    }
    pub fn try_recv(&self) -> Option<IncomingNotification> {
        self.0.values.lock().unwrap().pop_front()
    }
}
