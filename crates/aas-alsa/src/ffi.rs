//! The parts of libasound's external plugin SDK this plugin uses, declared
//! by hand from `alsa/pcm_ioplug.h`, `alsa/pcm.h` and `alsa/conf.h`.
//! `alsa-sys` does not cover the ioplug SDK. `tests/layout.rs` compares
//! every size, offset and constant here with the installed headers.

#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_int, c_long, c_uint, c_ulong, c_ushort, c_void};

pub type snd_pcm_uframes_t = c_ulong;
pub type snd_pcm_sframes_t = c_long;
pub type snd_pcm_stream_t = c_int;
pub type snd_pcm_state_t = c_int;
pub type snd_pcm_access_t = c_int;
pub type snd_pcm_format_t = c_int;

/// Opaque libasound types, only ever behind a pointer.
#[repr(C)]
pub struct snd_pcm_t {
    _private: [u8; 0],
}
#[repr(C)]
pub struct snd_pcm_hw_params_t {
    _private: [u8; 0],
}
#[repr(C)]
pub struct snd_pcm_sw_params_t {
    _private: [u8; 0],
}
#[repr(C)]
pub struct snd_output_t {
    _private: [u8; 0],
}
#[repr(C)]
pub struct snd_config_t {
    _private: [u8; 0],
}
#[repr(C)]
pub struct snd_config_iterator {
    _private: [u8; 0],
}
pub type snd_config_iterator_t = *mut snd_config_iterator;
#[repr(C)]
pub struct snd_pcm_chmap_query_t {
    _private: [u8; 0],
}
#[repr(C)]
pub struct snd_pcm_chmap_t {
    _private: [u8; 0],
}

pub const SND_PCM_IOPLUG_VERSION: c_uint = 0x010002;
/// The hw pointer wraps at the boundary, not at the buffer size.
pub const SND_PCM_IOPLUG_FLAG_BOUNDARY_WA: c_uint = 1 << 2;

pub const SND_PCM_IOPLUG_HW_ACCESS: c_int = 0;
pub const SND_PCM_IOPLUG_HW_FORMAT: c_int = 1;
pub const SND_PCM_IOPLUG_HW_CHANNELS: c_int = 2;
pub const SND_PCM_IOPLUG_HW_RATE: c_int = 3;
pub const SND_PCM_IOPLUG_HW_PERIOD_BYTES: c_int = 4;
pub const SND_PCM_IOPLUG_HW_BUFFER_BYTES: c_int = 5;
pub const SND_PCM_IOPLUG_HW_PERIODS: c_int = 6;

pub const SND_PCM_STREAM_PLAYBACK: snd_pcm_stream_t = 0;

pub const SND_PCM_STATE_PREPARED: snd_pcm_state_t = 2;
pub const SND_PCM_STATE_RUNNING: snd_pcm_state_t = 3;
pub const SND_PCM_STATE_XRUN: snd_pcm_state_t = 4;
pub const SND_PCM_STATE_PAUSED: snd_pcm_state_t = 6;
pub const SND_PCM_STATE_DISCONNECTED: snd_pcm_state_t = 8;

pub const SND_PCM_ACCESS_MMAP_INTERLEAVED: snd_pcm_access_t = 0;
pub const SND_PCM_ACCESS_RW_INTERLEAVED: snd_pcm_access_t = 3;

pub const SND_PCM_FORMAT_S16_LE: snd_pcm_format_t = 2;
pub const SND_PCM_FORMAT_S24_LE: snd_pcm_format_t = 6;
pub const SND_PCM_FORMAT_S32_LE: snd_pcm_format_t = 10;
pub const SND_PCM_FORMAT_FLOAT_LE: snd_pcm_format_t = 14;
pub const SND_PCM_FORMAT_S24_3LE: snd_pcm_format_t = 32;

/// One channel's samples: the first at bit `first` from `addr`, the next
/// `step` bits on.
#[repr(C)]
pub struct snd_pcm_channel_area_t {
    pub addr: *mut c_void,
    pub first: c_uint,
    pub step: c_uint,
}

/// The plugin's handle. The plugin fills the first fields before
/// `snd_pcm_ioplug_create`; libasound fills and updates the rest.
#[repr(C)]
pub struct snd_pcm_ioplug_t {
    pub version: c_uint,
    pub name: *const c_char,
    pub flags: c_uint,
    pub poll_fd: c_int,
    pub poll_events: c_uint,
    pub mmap_rw: c_uint,
    pub callback: *const snd_pcm_ioplug_callback_t,
    pub private_data: *mut c_void,
    pub pcm: *mut snd_pcm_t,
    pub stream: snd_pcm_stream_t,
    pub state: snd_pcm_state_t,
    pub appl_ptr: snd_pcm_uframes_t,
    pub hw_ptr: snd_pcm_uframes_t,
    pub nonblock: c_int,
    pub access: snd_pcm_access_t,
    pub format: snd_pcm_format_t,
    pub channels: c_uint,
    pub rate: c_uint,
    pub period_size: snd_pcm_uframes_t,
    pub buffer_size: snd_pcm_uframes_t,
}

