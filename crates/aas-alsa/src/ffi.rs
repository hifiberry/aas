//! The parts of libasound's external plugin SDKs this plugin uses, declared
//! by hand from `alsa/pcm_ioplug.h`, `alsa/control_external.h`,
//! `alsa/pcm.h`, `alsa/control.h` and `alsa/conf.h`. `alsa-sys` covers
//! neither the ioplug nor the ctl_ext SDK. `tests/layout.rs` compares
//! every size, offset and constant here with the installed headers.

#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_int, c_long, c_uchar, c_uint, c_ulong, c_ushort, c_void};

/// Declares a struct or union of function pointers, with `SIGNATURES`:
/// each field's name and its type as written. `tests/layout.rs` turns the
/// types into C and has the compiler compare them with the headers'.
macro_rules! with_signatures {
    (
        $(#[$meta:meta])*
        pub $kind:tt $name:ident { $(pub $field:ident: $type:ty,)* }
    ) => {
        $(#[$meta])*
        pub $kind $name { $(pub $field: $type,)* }

        impl $name {
            pub const SIGNATURES: &[(&str, &str)] = &[$((stringify!($field), stringify!($type))),*];
        }
    };
}

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
/// The open mode bit for a non-blocking PCM.
pub const SND_PCM_NONBLOCK: c_int = 1;

pub const SND_PCM_STATE_SETUP: snd_pcm_state_t = 1;
pub const SND_PCM_STATE_PREPARED: snd_pcm_state_t = 2;
pub const SND_PCM_STATE_RUNNING: snd_pcm_state_t = 3;
pub const SND_PCM_STATE_XRUN: snd_pcm_state_t = 4;
pub const SND_PCM_STATE_DRAINING: snd_pcm_state_t = 5;
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

with_signatures! {
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
}

pub type snd_ctl_ext_key_t = c_ulong;
pub type snd_ctl_elem_type_t = c_int;
pub type snd_ctl_elem_iface_t = c_int;

#[repr(C)]
pub struct snd_ctl_t {
    _private: [u8; 0],
}
#[repr(C)]
pub struct snd_ctl_elem_id_t {
    _private: [u8; 0],
}
#[repr(C)]
pub struct snd_aes_iec958_t {
    _private: [u8; 0],
}

/// Protocol 1.0.1, the first with TLV data.
pub const SND_CTL_EXT_VERSION: c_uint = 0x010001;
pub const SND_CTL_EXT_ACCESS_READ: c_uint = 1 << 0;
pub const SND_CTL_EXT_ACCESS_WRITE: c_uint = 1 << 1;
pub const SND_CTL_EXT_ACCESS_READWRITE: c_uint = 3;
pub const SND_CTL_EXT_ACCESS_TLV_READ: c_uint = 1 << 4;
/// What `find_elem` returns for an element the plugin does not have.
pub const SND_CTL_EXT_KEY_NOT_FOUND: snd_ctl_ext_key_t = !0;

pub const SND_CTL_ELEM_TYPE_INTEGER: snd_ctl_elem_type_t = 2;
pub const SND_CTL_ELEM_IFACE_MIXER: snd_ctl_elem_iface_t = 2;
pub const SND_CTL_EVENT_MASK_VALUE: c_uint = 1 << 0;
/// A dB range from a minimum to a maximum, linear in between, whose
/// lowest value mutes. In 0.01 dB.
pub const SND_CTL_TLVT_DB_MINMAX_MUTE: c_uint = 5;

type Ext = *mut snd_ctl_ext_t;

with_signatures! {
/// The TLV data of the plugin's elements: a callback or fixed data.
#[repr(C)]
pub union snd_ctl_ext_tlv {
    pub c: Option<
        unsafe extern "C" fn(Ext, snd_ctl_ext_key_t, c_int, c_uint, *mut c_uint, c_uint) -> c_int,
    >,
    pub p: *const c_uint,
}
}

/// The control plugin's handle. The plugin fills it before
/// `snd_ctl_ext_create`, which sets `handle`.
#[repr(C)]
pub struct snd_ctl_ext_t {
    pub version: c_uint,
    pub card_idx: c_int,
    pub id: [c_char; 16],
    pub driver: [c_char; 16],
    pub name: [c_char; 32],
    pub longname: [c_char; 80],
    pub mixername: [c_char; 80],
    pub poll_fd: c_int,
    pub callback: *const snd_ctl_ext_callback_t,
    pub private_data: *mut c_void,
    pub handle: *mut snd_ctl_t,
    pub nonblock: c_int,
    pub subscribed: c_int,
    pub tlv: snd_ctl_ext_tlv,
}

type Key = snd_ctl_ext_key_t;

with_signatures! {
#[repr(C)]
pub struct snd_ctl_ext_callback_t {
    pub close: Option<unsafe extern "C" fn(Ext)>,
    pub elem_count: Option<unsafe extern "C" fn(Ext) -> c_int>,
    pub elem_list: Option<unsafe extern "C" fn(Ext, c_uint, *mut snd_ctl_elem_id_t) -> c_int>,
    pub find_elem: Option<unsafe extern "C" fn(Ext, *const snd_ctl_elem_id_t) -> Key>,
    pub free_key: Option<unsafe extern "C" fn(Ext, Key)>,
    pub get_attribute:
        Option<unsafe extern "C" fn(Ext, Key, *mut c_int, *mut c_uint, *mut c_uint) -> c_int>,
    pub get_integer_info:
        Option<unsafe extern "C" fn(Ext, Key, *mut c_long, *mut c_long, *mut c_long) -> c_int>,
    pub get_integer64_info:
        Option<unsafe extern "C" fn(Ext, Key, *mut i64, *mut i64, *mut i64) -> c_int>,
    pub get_enumerated_info: Option<unsafe extern "C" fn(Ext, Key, *mut c_uint) -> c_int>,
    pub get_enumerated_name:
        Option<unsafe extern "C" fn(Ext, Key, c_uint, *mut c_char, usize) -> c_int>,
    pub read_integer: Option<unsafe extern "C" fn(Ext, Key, *mut c_long) -> c_int>,
    pub read_integer64: Option<unsafe extern "C" fn(Ext, Key, *mut i64) -> c_int>,
    pub read_enumerated: Option<unsafe extern "C" fn(Ext, Key, *mut c_uint) -> c_int>,
    pub read_bytes: Option<unsafe extern "C" fn(Ext, Key, *mut c_uchar, usize) -> c_int>,
    pub read_iec958: Option<unsafe extern "C" fn(Ext, Key, *mut snd_aes_iec958_t) -> c_int>,
    pub write_integer: Option<unsafe extern "C" fn(Ext, Key, *mut c_long) -> c_int>,
    pub write_integer64: Option<unsafe extern "C" fn(Ext, Key, *mut i64) -> c_int>,
    pub write_enumerated: Option<unsafe extern "C" fn(Ext, Key, *mut c_uint) -> c_int>,
    pub write_bytes: Option<unsafe extern "C" fn(Ext, Key, *mut c_uchar, usize) -> c_int>,
    pub write_iec958: Option<unsafe extern "C" fn(Ext, Key, *mut snd_aes_iec958_t) -> c_int>,
    pub subscribe_events: Option<unsafe extern "C" fn(Ext, c_int)>,
    pub read_event: Option<unsafe extern "C" fn(Ext, *mut snd_ctl_elem_id_t, *mut c_uint) -> c_int>,
    pub poll_descriptors_count: Option<unsafe extern "C" fn(Ext) -> c_int>,
    pub poll_descriptors: Option<unsafe extern "C" fn(Ext, *mut libc::pollfd, c_uint) -> c_int>,
    pub poll_revents:
        Option<unsafe extern "C" fn(Ext, *mut libc::pollfd, c_uint, *mut c_ushort) -> c_int>,
}
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
    pub fn snd_pcm_state(pcm: *mut snd_pcm_t) -> snd_pcm_state_t;

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
    pub fn snd_pcm_sw_params_get_avail_min(
        params: *const snd_pcm_sw_params_t,
        val: *mut snd_pcm_uframes_t,
    ) -> c_int;

    pub fn snd_ctl_ext_create(ext: Ext, name: *const c_char, mode: c_int) -> c_int;
    pub fn snd_ctl_elem_id_get_interface(id: *const snd_ctl_elem_id_t) -> snd_ctl_elem_iface_t;
    pub fn snd_ctl_elem_id_get_name(id: *const snd_ctl_elem_id_t) -> *const c_char;
    pub fn snd_ctl_elem_id_get_index(id: *const snd_ctl_elem_id_t) -> c_uint;
    pub fn snd_ctl_elem_id_set_interface(id: *mut snd_ctl_elem_id_t, iface: snd_ctl_elem_iface_t);
    pub fn snd_ctl_elem_id_set_name(id: *mut snd_ctl_elem_id_t, name: *const c_char);
    pub fn snd_ctl_elem_id_set_numid(id: *mut snd_ctl_elem_id_t, numid: c_uint);

    pub fn snd_config_iterator_first(node: *const snd_config_t) -> snd_config_iterator_t;
    pub fn snd_config_iterator_next(iterator: snd_config_iterator_t) -> snd_config_iterator_t;
    pub fn snd_config_iterator_end(node: *const snd_config_t) -> snd_config_iterator_t;
    pub fn snd_config_iterator_entry(iterator: snd_config_iterator_t) -> *mut snd_config_t;
    pub fn snd_config_get_id(config: *const snd_config_t, value: *mut *const c_char) -> c_int;
}
