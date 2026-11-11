//! The C ABI of aas plugins, version 1.
//!
//! A plugin is a shared library that exports one function,
//! `aas_plugin`, which returns a pointer to a [`Descriptor`]. The daemon
//! loads the library once and never unloads it, so the descriptor, the
//! strings and the parameter list it points to must live as long as the
//! library and never change.
//!
//! A plugin processes stereo audio in place, in one of the formats it
//! names. Float samples are scaled so that full scale is 1.0 and are not
//! clipped. `create` and `destroy` run on the control thread. `process`,
//! `set_param`, `neutral` and `latency` run on the audio thread and must
//! not allocate, lock or block. An instance can move between threads but
//! is never used by two at once. `process` may be called with
//! `frames == 0`.
//!
//! The daemon may call `aas_plugin` more than once. It must return the
//! same descriptor every time.
//!
//! A panic that leaves an `extern "C" fn` aborts the whole daemon. A
//! plugin written in Rust must not panic in the functions it exports, or
//! must catch the panic with `std::panic::catch_unwind` first.
//!
//! The six functions are `Option`s of function pointers, which have the
//! layout of a bare C function pointer with null as `None`. A C header
//! declares them as plain function pointers. The daemon refuses a
//! descriptor in which any of them is null.
//!
//! A plugin written in Rust exports its descriptor as a `static`:
//!
//! ```
//! use aas_plugin::{ABI, Descriptor, FORMAT_F32, ParamInfo};
//! use std::ffi::c_void;
//!
//! static PARAMS: [ParamInfo; 1] = [ParamInfo {
//!     name: c"level".as_ptr(),
//!     unit: c"dB".as_ptr(),
//!     min: -20.0,
//!     max: 0.0,
//!     default: 0.0,
//! }];
//!
//! static DESCRIPTOR: Descriptor = Descriptor {
//!     abi: ABI,
//!     name: c"example".as_ptr(),
//!     formats: FORMAT_F32,
//!     channels_in: 2,
//!     channels_out: 2,
//!     param_count: PARAMS.len() as u32,
//!     params: PARAMS.as_ptr(),
//!     create: Some(create),
//!     destroy: Some(destroy),
//!     set_param: Some(set_param),
//!     neutral: Some(neutral),
//!     latency: Some(latency),
//!     process: Some(process),
//! };
//!
//! #[unsafe(no_mangle)]
//! pub extern "C" fn aas_plugin() -> *const Descriptor {
//!     &DESCRIPTOR
//! }
//! # extern "C" fn create(_: u32, _: u32) -> *mut c_void { std::ptr::null_mut() }
//! # extern "C" fn destroy(_: *mut c_void) {}
//! # extern "C" fn set_param(_: *mut c_void, _: u32, _: f64) {}
//! # extern "C" fn neutral(_: *const c_void) -> u32 { 1 }
//! # extern "C" fn latency(_: *const c_void) -> u32 { 0 }
//! # extern "C" fn process(_: *mut c_void, _: u32, _: *mut c_void, _: u32) -> u32 { 0 }
//! ```
//!
//! `plugins/gain` in the aas repository is a complete example.

use std::ffi::{c_char, c_void};

/// The ABI version this crate describes. The daemon refuses any other.
pub const ABI: u32 = 1;

/// Signed 32 bit integer samples, full scale at 2^31.
pub const FORMAT_I32: u32 = 1;
/// 32 bit float samples.
pub const FORMAT_F32: u32 = 2;
/// 64 bit float samples.
pub const FORMAT_F64: u32 = 4;

/// Status bit of `process`: the block carried no signal worth sending on.
pub const STATUS_NO_SIGNAL: u32 = 1;

/// One parameter of a plugin.
#[repr(C)]
#[derive(Debug)]
pub struct ParamInfo {
    /// A NUL terminated UTF-8 name. Must not be null.
    pub name: *const c_char,
    /// A NUL terminated UTF-8 unit, such as `dB`. Null means none.
    pub unit: *const c_char,
    /// The smallest value, inclusive. Not NaN.
    pub min: f64,
    /// The largest value, inclusive. Not NaN, and not below `min`.
    pub max: f64,
    /// The value an instance starts with. It lies in `min` to `max`.
    pub default: f64,
}

/// What a plugin is and the functions that run it. Every function must be
/// given; the daemon refuses a descriptor in which one is `None`.
#[repr(C)]
#[derive(Debug)]
pub struct Descriptor {
    /// Must be [`ABI`].
    pub abi: u32,
    /// A NUL terminated UTF-8 name. Must not be null.
    pub name: *const c_char,
    /// The formats `process` takes, a mask of the `FORMAT_` bits. Not 0.
    pub formats: u32,
    /// 2 in version 1. Other channel counts come with the matrix.
    pub channels_in: u32,
    /// 2 in version 1.
    pub channels_out: u32,
    /// The number of entries at `params`.
    pub param_count: u32,
    /// `param_count` parameters, or null if there are none. The daemon
    /// takes at most 4096.
    pub params: *const ParamInfo,
    /// Builds an instance for `rate` Hz and blocks of up to `max_frames`
    /// frames, with every parameter at its default. Null means it failed.
    pub create: Option<extern "C" fn(rate: u32, max_frames: u32) -> *mut c_void>,
    /// Frees an instance `create` returned.
    pub destroy: Option<extern "C" fn(*mut c_void)>,
    /// Sets parameter `index` to `value`. The daemon passes only indices
    /// below `param_count` and values inside the parameter's range.
    pub set_param: Option<extern "C" fn(*mut c_void, index: u32, value: f64)>,
    /// Not 0 if, with the parameters it has now, the instance leaves every
    /// block as it is.
    pub neutral: Option<extern "C" fn(*const c_void) -> u32>,
    /// How many frames the instance delays the audio by.
    pub latency: Option<extern "C" fn(*const c_void) -> u32>,
    /// Processes `frames` frames in place: `2 * frames` interleaved samples
    /// in `format`, one of the `FORMAT_` bits in `formats`. `frames` is at
    /// most the `max_frames` of `create`, and may be 0. Returns a mask of
    /// the `STATUS_` bits.
    pub process:
        Option<extern "C" fn(*mut c_void, format: u32, samples: *mut c_void, frames: u32) -> u32>,
}

// SAFETY: the pointers in a parameter are only read, and the data they
// point to is constant for the life of the library (see the crate docs).
// Sharing the pointer values between threads is sound on its own; reading
// through them needs `unsafe` that relies on that rule.
unsafe impl Sync for ParamInfo {}

// SAFETY: as for `ParamInfo`. The functions are plain code pointers.
unsafe impl Sync for Descriptor {}
