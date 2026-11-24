//! `GET /graph`: the processing graph as the control thread holds it, and
//! its Graphviz form. The JSON is built here from the views; the DOT is made
//! from the JSON by the HTTP thread, so that it costs the control thread
//! nothing.

use super::Control;
use super::api::format_name;
use crate::config::PluginPlace;
use crate::latency::PERIODS;
use crate::plugin::Channels;
use crate::plugin::chain::conversion;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::atomic::Ordering::Relaxed;

/// The width of the players bus, of the analog input and bus, and of the
/// AES67 input and bus.
const PLAYER_CHANNELS: usize = 2;
const ANALOG_CHANNELS: usize = 2;
const AES67_CHANNELS: usize = 2;

/// Nodes and edges, in the order they are made.
#[derive(Default)]
struct Graph {
    nodes: Vec<Value>,
    edges: Vec<Value>,
}

impl Graph {
    fn node(&mut self, id: &str, kind: &str, label: &str, extra: Value) {
        let mut node = json!({"id": id, "kind": kind, "label": label});
        if let (Some(node), Value::Object(extra)) = (node.as_object_mut(), extra) {
            node.extend(extra);
        }
        self.nodes.push(node);
    }

    fn edge(&mut self, from: &str, to: &str) {
        self.edges.push(json!({"from": from, "to": to}));
    }

    /// Makes `ids` a path, each node leading to the next.
    fn path(&mut self, ids: &[String]) {
        for pair in ids.windows(2) {
            self.edge(&pair[0], &pair[1]);
        }
    }
}

impl Control {
    /// The master's rate, which every node of the processing runs at.
    fn graph_rate(&self) -> Option<u32> {
        self.output.or(self.parked).map(|(rate, _)| rate)
    }

    /// `node` with the master's rate, when there is one.
    fn with_rate(&self, mut node: Value) -> Value {
        if let (Some(rate), Some(map)) = (self.graph_rate(), node.as_object_mut()) {
            map.insert("rate".into(), json!(rate));
        }
        node
    }

    pub(super) fn graph(&self) -> Value {
        let mut g = Graph::default();
        let format = format_name(self.format);
        let with_rate = |extra: Value| self.with_rate(extra);

        // Each connected client by name, into the players bus.
        let mut clients = BTreeMap::new();
        for conn in self.conns.values() {
            if let Some(stream) = &conn.stream {
                let latency = stream.view.header().output_delay_frames.load(Relaxed);
                clients.insert(conn.name.clone(), (stream.rate, latency));
            }
        }
        for (name, (client_rate, latency)) in &clients {
            let id = format!("client/{name}");
            let extra = json!({
                "rate": client_rate,
                "channels": PLAYER_CHANNELS,
                "latency_frames": latency
            });
            g.node(&id, "client", name, extra);
            g.edge(&id, "bus/players");
        }
        g.node(
            "bus/players",
            "bus",
            "players",
            with_rate(json!({"format": format, "channels": PLAYER_CHANNELS})),
        );

        // The analog input, its chain and its bus.
        if self.analog.is_some() {
            let latency = self.counters.analog_latency_frames.load(Relaxed);
            let extra = json!({"channels": ANALOG_CHANNELS, "latency_frames": latency});
            g.node("analog", "analog", "analog input", with_rate(extra));
            let mut path = vec!["analog".to_string()];
            for entry in self.entries_of(0) {
                if entry.config.place == PluginPlace::Analog {
                    path.push(self.plugin_node(&mut g, entry, ANALOG_CHANNELS));
                }
            }
            path.push("bus/analog".into());
            g.node(
                "bus/analog",
                "bus",
                "analog",
                with_rate(json!({"format": format, "channels": ANALOG_CHANNELS})),
            );
            g.path(&path);
        }

        // The AES67 input and its bus.
        if self.aes67_input {
            // It reaches the engine at the rate of the wire, as a client
            // at its own rate.
            let extra = json!({"rate": 48000, "channels": AES67_CHANNELS});
            g.node("aes67", "aes67", "AES67 input", extra);
            g.node(
                "bus/aes67",
                "bus",
                "aes67",
                with_rate(json!({"format": format, "channels": AES67_CHANNELS})),
            );
            g.edge("aes67", "bus/aes67");
        }

        for index in 0..=self.further.len() {
            self.output_nodes(&mut g, index);
        }
        json!({"nodes": g.nodes, "edges": g.edges})
    }

