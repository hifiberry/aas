//! The PulseAudio native protocol's command numbers and reply helpers.

use crate::pulse::tagstruct::Builder;

/// The protocol version this endpoint advertises.
pub const PROTOCOL_VERSION: u32 = 35;

// Client to server.
pub const ERROR: u32 = 0;
pub const REPLY: u32 = 2;
pub const CREATE_PLAYBACK_STREAM: u32 = 3;
pub const DELETE_PLAYBACK_STREAM: u32 = 4;
pub const EXIT: u32 = 7;
pub const AUTH: u32 = 8;
pub const SET_CLIENT_NAME: u32 = 9;
pub const LOOKUP_SINK: u32 = 10;
pub const DRAIN_PLAYBACK_STREAM: u32 = 12;
pub const STAT: u32 = 13;
pub const GET_PLAYBACK_LATENCY: u32 = 14;
pub const GET_SERVER_INFO: u32 = 20;
pub const GET_SINK_INFO: u32 = 21;
pub const GET_SINK_INFO_LIST: u32 = 22;
pub const GET_SOURCE_INFO_LIST: u32 = 24;
pub const GET_MODULE_INFO_LIST: u32 = 26;
pub const GET_CLIENT_INFO: u32 = 27;
pub const GET_CLIENT_INFO_LIST: u32 = 28;
pub const GET_SINK_INPUT_INFO: u32 = 29;
pub const GET_SINK_INPUT_INFO_LIST: u32 = 30;
pub const GET_SAMPLE_INFO_LIST: u32 = 34;
pub const SUBSCRIBE: u32 = 35;
pub const SET_SINK_VOLUME: u32 = 36;
pub const SET_SINK_INPUT_VOLUME: u32 = 37;
pub const SET_SINK_MUTE: u32 = 39;
pub const CORK_PLAYBACK_STREAM: u32 = 41;
pub const FLUSH_PLAYBACK_STREAM: u32 = 42;
pub const TRIGGER_PLAYBACK_STREAM: u32 = 43;
pub const SET_DEFAULT_SINK: u32 = 44;
pub const SET_PLAYBACK_STREAM_NAME: u32 = 46;
pub const PREBUF_PLAYBACK_STREAM: u32 = 60;
pub const SET_SINK_INPUT_MUTE: u32 = 69;
pub const SET_PLAYBACK_STREAM_BUFFER_ATTR: u32 = 72;
pub const UPDATE_PLAYBACK_STREAM_PROPLIST: u32 = 81;
pub const REMOVE_PLAYBACK_STREAM_PROPLIST: u32 = 84;
pub const GET_CARD_INFO_LIST: u32 = 89;

// Server to client.
pub const REQUEST: u32 = 61;
pub const UNDERFLOW: u32 = 63;
pub const PLAYBACK_STREAM_KILLED: u32 = 64;
pub const SUBSCRIBE_EVENT: u32 = 66;
pub const STARTED: u32 = 86;
pub const PLAYBACK_BUFFER_ATTR_CHANGED: u32 = 94;

// Errors.
pub const ERR_ACCESS: u32 = 1;
pub const ERR_COMMAND: u32 = 2;
pub const ERR_INVALID: u32 = 3;
pub const ERR_NOENTITY: u32 = 5;
pub const ERR_PROTOCOL: u32 = 6;
pub const ERR_VERSION: u32 = 17;
pub const ERR_NOTSUPPORTED: u32 = 18;

// Subscription masks and events.
pub const MASK_SINK: u32 = 0x0001;
pub const MASK_SINK_INPUT: u32 = 0x0004;
pub const MASK_CLIENT: u32 = 0x0020;
pub const MASK_SERVER: u32 = 0x0080;
pub const MASK_ALL: u32 = 0x02ff;
pub const EVENT_SINK: u32 = 0;
pub const EVENT_SINK_INPUT: u32 = 2;
pub const EVENT_CLIENT: u32 = 5;
pub const EVENT_SERVER: u32 = 7;
pub const EVENT_NEW: u32 = 0x00;
pub const EVENT_CHANGE: u32 = 0x10;
pub const EVENT_REMOVE: u32 = 0x20;

// Sample formats.
pub const FORMAT_S16LE: u8 = 3;
pub const FORMAT_FLOAT32LE: u8 = 5;
pub const FORMAT_S32LE: u8 = 7;
pub const FORMAT_S24LE: u8 = 9;
pub const FORMAT_S24_32LE: u8 = 11;

/// PulseAudio's unit volume.
pub const VOLUME_NORM: u32 = 0x1_0000;
/// The invalid index.
pub const INVALID_INDEX: u32 = u32::MAX;

/// A reply packet's payload: the reply command, the client's tag, and the
/// body.
pub fn reply(tag: u32, body: Vec<u8>) -> Vec<u8> {
    let mut out = Builder::new();
    out.u32(REPLY);
    out.u32(tag);
    let mut encoded = out.into_vec();
    encoded.extend_from_slice(&body);
    encoded
}

/// An error packet's payload.
pub fn error(tag: u32, code: u32, message: &str) -> Vec<u8> {
    let mut body = Builder::new();
    body.u32(code);
    body.string(message);
    reply_as_error(tag, body.into_vec())
}

fn reply_as_error(tag: u32, body: Vec<u8>) -> Vec<u8> {
    let mut out = Builder::new();
    out.u32(ERROR);
    out.u32(tag);
    let mut encoded = out.into_vec();
    encoded.extend_from_slice(&body);
    encoded
}

/// A subscription event, which is a packet and not a reply.
pub fn subscribe_event(event_type: u32, index: u32) -> Vec<u8> {
    let mut out = Builder::new();
    out.u32(SUBSCRIBE_EVENT);
    out.u32(INVALID_INDEX);
    out.u32(event_type);
    out.u32(index);
    out.into_vec()
}

/// A request for more audio, from the server.
pub fn request(stream: u32, length: u32) -> Vec<u8> {
    let mut out = Builder::new();
    out.u32(REQUEST);
    out.u32(INVALID_INDEX);
    out.u32(stream);
    out.u32(length);
    out.into_vec()
}

pub fn underflow(stream: u32) -> Vec<u8> {
    let mut out = Builder::new();
    out.u32(UNDERFLOW);
    out.u32(INVALID_INDEX);
    out.u32(stream);
    out.into_vec()
}

pub fn killed(stream: u32) -> Vec<u8> {
    let mut out = Builder::new();
    out.u32(PLAYBACK_STREAM_KILLED);
    out.u32(INVALID_INDEX);
    out.u32(stream);
    out.into_vec()
}
