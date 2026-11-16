//! The replies for the one sink, its clients and this daemon's fixed
//! objects.

use crate::pulse::registry::{SinkInput, SourceOutput};
use crate::pulse::volume;
use crate::pulse::tagstruct::{Builder, SampleSpec};
use crate::pulse::wire;

/// The sink's sample format as clients see it.
pub const SINK_FORMAT: SampleSpec = SampleSpec {
    format: wire::FORMAT_S32LE,
    channels: 2,
    rate: 48_000,
};

/// A source: what the master card gets, or the analog input when the
/// config has one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Monitor,
    Analog,
}

impl Source {
    pub fn index(self) -> u32 {
        match self {
            Source::Monitor => 0,
            Source::Analog => 1,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Source::Monitor => "aas.monitor",
            Source::Analog => "aas.analog",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Source::Monitor => "Monitor of aas",
            Source::Analog => "aas analog input",
        }
    }
}

/// The sources there are: the monitor, and the analog input if `analog`.
pub fn sources(analog: bool) -> Vec<Source> {
    if analog {
        vec![Source::Monitor, Source::Analog]
    } else {
        vec![Source::Monitor]
    }
}

/// The source an index and name pick. A valid index decides alone;
/// otherwise the name does, and no name means the default source.
pub fn find_source(
    index: Option<u32>,
    name: Option<&str>,
    default: Source,
    analog: bool,
) -> Option<Source> {
    let found = match index {
        Some(index) if index != wire::INVALID_INDEX => sources(analog)
            .into_iter()
            .find(|source| source.index() == index),
        _ => match name {
            None | Some("@DEFAULT_SOURCE@") => Some(default),
            Some("@DEFAULT_MONITOR@") => Some(Source::Monitor),
            Some(name) => sources(analog)
                .into_iter()
                .find(|source| source.name() == name),
        },
    };
    found.filter(|&source| source == Source::Monitor || analog)
}

pub fn server_info(rate: u32, user: &str, host: &str, default_source: Source) -> Vec<u8> {
    let mut b = Builder::new();
    b.string("aas");
    b.string("0.1.0");
    b.string(user);
    b.string(host);
    b.sample_spec(SampleSpec {
        rate,
        ..SINK_FORMAT
    });
    b.string("aas");
    b.string(default_source.name());
    b.u32(0);
    b.channel_map(&[1, 2]);
    b.into_vec()
}

pub fn sink_info(rate: u32, volume: u8, mute: bool, latency_usec: u64, state: u32) -> Vec<u8> {
    let mut b = Builder::new();
    b.u32(0);
    b.string("aas");
    b.string("aas output");
    b.sample_spec(SampleSpec {
        rate,
        ..SINK_FORMAT
    });
    b.channel_map(&[1, 2]);
    b.u32(wire::INVALID_INDEX);
    let level = (u32::from(volume) * wire::VOLUME_NORM + 50) / 100;
    b.cvolume(&[level; 2]);
    b.boolean(mute);
    b.u32(Source::Monitor.index());
    b.string(Source::Monitor.name());
    b.usec(latency_usec);
    b.string("aas");
    b.u32(0);
    b.proplist(&[("device.description".into(), "aas output".into())]);
    b.usec(0);
    b.volume(wire::VOLUME_NORM);
    b.u32(state);
    b.u32(100);
    b.u32(wire::INVALID_INDEX);
    b.u32(0);
    b.null_string();
    // libpulse refuses a sink with no formats; PulseAudio's own sinks list
    // one, plain PCM.
    b.u8(1);
    b.format_info(wire::ENCODING_PCM, &[]);
    b.into_vec()
}

/// `PA_SOURCE_LATENCY` and `PA_SOURCE_HARDWARE` in PulseAudio's def.h.
const SOURCE_LATENCY: u32 = 0x2;
const SOURCE_HARDWARE: u32 = 0x4;

/// A source's info body, in the order of `source_fill_tagstruct` for
/// protocol 35. Its volume is fixed at unity and it is never muted.
pub fn source_info(source: Source, rate: u32, latency_usec: u64) -> Vec<u8> {
    let mut b = Builder::new();
    b.u32(source.index());
    b.string(source.name());
    b.string(source.description());
    b.sample_spec(SampleSpec {
        rate,
        ..SINK_FORMAT
    });
    b.channel_map(&[1, 2]);
    b.u32(wire::INVALID_INDEX);
    b.cvolume(&[wire::VOLUME_NORM; 2]);
    b.boolean(false);
    let (flags, class) = match source {
        Source::Monitor => {
            b.u32(0);
            b.string("aas");
            (SOURCE_LATENCY, "monitor")
        }
        Source::Analog => {
            b.u32(wire::INVALID_INDEX);
            b.null_string();
            (SOURCE_LATENCY | SOURCE_HARDWARE, "sound")
        }
    };
    b.usec(latency_usec);
    b.string("aas");
    b.u32(flags);
    b.proplist(&[
        ("device.description".into(), source.description().into()),
        ("device.class".into(), class.into()),
    ]);
    b.usec(latency_usec);
    b.volume(wire::VOLUME_NORM);
    b.u32(0);
    b.u32(wire::VOLUME_NORM + 1);
    b.u32(wire::INVALID_INDEX);
    b.u32(0);
    b.null_string();
    b.u8(1);
    b.format_info(wire::ENCODING_PCM, &[]);
    b.into_vec()
}

