//! The objects a PulseAudio context can see, and the events about them.

use crate::pulse::objects::Source;
use crate::pulse::wire;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
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
    /// The daemon's id of its stream, 0 if unknown.
    pub stream: u32,
    pub db_millis: i32,
    /// Set to PulseAudio's silence, volume 0: muted in `aas`, reported as
    /// volume 0 rather than as a mute.
    pub silent: bool,
    pub mute: bool,
    /// What its ring holds and what plays after it, as last measured.
    pub buffer_usec: u64,
    pub sink_usec: u64,
}

/// A record stream as every context sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceOutput {
    pub index: u32,
    pub client: u32,
    pub name: String,
    pub source: Source,
    pub rate: u32,
    pub channels: u8,
    pub format: u8,
    pub corked: bool,
    /// Whether it is resampled from the daemon's rate.
    pub resampled: bool,
    /// What waits to be sent and what the source holds before it, as last
    /// measured.
    pub buffer_usec: u64,
    pub source_usec: u64,
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
    source_outputs: Mutex<BTreeMap<u32, SourceOutput>>,
    sink_mute: Mutex<Option<u8>>,
    /// The output's delay after a stream's ring, as a playing stream last
    /// measured it.
    sink_latency: AtomicU64,
    next_stream: AtomicU32,
    default_source: Mutex<Source>,
    events: Mutex<Vec<Sender<Event>>>,
}

impl Registry {
    /// An empty registry. With an analog input, it is the default source.
    pub fn new(analog: bool) -> Self {
        Registry {
            clients: Mutex::new(BTreeMap::new()),
            inboxes: Mutex::new(BTreeMap::new()),
            sink_inputs: Mutex::new(BTreeMap::new()),
            source_outputs: Mutex::new(BTreeMap::new()),
            sink_mute: Mutex::new(None),
            sink_latency: AtomicU64::new(0),
            next_stream: AtomicU32::new(1),
            default_source: Mutex::new(if analog {
                Source::Analog
            } else {
                Source::Monitor
            }),
            events: Mutex::new(Vec::new()),
        }
    }

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

