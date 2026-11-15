//! The replies for the one sink, its clients and this daemon's fixed
//! objects.

use crate::pulse::registry::SinkInput;
use crate::pulse::volume;
use crate::pulse::tagstruct::{Builder, SampleSpec};
use crate::pulse::wire;

/// The sink's sample format as clients see it.
pub const SINK_FORMAT: SampleSpec = SampleSpec {
    format: wire::FORMAT_S32LE,
    channels: 2,
    rate: 48_000,
};

pub fn server_info(rate: u32, user: &str, host: &str) -> Vec<u8> {
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
    b.null_string();
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
    b.u32(wire::INVALID_INDEX);
    b.null_string();
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
    b.u8(0);
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
    let cvolume = volume::cvolume_from_db(input.db_millis);
    b.cvolume(&cvolume[..usize::from(input.channels).min(2)]);
    b.usec(0);
    b.usec(0);
    b.string("null");
    b.string("aas");
    b.boolean(input.mute);
    b.proplist(&[("media.name".into(), input.name.clone())]);
    b.boolean(input.corked);
    b.boolean(true);
    b.boolean(true);
    b.format_info(0, &[]);
    b.into_vec()
}