    /// A plugin's node, and its id. `width` is the channel count that
    /// reaches the plugin; the node's `channels` is the count it gives.
    fn plugin_node(
        &self,
        g: &mut Graph,
        entry: &super::plugins::PluginEntry,
        width: usize,
    ) -> String {
        let channels = match entry.channels {
            Channels::Fixed { output, .. } => output,
            Channels::Any => width,
        };
        let id = format!("out/{}", entry.id);
        let mut plugin = json!({
            "id": entry.id.to_string(),
            "ns_avg": entry
                .stats
                .total_ns
                .load(Relaxed)
                .checked_div(entry.stats.runs.load(Relaxed))
                .unwrap_or(0),
        });
        if let Some(to) = conversion(entry.formats, self.format) {
            plugin["converts_to"] = json!(format_name(to));
        }
        let extra = self.with_rate(json!({"format": format_name(self.format),
            "channels": channels,
            "enabled": entry.enabled,
            "latency_frames": entry.stats.latency_frames.load(Relaxed),
            "plugin": plugin}));
        g.node(&id, "plugin", &entry.name, extra);
        id
    }

    /// The nodes of the output at engine index `index`, from its matrix to
    /// its card, and the edges into and through them.
    fn output_nodes(&self, g: &mut Graph, index: usize) {
        let n = index + 1;
        let format = format_name(self.format);
        let rate = self.graph_rate();
        let extra = |extra: Value| self.with_rate(extra);
        let matrix = format!("out/{n}/matrix");
        let width = self.matrices[index].width;
        let node = json!({"format": format, "channels": width});
        g.node(&matrix, "matrix", "matrix", extra(node));
        // A bus feeds the matrix when a route starts from it.
        let routes = self.route_views(index);
        let buses = [
            ("bus/players", "players.", true),
            ("bus/analog", "analog.", self.analog.is_some()),
            ("bus/aes67", "aes67.", self.aes67_input),
        ];
        for (bus, prefix, present) in buses {
            let used = routes.iter().any(|r| r.from.starts_with(prefix));
            if used && present {
                g.edge(bus, &matrix);
            }
        }

        let volume = format!("out/{n}/volume");
        g.node(
            &volume,
            "volume",
            "volume",
            extra(json!({"format": format, "channels": width})),
        );
        let mut path = vec![matrix, volume];
        let entries = self.entries_of(index);
        let mut at = width;
        for place in [PluginPlace::Mix, PluginPlace::Output] {
            for entry in entries.iter().filter(|e| e.config.place == place) {
                path.push(self.plugin_node(g, entry, at));
                if let Channels::Fixed { output, .. } = entry.channels {
                    at = output;
                }
            }
        }
        let (card_rate, channels, period) = match index {
            0 => (
                rate,
                self.channels,
                self.output.or(self.parked).map(|(_, p)| p),
            ),
            _ => {
                let output = &self.further[index - 1];
                let current = output.current;
                (
                    current.map(|(r, ..)| r),
                    output.channels,
                    current.map(|(_, p, _)| p),
                )
            }
        };
        if index == 0 {
            let delay = format!("out/{n}/delay");
            let latency = self.delay.unwrap_or(0);
            let node = json!({"format": format, "channels": channels, "latency_frames": latency});
            g.node(&delay, "delay", "delay", extra(node));
            path.push(delay);
        }
        let card = format!("out/{n}/card");
        // The card's own sample format is not known here: this is the
        // processing format the card is fed.
        let mut node = json!({"channels": channels, "format": format});
        if let Some(rate) = card_rate {
            node["rate"] = json!(rate);
        }
        if let Some(period) = period {
            node["latency_frames"] = json!(period * PERIODS);
        }
        let label = self.card_of(index);
        let label = if label.is_empty() { "card" } else { label };
        g.node(&card, "card", label, node);
        path.push(card);
        g.path(&path);
    }
}

/// A DOT string, quoted. Labels are plain text: the nodes are boxes, not
/// records, so `{ } | < >` in a name mean nothing to Graphviz.
fn quoted(text: &str) -> String {
    let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{}\"", escaped.replace('\n', "\\n"))
}

