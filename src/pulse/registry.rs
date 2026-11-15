//! The objects a PulseAudio context can see, and the events about them.

use crate::pulse::wire;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    pub facility: u32,
    pub change: u32,
    pub index: u32,
}

/// A playback stream as every context sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SinkInput {
    pub index: u32,
    pub client: u32,
    pub name: String,
    pub rate: u32,
    pub channels: u8,
    pub format: u8,
    pub corked: bool,
    pub db_millis: i32,
    /// Set to PulseAudio's silence, volume 0: muted in `aas`, reported as
    /// volume 0 rather than as a mute.
    pub silent: bool,
    pub mute: bool,
}

/// A change to a stream that another connection asked for. Only the
/// connection that owns the stream holds its daemon client, so it applies
/// the change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Remote {
    Volume { stream: u32, db_millis: i32, silent: bool },
    Mute { stream: u32, muted: bool },
}

/// Every connection's client and stream, and everyone waiting for events.
pub struct Registry {
    clients: Mutex<BTreeMap<u32, String>>,
    inboxes: Mutex<BTreeMap<u32, Sender<Remote>>>,
    sink_inputs: Mutex<BTreeMap<u32, SinkInput>>,
    sink_mute: Mutex<Option<u8>>,
    next_stream: AtomicU32,
    events: Mutex<Vec<Sender<Event>>>,
}

impl Default for Registry {
    fn default() -> Self {
        Registry {
            clients: Mutex::new(BTreeMap::new()),
            inboxes: Mutex::new(BTreeMap::new()),
            sink_inputs: Mutex::new(BTreeMap::new()),
            sink_mute: Mutex::new(None),
            next_stream: AtomicU32::new(1),
            events: Mutex::new(Vec::new()),
        }
    }
}

impl Registry {
    pub fn next_stream(&self) -> u32 {
        self.next_stream.fetch_add(1, Ordering::Relaxed)
    }

    pub fn add_client(&self, index: u32, name: String) {
        self.clients.lock().unwrap().insert(index, name);
        self.notify(Event {
            facility: wire::EVENT_CLIENT,
            change: wire::EVENT_NEW,
            index,
        });
    }

    pub fn rename_client(&self, index: u32, name: String) {
        self.clients.lock().unwrap().insert(index, name);
        self.notify(Event {
            facility: wire::EVENT_CLIENT,
            change: wire::EVENT_CHANGE,
            index,
        });
    }

    /// The inbox of the connection whose client is `index`, for changes
    /// other connections ask for.
    pub fn inbox(&self, index: u32) -> Receiver<Remote> {
        let (sender, receiver) = channel();
        self.inboxes.lock().unwrap().insert(index, sender);
        receiver
    }

    /// Hands a change to the connection whose client is `client`. False if
    /// that connection is gone.
    pub fn send_remote(&self, client: u32, remote: Remote) -> bool {
        self.inboxes
            .lock()
            .unwrap()
            .get(&client)
            .is_some_and(|sender| sender.send(remote).is_ok())
    }

    pub fn remove_client(&self, index: u32) {
        self.inboxes.lock().unwrap().remove(&index);
        if self.clients.lock().unwrap().remove(&index).is_some() {
            self.notify(Event {
                facility: wire::EVENT_CLIENT,
                change: wire::EVENT_REMOVE,
                index,
            });
        }
    }

    pub fn clients(&self) -> Vec<(u32, String)> {
        self.clients
            .lock()
            .unwrap()
            .iter()
            .map(|(&index, name)| (index, name.clone()))
            .collect()
    }

    pub fn add_sink_input(&self, input: SinkInput) {
        let index = input.index;
        self.sink_inputs.lock().unwrap().insert(index, input);
        self.notify(Event {
            facility: wire::EVENT_SINK_INPUT,
            change: wire::EVENT_NEW,
            index,
        });
    }

    pub fn update_sink_input(&self, input: SinkInput) {
        let index = input.index;
        self.sink_inputs.lock().unwrap().insert(index, input);
        self.notify(Event {
            facility: wire::EVENT_SINK_INPUT,
            change: wire::EVENT_CHANGE,
            index,
        });
    }

    pub fn remove_sink_input(&self, index: u32) {
        if self.sink_inputs.lock().unwrap().remove(&index).is_some() {
            self.notify(Event {
                facility: wire::EVENT_SINK_INPUT,
                change: wire::EVENT_REMOVE,
                index,
            });
        }
    }

    pub fn sink_inputs(&self) -> Vec<SinkInput> {
        self.sink_inputs.lock().unwrap().values().cloned().collect()
    }

    pub fn sink_input(&self, index: u32) -> Option<SinkInput> {
        self.sink_inputs.lock().unwrap().get(&index).cloned()
    }

    /// The output percent remembered before a mute, while the sink is
    /// muted. Shared, because a client's connection may close between the
    /// mute and the unmute.
    pub fn sink_muted(&self) -> Option<u8> {
        *self.sink_mute.lock().unwrap()
    }

    pub fn set_sink_muted(&self, percent: Option<u8>) {
        *self.sink_mute.lock().unwrap() = percent;
    }

    pub fn subscribe(self: &Arc<Self>) -> Receiver<Event> {
        let (sender, receiver) = channel();
        self.events.lock().unwrap().push(sender);
        receiver
    }

    pub fn notify(&self, event: Event) {
        self.events
            .lock()
            .unwrap()
            .retain(|sender| sender.send(event).is_ok());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subscriber_hears_membership() {
        let registry = Arc::new(Registry::default());
        let events = registry.subscribe();
        registry.add_client(1, "mpv".into());
        registry.rename_client(1, "mpv-2".into());
        registry.remove_client(1);
        let heard: Vec<Event> = events.try_iter().collect();
        assert_eq!(heard.len(), 3);
        assert_eq!(heard[0].change, wire::EVENT_NEW);
        assert_eq!(heard[1].change, wire::EVENT_CHANGE);
        assert_eq!(heard[2].change, wire::EVENT_REMOVE);
        assert!(registry.clients().is_empty());
    }
}
