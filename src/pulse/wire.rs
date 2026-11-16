//! The PulseAudio native protocol's command numbers and reply helpers.

use crate::pulse::tagstruct::Builder;

/// The protocol version this endpoint advertises.
pub const PROTOCOL_VERSION: u32 = 35;

// Client to server.
pub const ERROR: u32 = 0;
pub const REPLY: u32 = 2;
pub const CREATE_PLAYBACK_STREAM: u32 = 3;
pub const DELETE_PLAYBACK_STREAM: u32 = 4;
pub const CREATE_RECORD_STREAM: u32 = 5;
pub const DELETE_RECORD_STREAM: u32 = 6;
pub const EXIT: u32 = 7;
pub const AUTH: u32 = 8;
pub const SET_CLIENT_NAME: u32 = 9;
pub const LOOKUP_SINK: u32 = 10;
pub const LOOKUP_SOURCE: u32 = 11;
pub const DRAIN_PLAYBACK_STREAM: u32 = 12;
pub const STAT: u32 = 13;
pub const GET_PLAYBACK_LATENCY: u32 = 14;
pub const GET_SERVER_INFO: u32 = 20;
pub const GET_SINK_INFO: u32 = 21;
pub const GET_SINK_INFO_LIST: u32 = 22;
pub const GET_SOURCE_INFO: u32 = 23;
pub const GET_SOURCE_INFO_LIST: u32 = 24;
pub const GET_MODULE_INFO_LIST: u32 = 26;
pub const GET_CLIENT_INFO: u32 = 27;
pub const GET_CLIENT_INFO_LIST: u32 = 28;
pub const GET_SINK_INPUT_INFO: u32 = 29;
pub const GET_SINK_INPUT_INFO_LIST: u32 = 30;
pub const GET_SOURCE_OUTPUT_INFO: u32 = 31;
pub const GET_SOURCE_OUTPUT_INFO_LIST: u32 = 32;
pub const GET_SAMPLE_INFO_LIST: u32 = 34;
pub const SUBSCRIBE: u32 = 35;
pub const SET_SINK_VOLUME: u32 = 36;
pub const SET_SINK_INPUT_VOLUME: u32 = 37;
pub const SET_SOURCE_VOLUME: u32 = 38;
pub const SET_SINK_MUTE: u32 = 39;
pub const SET_SOURCE_MUTE: u32 = 40;
pub const CORK_PLAYBACK_STREAM: u32 = 41;
pub const FLUSH_PLAYBACK_STREAM: u32 = 42;
pub const TRIGGER_PLAYBACK_STREAM: u32 = 43;
pub const SET_DEFAULT_SINK: u32 = 44;
pub const SET_DEFAULT_SOURCE: u32 = 45;
pub const SET_PLAYBACK_STREAM_NAME: u32 = 46;
pub const SET_RECORD_STREAM_NAME: u32 = 47;
pub const GET_RECORD_LATENCY: u32 = 57;
pub const CORK_RECORD_STREAM: u32 = 58;
pub const FLUSH_RECORD_STREAM: u32 = 59;
pub const PREBUF_PLAYBACK_STREAM: u32 = 60;
pub const SET_SINK_INPUT_MUTE: u32 = 69;
pub const SET_PLAYBACK_STREAM_BUFFER_ATTR: u32 = 72;
pub const SET_RECORD_STREAM_BUFFER_ATTR: u32 = 73;
pub const UPDATE_RECORD_STREAM_PROPLIST: u32 = 80;
pub const UPDATE_PLAYBACK_STREAM_PROPLIST: u32 = 81;
pub const REMOVE_RECORD_STREAM_PROPLIST: u32 = 83;
pub const REMOVE_PLAYBACK_STREAM_PROPLIST: u32 = 84;
pub const GET_CARD_INFO_LIST: u32 = 89;
pub const SET_SOURCE_OUTPUT_VOLUME: u32 = 98;
pub const SET_SOURCE_OUTPUT_MUTE: u32 = 99;

// Server to client.
pub const REQUEST: u32 = 61;
pub const OVERFLOW: u32 = 62;
pub const UNDERFLOW: u32 = 63;
pub const PLAYBACK_STREAM_KILLED: u32 = 64;
pub const RECORD_STREAM_KILLED: u32 = 65;
pub const SUBSCRIBE_EVENT: u32 = 66;
pub const STARTED: u32 = 86;
pub const PLAYBACK_BUFFER_ATTR_CHANGED: u32 = 94;

// Errors.
pub const ERR_ACCESS: u32 = 1;
pub const ERR_COMMAND: u32 = 2;
pub const ERR_INVALID: u32 = 3;
pub const ERR_NOENTITY: u32 = 5;
pub const ERR_PROTOCOL: u32 = 7;
pub const ERR_VERSION: u32 = 17;
pub const ERR_INTERNAL: u32 = 10;
pub const ERR_NOTSUPPORTED: u32 = 19;