    pub fn update_sink_input(&self, mut input: SinkInput) {
        let index = input.index;
        let mut inputs = self.sink_inputs.lock().unwrap();
        if let Some(old) = inputs.get(&index) {
            // A stream's entry carries no measurement; keep the last one.
            input.buffer_usec = old.buffer_usec;
            input.sink_usec = old.sink_usec;
        }
        inputs.insert(index, input);
        drop(inputs);
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

    /// Records a stream's latency, without an event: it changes all the
    /// time and clients ask for it.
    pub fn set_latency(&self, index: u32, buffer_usec: u64, sink_usec: u64) {
        if let Some(input) = self.sink_inputs.lock().unwrap().get_mut(&index) {
            input.buffer_usec = buffer_usec;
            input.sink_usec = sink_usec;
        }
        self.sink_latency.store(sink_usec, Ordering::Relaxed);
    }

    pub fn sink_latency(&self) -> u64 {
        self.sink_latency.load(Ordering::Relaxed)
    }

    pub fn sink_inputs(&self) -> Vec<SinkInput> {
        self.sink_inputs.lock().unwrap().values().cloned().collect()
    }

    pub fn sink_input(&self, index: u32) -> Option<SinkInput> {
        self.sink_inputs.lock().unwrap().get(&index).cloned()
    }

    pub fn add_source_output(&self, output: SourceOutput) {
        let index = output.index;
        self.source_outputs.lock().unwrap().insert(index, output);
        self.notify(source_output_event(wire::EVENT_NEW, index));
    }

    pub fn update_source_output(&self, mut output: SourceOutput) {
        let index = output.index;
        let mut outputs = self.source_outputs.lock().unwrap();
        if let Some(old) = outputs.get(&index) {
            // A stream's entry carries no measurement; keep the last one.
            output.buffer_usec = old.buffer_usec;
            output.source_usec = old.source_usec;
        }
        outputs.insert(index, output);
        drop(outputs);
        self.notify(source_output_event(wire::EVENT_CHANGE, index));
    }

    pub fn remove_source_output(&self, index: u32) {
        if self.source_outputs.lock().unwrap().remove(&index).is_some() {
            self.notify(source_output_event(wire::EVENT_REMOVE, index));
        }
    }

    /// Records a record stream's latency, without an event, as
    /// `set_latency` does for playback.
    pub fn set_source_output_latency(&self, index: u32, buffer_usec: u64, source_usec: u64) {
        if let Some(output) = self.source_outputs.lock().unwrap().get_mut(&index) {
            output.buffer_usec = buffer_usec;
            output.source_usec = source_usec;
        }
    }

    pub fn source_outputs(&self) -> Vec<SourceOutput> {
        self.source_outputs.lock().unwrap().values().cloned().collect()
    }

    pub fn source_output(&self, index: u32) -> Option<SourceOutput> {
        self.source_outputs.lock().unwrap().get(&index).cloned()
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

    pub fn default_source(&self) -> Source {
        *self.default_source.lock().unwrap()
    }

    /// Makes `source` the default, and tells subscribers the server
    /// changed if it did.
    pub fn set_default_source(&self, source: Source) {
        let previous = std::mem::replace(&mut *self.default_source.lock().unwrap(), source);
        if previous != source {
            self.notify(Event {
                facility: wire::EVENT_SERVER,
                change: wire::EVENT_CHANGE,
                index: wire::INVALID_INDEX,
            });
        }
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

fn source_output_event(change: u32, index: u32) -> Event {
    Event {
        facility: wire::EVENT_SOURCE_OUTPUT,
        change,
        index,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subscriber_hears_membership() {
        let registry = Arc::new(Registry::new(false));
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

    #[test]
    fn the_default_source_is_the_analog_input_when_there_is_one() {
        assert_eq!(Registry::new(false).default_source(), Source::Monitor);
        let registry = Arc::new(Registry::new(true));
        assert_eq!(registry.default_source(), Source::Analog);
        let events = registry.subscribe();
        registry.set_default_source(Source::Monitor);
        registry.set_default_source(Source::Monitor);
        assert_eq!(registry.default_source(), Source::Monitor);
        let heard: Vec<Event> = events.try_iter().collect();
        assert_eq!(heard.len(), 1, "only a change is announced");
        assert_eq!(
            heard[0].facility | heard[0].change,
            wire::EVENT_SERVER | wire::EVENT_CHANGE
        );
    }

    #[test]
    fn source_outputs_are_announced_and_keep_their_latency() {
        let registry = Arc::new(Registry::new(false));
        let events = registry.subscribe();
        let output = SourceOutput {
            index: 4,
            client: 1,
            name: "rec".into(),
            source: Source::Monitor,
            rate: 48_000,
            channels: 2,
            format: wire::FORMAT_S16LE,
            corked: false,
            resampled: false,
            buffer_usec: 0,
            source_usec: 0,
        };
        registry.add_source_output(output.clone());
        registry.set_source_output_latency(4, 10, 20);
        registry.update_source_output(SourceOutput {
            corked: true,
            ..output
        });
        let kept = registry.source_output(4).unwrap();
        assert_eq!((kept.corked, kept.buffer_usec, kept.source_usec), (true, 10, 20));
        assert_eq!(registry.source_outputs().len(), 1);
        registry.remove_source_output(4);
        registry.remove_source_output(4);
        let heard: Vec<u32> = events
            .try_iter()
            .map(|event| event.facility | event.change)
            .collect();
        let facility = wire::EVENT_SOURCE_OUTPUT;
        assert_eq!(
            heard,
            [
                facility | wire::EVENT_NEW,
                facility | wire::EVENT_CHANGE,
                facility | wire::EVENT_REMOVE
            ]
        );
    }
}
