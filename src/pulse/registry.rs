//! The objects a PulseAudio context can see, and the events about them.

use crate::pulse::wire;
use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    pub facility: u32,
    pub change: u32,
    pub index: u32,
}

/// Every connection's client and stream, and everyone waiting for events.
#[derive(Default)]
pub struct Registry {
    clients: Mutex<BTreeMap<u32, String>>,
    events: Mutex<Vec<Sender<Event>>>,
}

impl Registry {
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

    pub fn remove_client(&self, index: u32) {
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