// Subscription masks and events.
pub const MASK_SINK: u32 = 0x0001;
pub const MASK_SOURCE: u32 = 0x0002;
pub const MASK_SINK_INPUT: u32 = 0x0004;
pub const MASK_SOURCE_OUTPUT: u32 = 0x0008;
pub const MASK_CLIENT: u32 = 0x0020;
pub const MASK_SERVER: u32 = 0x0080;
pub const MASK_ALL: u32 = 0x02ff;
pub const EVENT_SINK: u32 = 0;
pub const EVENT_SOURCE: u32 = 1;
pub const EVENT_SINK_INPUT: u32 = 2;
pub const EVENT_SOURCE_OUTPUT: u32 = 3;
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

/// `PA_ENCODING_PCM`, the only encoding this endpoint plays.
pub const ENCODING_PCM: u8 = 1;

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
pub fn error(tag: u32, code: u32) -> Vec<u8> {
    let mut body = Builder::new();
    body.u32(code);
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

pub fn underflow(stream: u32, offset: i64) -> Vec<u8> {
    let mut out = Builder::new();
    out.u32(UNDERFLOW);
    out.u32(INVALID_INDEX);
    out.u32(stream);
    out.i64(offset);
    out.into_vec()
}

/// The stream has started playing: sent once its first data is in.
pub fn started(stream: u32) -> Vec<u8> {
    let mut out = Builder::new();
    out.u32(STARTED);
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

/// A record stream's buffer ran full and data was dropped.
pub fn overflow(stream: u32) -> Vec<u8> {
    let mut out = Builder::new();
    out.u32(OVERFLOW);
    out.u32(INVALID_INDEX);
    out.u32(stream);
    out.into_vec()
}

pub fn record_killed(stream: u32) -> Vec<u8> {
    let mut out = Builder::new();
    out.u32(RECORD_STREAM_KILLED);
    out.u32(INVALID_INDEX);
    out.u32(stream);
    out.into_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pulse::tagstruct::Builder;

    #[test]
    fn error_codes_are_pulseaudios() {
        // pa_error_code in PulseAudio's def.h.
        assert_eq!(
            [ERR_ACCESS, ERR_COMMAND, ERR_INVALID, ERR_NOENTITY, ERR_PROTOCOL],
            [1, 2, 3, 5, 7]
        );
        assert_eq!([ERR_INTERNAL, ERR_VERSION, ERR_NOTSUPPORTED], [10, 17, 19]);
    }

    #[test]
    fn record_and_source_numbers_are_pulseaudios() {
        // The command enum in PulseAudio's native-common.h.
        assert_eq!(
            [
                CREATE_RECORD_STREAM,
                DELETE_RECORD_STREAM,
                LOOKUP_SOURCE,
                GET_SOURCE_INFO
            ],
            [5, 6, 11, 23]
        );
        assert_eq!(
            [
                GET_SOURCE_OUTPUT_INFO,
                GET_SOURCE_OUTPUT_INFO_LIST,
                SET_SOURCE_VOLUME,
                SET_SOURCE_MUTE
            ],
            [31, 32, 38, 40]
        );
        assert_eq!(
            [
                SET_DEFAULT_SOURCE,
                SET_RECORD_STREAM_NAME,
                GET_RECORD_LATENCY,
                CORK_RECORD_STREAM
            ],
            [45, 47, 57, 58]
        );
        assert_eq!(
            [FLUSH_RECORD_STREAM, OVERFLOW, RECORD_STREAM_KILLED],
            [59, 62, 65]
        );
        assert_eq!(
            [
                SET_RECORD_STREAM_BUFFER_ATTR,
                UPDATE_RECORD_STREAM_PROPLIST,
                REMOVE_RECORD_STREAM_PROPLIST,
            ],
            [73, 80, 83]
        );
        assert_eq!([SET_SOURCE_OUTPUT_VOLUME, SET_SOURCE_OUTPUT_MUTE], [98, 99]);
        // pa_subscription_mask and pa_subscription_event_type in def.h.
        assert_eq!([MASK_SOURCE, MASK_SOURCE_OUTPUT], [0x0002, 0x0008]);
        assert_eq!([EVENT_SOURCE, EVENT_SOURCE_OUTPUT], [1, 3]);
        assert_eq!(
            MASK_ALL & (MASK_SOURCE | MASK_SOURCE_OUTPUT),
            MASK_SOURCE | MASK_SOURCE_OUTPUT
        );
    }

    #[test]
    fn an_overflow_and_a_record_kill_name_their_stream() {
        let mut expected = Builder::new();
        expected.u32(OVERFLOW);
        expected.u32(INVALID_INDEX);
        expected.u32(4);
        assert_eq!(overflow(4), expected.into_vec());
        let mut expected = Builder::new();
        expected.u32(RECORD_STREAM_KILLED);
        expected.u32(INVALID_INDEX);
        expected.u32(4);
        assert_eq!(record_killed(4), expected.into_vec());
    }

    #[test]
    fn an_error_ends_after_its_code() {
        let bytes = error(7, ERR_INVALID);
        let mut expected = Builder::new();
        expected.u32(ERROR);
        expected.u32(7);
        expected.u32(ERR_INVALID);
        assert_eq!(bytes, expected.into_vec());
    }

    #[test]
    fn an_underflow_carries_its_offset() {
        let bytes = underflow(3, -1);
        let mut expected = Builder::new();
        expected.u32(UNDERFLOW);
        expected.u32(INVALID_INDEX);
        expected.u32(3);
        expected.i64(-1);
        assert_eq!(bytes, expected.into_vec());
    }
}