/// The graph as Graphviz DOT: a node for each node, labelled with what it
/// has to say, and an arrow for each edge.
pub fn to_dot(graph: &Value) -> String {
    let mut dot = String::from("digraph aas {\n  rankdir=LR;\n");
    let empty = Vec::new();
    for node in graph["nodes"].as_array().unwrap_or(&empty) {
        let id = node["id"].as_str().unwrap_or_default();
        let mut label = node["label"].as_str().unwrap_or(id).to_string();
        if let Some(rate) = node["rate"].as_u64() {
            label.push_str(&format!("\n{rate} Hz"));
        }
        if let Some(channels) = node["channels"].as_u64() {
            label.push_str(&format!("\n{channels} ch"));
        }
        if let Some(format) = node["format"].as_str() {
            label.push_str(&format!("\n{format}"));
        }
        if let Some(frames) = node["latency_frames"].as_u64() {
            label.push_str(&format!("\n{frames} frames"));
        }
        if let Some(plugin) = node.get("plugin") {
            let ns = plugin["ns_avg"].as_u64().unwrap_or(0);
            label.push_str(&format!("\n{ns} ns per cycle"));
            if let Some(to) = plugin["converts_to"].as_str() {
                label.push_str(&format!("\nconverted to {to}"));
            }
        }
        let kind = node["kind"].as_str().unwrap_or_default();
        let shape = match kind {
            "bus" => "ellipse",
            _ => "box",
        };
        dot.push_str(&format!(
            "  {} [label={}, shape={shape}];\n",
            quoted(id),
            quoted(&label)
        ));
    }
    for edge in graph["edges"].as_array().unwrap_or(&empty) {
        let from = edge["from"].as_str().unwrap_or_default();
        let to = edge["to"].as_str().unwrap_or_default();
        dot.push_str(&format!("  {} -> {};\n", quoted(from), quoted(to)));
    }
    dot.push_str("}\n");
    dot
}

#[cfg(test)]
mod tests {
    use super::super::tests::{further, play, started, started_with};
    use super::*;
    use crate::api::Call;
    use crate::config::{Config, PluginConfig};
    use crate::control::tests::{Opener, analog_config};
    use std::time::Instant;

    fn plugin(place: PluginPlace, kind: &str) -> PluginConfig {
        PluginConfig {
            place,
            kind: kind.into(),
            path: None,
            params: Default::default(),
        }
    }

    fn ids(graph: &Value) -> Vec<String> {
        let nodes = graph["nodes"].as_array().unwrap();
        nodes
            .iter()
            .map(|n| n["id"].as_str().unwrap().into())
            .collect()
    }

    fn edges(graph: &Value) -> Vec<(String, String)> {
        let edges = graph["edges"].as_array().unwrap();
        let text = |e: &Value, k: &str| e[k].as_str().unwrap().to_string();
        edges
            .iter()
            .map(|e| (text(e, "from"), text(e, "to")))
            .collect()
    }

    /// The edges as a path, when each one leaves where the last arrived.
    fn path(graph: &Value) -> Vec<String> {
        let edges = edges(graph);
        let mut path = vec![edges[0].0.clone()];
        for (from, to) in &edges {
            assert_eq!(from, path.last().unwrap(), "{edges:?}");
            path.push(to.clone());
        }
        path
    }

    fn graph(control: &mut Control, now: Instant) -> Value {
        control.call(Call::Graph, now).unwrap()
    }