type Io = *mut snd_pcm_ioplug_t;

#[repr(C)]
pub struct snd_pcm_ioplug_callback_t {
    pub start: Option<unsafe extern "C" fn(Io) -> c_int>,
    pub stop: Option<unsafe extern "C" fn(Io) -> c_int>,
    pub pointer: Option<unsafe extern "C" fn(Io) -> snd_pcm_sframes_t>,
    pub transfer: Option<
        unsafe extern "C" fn(
            Io,
            *const snd_pcm_channel_area_t,
            snd_pcm_uframes_t,
            snd_pcm_uframes_t,
        ) -> snd_pcm_sframes_t,
    >,
    pub close: Option<unsafe extern "C" fn(Io) -> c_int>,
    pub hw_params: Option<unsafe extern "C" fn(Io, *mut snd_pcm_hw_params_t) -> c_int>,
    pub hw_free: Option<unsafe extern "C" fn(Io) -> c_int>,
    pub sw_params: Option<unsafe extern "C" fn(Io, *mut snd_pcm_sw_params_t) -> c_int>,
    pub prepare: Option<unsafe extern "C" fn(Io) -> c_int>,
    pub drain: Option<unsafe extern "C" fn(Io) -> c_int>,
    pub pause: Option<unsafe extern "C" fn(Io, c_int) -> c_int>,
    pub resume: Option<unsafe extern "C" fn(Io) -> c_int>,
    pub poll_descriptors_count: Option<unsafe extern "C" fn(Io) -> c_int>,
    pub poll_descriptors: Option<unsafe extern "C" fn(Io, *mut libc::pollfd, c_uint) -> c_int>,
    pub poll_revents:
        Option<unsafe extern "C" fn(Io, *mut libc::pollfd, c_uint, *mut c_ushort) -> c_int>,
    pub dump: Option<unsafe extern "C" fn(Io, *mut snd_output_t)>,
    pub delay: Option<unsafe extern "C" fn(Io, *mut snd_pcm_sframes_t) -> c_int>,
    pub query_chmaps: Option<unsafe extern "C" fn(Io) -> *mut *mut snd_pcm_chmap_query_t>,
    pub get_chmap: Option<unsafe extern "C" fn(Io) -> *mut snd_pcm_chmap_t>,
    pub set_chmap: Option<unsafe extern "C" fn(Io, *const snd_pcm_chmap_t) -> c_int>,
}

#[link(name = "asound")]
unsafe extern "C" {
    pub fn snd_pcm_ioplug_create(
        io: Io,
        name: *const c_char,
        stream: snd_pcm_stream_t,
        mode: c_int,
    ) -> c_int;
    pub fn snd_pcm_ioplug_delete(io: Io) -> c_int;
    pub fn snd_pcm_ioplug_set_param_minmax(io: Io, kind: c_int, min: c_uint, max: c_uint) -> c_int;
    pub fn snd_pcm_ioplug_set_param_list(
        io: Io,
        kind: c_int,
        num_list: c_uint,
        list: *const c_uint,
    ) -> c_int;
    pub fn snd_pcm_ioplug_set_state(io: Io, state: snd_pcm_state_t) -> c_int;

    pub fn snd_pcm_sw_params_malloc(params: *mut *mut snd_pcm_sw_params_t) -> c_int;
    pub fn snd_pcm_sw_params_free(params: *mut snd_pcm_sw_params_t);
    pub fn snd_pcm_sw_params_current(
        pcm: *mut snd_pcm_t,
        params: *mut snd_pcm_sw_params_t,
    ) -> c_int;
    pub fn snd_pcm_sw_params_get_boundary(
        params: *const snd_pcm_sw_params_t,
        val: *mut snd_pcm_uframes_t,
    ) -> c_int;

    pub fn snd_config_iterator_first(node: *const snd_config_t) -> snd_config_iterator_t;
    pub fn snd_config_iterator_next(iterator: snd_config_iterator_t) -> snd_config_iterator_t;
    pub fn snd_config_iterator_end(node: *const snd_config_t) -> snd_config_iterator_t;
    pub fn snd_config_iterator_entry(iterator: snd_config_iterator_t) -> *mut snd_config_t;
    pub fn snd_config_get_id(config: *const snd_config_t, value: *mut *const c_char) -> c_int;
}