pub fn client_info(index: u32, name: &str) -> Vec<u8> {
    let mut b = Builder::new();
    b.u32(index);
    b.string(name);
    b.u32(wire::INVALID_INDEX);
    b.string("aas");
    b.proplist(&[("application.name".into(), name.into())]);
    b.into_vec()
}

pub fn sink_input_info(input: &SinkInput) -> Vec<u8> {
    let mut b = Builder::new();
    b.u32(input.index);
    b.string(&input.name);
    b.u32(wire::INVALID_INDEX);
    b.u32(input.client);
    b.u32(0);
    b.sample_spec(SampleSpec {
        format: input.format,
        channels: input.channels,
        rate: input.rate,
    });
    let map: &[u8] = if input.channels == 1 { &[0] } else { &[1, 2] };
    b.channel_map(map);
    let cvolume = if input.silent {
        [0; 2]
    } else {
        volume::cvolume_from_db(input.db_millis)
    };
    b.cvolume(&cvolume[..usize::from(input.channels).min(2)]);
    b.usec(input.buffer_usec);
    b.usec(input.sink_usec);
    b.string("null");
    b.string("aas");
    b.boolean(input.mute);
    b.proplist(&[("media.name".into(), input.name.clone())]);
    b.boolean(input.corked);
    b.boolean(true);
    b.boolean(true);
    let spec = SampleSpec {
        format: input.format,
        channels: input.channels,
        rate: input.rate,
    };
    b.format_info(
        wire::ENCODING_PCM,
        &crate::pulse::stream::pcm_props(spec, map),
    );
    b.into_vec()
}

/// A record stream's info body, in the order of
/// `source_output_fill_tagstruct` for protocol 35. Its volume is fixed at
/// unity and it is never muted.
pub fn source_output_info(output: &SourceOutput) -> Vec<u8> {
    let spec = SampleSpec {
        format: output.format,
        channels: output.channels,
        rate: output.rate,
    };
    let map: &[u8] = if output.channels == 1 { &[0] } else { &[1, 2] };
    let mut b = Builder::new();
    b.u32(output.index);
    b.string(&output.name);
    b.u32(wire::INVALID_INDEX);
    b.u32(output.client);
    b.u32(output.source.index());
    b.sample_spec(spec);
    b.channel_map(map);
    b.usec(output.buffer_usec);
    b.usec(output.source_usec);
    b.string(if output.resampled { "aas" } else { "trivial" });
    b.string("aas");
    b.proplist(&[("media.name".into(), output.name.clone())]);
    b.boolean(output.corked);
    b.cvolume(&vec![wire::VOLUME_NORM; usize::from(output.channels)]);
    b.boolean(false);
    b.boolean(true);
    b.boolean(false);
    b.format_info(
        wire::ENCODING_PCM,
        &crate::pulse::stream::pcm_props(spec, map),
    );
    b.into_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_an_analog_input_it_is_the_default_and_the_monitor_stays_named() {
        let found = |index, name| find_source(index, name, Source::Analog, true);
        assert_eq!(found(None, None), Some(Source::Analog));
        assert_eq!(found(None, Some("@DEFAULT_SOURCE@")), Some(Source::Analog));
        assert_eq!(
            found(None, Some("@DEFAULT_MONITOR@")),
            Some(Source::Monitor)
        );
        assert_eq!(found(Some(1), None), Some(Source::Analog));
        assert_eq!(found(Some(2), None), None);
        assert_eq!(
            found(Some(wire::INVALID_INDEX), Some("aas.monitor")),
            Some(Source::Monitor)
        );
        assert_eq!(sources(true), vec![Source::Monitor, Source::Analog]);
    }

    #[test]
    fn without_an_analog_input_there_is_only_the_monitor() {
        assert_eq!(find_source(Some(1), None, Source::Monitor, false), None);
        assert_eq!(
            find_source(None, Some("aas.analog"), Source::Monitor, false),
            None
        );
        assert_eq!(sources(false), vec![Source::Monitor]);
    }
}