    #[test]
    fn the_master_alone() {
        let (mut control, _engine, now) = started(&Config::default(), None);
        play(&mut control, 1, 48000, now);
        let g = graph(&mut control, now);
        let kinds: Vec<&str> = g["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["kind"].as_str().unwrap())
            .collect();
        assert!(kinds.contains(&"client"));
        let edges = edges(&g);
        let names = |ids: &[&str]| -> Vec<(String, String)> {
            ids.windows(2)
                .map(|p| (p[0].to_string(), p[1].to_string()))
                .collect()
        };
        let expected = names(&[
            "bus/players",
            "out/1/matrix",
            "out/1/volume",
            "out/1/delay",
            "out/1/card",
        ]);
        assert!(
            edges
                .iter()
                .any(|e| e.0.starts_with("client/") && e.1 == "bus/players")
        );
        let rest: Vec<_> = edges
            .into_iter()
            .filter(|e| !e.0.starts_with("client/"))
            .collect();
        assert_eq!(rest, expected);
    }

    #[test]
    fn plugins_in_chain_order() {
        let config = Config {
            plugins: vec![
                plugin(PluginPlace::Output, "balance"),
                plugin(PluginPlace::Mix, "balance"),
                plugin(PluginPlace::Mix, "gain"),
            ],
            ..Config::default()
        };
        let (mut control, _engine, now) = started(&config, None);
        let g = graph(&mut control, now);
        assert_eq!(
            path(&g),
            [
                "bus/players",
                "out/1/matrix",
                "out/1/volume",
                "out/1/mix/0",
                "out/1/mix/1",
                "out/1/output/0",
                "out/1/delay",
                "out/1/card",
            ]
        );
        let node = g["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == "out/1/mix/1")
            .unwrap();
        assert_eq!(node["kind"], "plugin");
        assert_eq!(node["plugin"]["id"], "1/mix/1");
        assert_eq!(node["plugin"]["ns_avg"], 0);
        assert_eq!(node["latency_frames"], 0);
    }

    #[test]
    fn the_analog_chain_comes_before_its_bus() {
        let config = Config {
            plugins: vec![plugin(PluginPlace::Analog, "detect")],
            ..analog_config()
        };
        let opener = Opener::default();
        let (mut control, _engine, now) = started(&config, Some(&opener));
        let g = graph(&mut control, now);
        let edges = edges(&g);
        let at = |e: (&str, &str)| edges.iter().position(|x| (x.0.as_str(), x.1.as_str()) == e);
        let chain = at(("analog", "out/1/analog/0")).unwrap();
        let bus = at(("out/1/analog/0", "bus/analog")).unwrap();
        assert_eq!(chain + 1, bus);
        assert!(at(("bus/analog", "out/1/matrix")).is_some());
    }

    #[test]
    fn the_aes67_input_has_its_own_bus() {
        let (mut control, _engine, now) = started(&Config::default(), None);
        assert!(!ids(&graph(&mut control, now)).contains(&"bus/aes67".to_string()));
        control.aes67_input = true;
        let g = graph(&mut control, now);
        for edge in [("aes67", "bus/aes67"), ("bus/aes67", "out/1/matrix")] {
            assert!(
                edges(&g).contains(&(edge.0.into(), edge.1.into())),
                "{edge:?}"
            );
        }
        assert_eq!(node(&g, "aes67")["rate"], 48000);
        let bus = node(&g, "bus/aes67");
        assert_eq!(bus["kind"], "bus");
        assert_eq!(bus["channels"], 2);
        // A matrix that names no `aes67` route still plays it.
        let players = vec![crate::api::RouteJson {
            from: "players.left".into(),
            to: 1,
            gain_db: None,
        }];
        let put = crate::api::MatrixPut {
            outputs: vec![crate::api::OutputRoutes {
                output: 1,
                routes: players,
            }],
        };
        control.call(Call::PutMatrix(put), now).unwrap();
        let g = graph(&mut control, now);
        assert!(edges(&g).contains(&("bus/aes67".into(), "out/1/matrix".into())));
    }

    #[test]
    fn a_further_output_has_no_delay_node() {
        let (mut control, _engine, _logs, now) =
            started_with(&Config::default(), vec![further(&[])]);
        let g = graph(&mut control, now);
        let ids = ids(&g);
        assert!(ids.contains(&"out/1/delay".to_string()));
        assert!(!ids.contains(&"out/2/delay".to_string()));
        for id in ["out/2/matrix", "out/2/volume", "out/2/card"] {
            assert!(ids.contains(&id.to_string()), "{id}");
        }
        assert!(edges(&g).contains(&("out/2/volume".into(), "out/2/card".into())));
        assert!(edges(&g).contains(&("bus/players".into(), "out/2/matrix".into())));
    }

    #[test]
    fn dot_is_a_digraph() {
        let (mut control, _engine, now) = started(&Config::default(), None);
        let g = graph(&mut control, now);
        let dot = to_dot(&g);
        assert!(dot.starts_with("digraph aas {"));
        assert_eq!(dot.matches("->").count(), edges(&g).len());
        for id in ids(&g) {
            assert!(dot.contains(&format!("\"{id}\"")), "{id}");
        }
    }

    fn node<'a>(graph: &'a Value, id: &str) -> &'a Value {
        let nodes = graph["nodes"].as_array().unwrap();
        nodes.iter().find(|n| n["id"] == id).unwrap()
    }

    #[test]
    fn every_node_gives_its_channels_and_format() {
        let config = Config {
            plugins: vec![plugin(PluginPlace::Mix, "balance")],
            ..Config::default()
        };
        let (mut control, _engine, now) = started(&config, None);
        play(&mut control, 1, 48000, now);
        let g = graph(&mut control, now);
        for node in g["nodes"].as_array().unwrap() {
            let id = node["id"].as_str().unwrap();
            assert!(node["channels"].as_u64().unwrap() >= 1, "{id}");
            if node["kind"] != "client" {
                assert_eq!(node["format"], "f32", "{id}");
            }
        }
        assert_eq!(node(&g, "out/1/mix/0")["channels"], 2);
        assert_eq!(node(&g, "out/1/mix/0")["enabled"], true);
        assert_eq!(node(&g, "out/1/volume")["channels"], 2);
        assert_eq!(node(&g, "out/1/card")["format"], "f32");
    }

    #[test]
    fn dot_survives_record_characters_in_a_name() {
        let g = json!({"nodes": [{"id": "client/a{b|c}<d>", "kind": "client",
            "label": "a{b|c}<d>"}], "edges": []});
        let dot = to_dot(&g);
        assert!(dot.contains("label=\"a{b|c}<d>\", shape=box"), "{dot}");
        assert!(!dot.contains("record"));
    }
}
