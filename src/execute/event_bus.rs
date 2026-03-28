use crate::execute::event::ExecutionEvent;
use tokio::sync::broadcast;

/// Default channel capacity for the event bus.
const DEFAULT_CAPACITY: usize = 256;

/// Broadcast-based event bus for execution events.
///
/// Subscribers receive all events emitted after they subscribe.
/// Events are also buffered for batch retrieval after execution completes.
/// Uses `tokio::sync::broadcast` — slow subscribers that fall behind
/// will miss events (lagged).
pub struct EventBus {
    sender: broadcast::Sender<ExecutionEvent>,
}

impl EventBus {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    /// Emit an event to all current subscribers.
    /// Returns the number of subscribers that received the event.
    /// If there are no subscribers, the event is silently dropped.
    pub fn emit(&self, event: ExecutionEvent) -> usize {
        self.sender.send(event).unwrap_or(0)
    }

    /// Subscribe to receive future events.
    pub fn subscribe(&self) -> EventSubscriber {
        EventSubscriber {
            receiver: self.sender.subscribe(),
        }
    }

    /// Number of active subscribers.
    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

/// A subscriber that receives execution events from an `EventBus`.
pub struct EventSubscriber {
    receiver: broadcast::Receiver<ExecutionEvent>,
}

impl EventSubscriber {
    /// Receive the next event, waiting if none are available.
    pub async fn recv(&mut self) -> Result<ExecutionEvent, EventBusError> {
        match self.receiver.recv().await {
            Ok(event) => Ok(event),
            Err(broadcast::error::RecvError::Lagged(count)) => {
                Err(EventBusError::Lagged(count))
            }
            Err(broadcast::error::RecvError::Closed) => Err(EventBusError::Closed),
        }
    }

    /// Try to receive an event without waiting.
    pub fn try_recv(&mut self) -> Result<ExecutionEvent, EventBusError> {
        match self.receiver.try_recv() {
            Ok(event) => Ok(event),
            Err(broadcast::error::TryRecvError::Empty) => Err(EventBusError::Empty),
            Err(broadcast::error::TryRecvError::Lagged(count)) => {
                Err(EventBusError::Lagged(count))
            }
            Err(broadcast::error::TryRecvError::Closed) => Err(EventBusError::Closed),
        }
    }
}

/// Errors from event bus operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventBusError {
    /// The subscriber fell behind and missed events.
    Lagged(u64),
    /// The bus has been dropped (execution finished).
    Closed,
    /// No events currently available (non-blocking try_recv).
    Empty,
}

impl std::fmt::Display for EventBusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EventBusError::Lagged(n) => write!(f, "subscriber lagged, missed {n} events"),
            EventBusError::Closed => write!(f, "event bus closed"),
            EventBusError::Empty => write!(f, "no events available"),
        }
    }
}

impl std::error::Error for EventBusError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execute::lifecycle::NodeState;
    use std::time::Instant;

    fn state_event(node: &str, from: NodeState, to: NodeState) -> ExecutionEvent {
        ExecutionEvent::StateChanged {
            node_id: node.into(),
            from,
            to,
            timestamp: Instant::now(),
        }
    }

    #[tokio::test]
    async fn subscribe_receives_events() {
        let bus = EventBus::new();
        let mut sub = bus.subscribe();

        bus.emit(state_event("A", NodeState::Idle, NodeState::Pending));
        bus.emit(state_event("A", NodeState::Pending, NodeState::Running));

        let e1 = sub.recv().await.unwrap();
        assert!(matches!(e1, ExecutionEvent::StateChanged { ref node_id, .. } if node_id == "A"));
        let e2 = sub.recv().await.unwrap();
        assert!(matches!(e2, ExecutionEvent::StateChanged { to: NodeState::Running, .. }));
    }

    #[tokio::test]
    async fn no_subscribers_does_not_panic() {
        let bus = EventBus::new();
        let count = bus.emit(state_event("A", NodeState::Idle, NodeState::Pending));
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn multiple_subscribers() {
        let bus = EventBus::new();
        let mut sub1 = bus.subscribe();
        let mut sub2 = bus.subscribe();

        bus.emit(state_event("A", NodeState::Idle, NodeState::Pending));

        assert!(sub1.recv().await.is_ok());
        assert!(sub2.recv().await.is_ok());
    }

    #[test]
    fn subscriber_count() {
        let bus = EventBus::new();
        assert_eq!(bus.subscriber_count(), 0);
        let _sub1 = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 1);
        let _sub2 = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 2);
        drop(_sub1);
        assert_eq!(bus.subscriber_count(), 1);
    }

    #[tokio::test]
    async fn try_recv_empty() {
        let bus = EventBus::new();
        let mut sub = bus.subscribe();
        assert_eq!(sub.try_recv().unwrap_err(), EventBusError::Empty);
    }

    #[tokio::test]
    async fn try_recv_has_event() {
        let bus = EventBus::new();
        let mut sub = bus.subscribe();
        bus.emit(state_event("A", NodeState::Idle, NodeState::Pending));
        assert!(sub.try_recv().is_ok());
    }

    #[tokio::test]
    async fn bus_dropped_signals_closed() {
        let bus = EventBus::new();
        let mut sub = bus.subscribe();
        drop(bus);
        assert_eq!(sub.recv().await.unwrap_err(), EventBusError::Closed);
    }
}
