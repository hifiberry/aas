//! The C ABI of aas plugins, version 1.
//!
//! A plugin is a shared library that exports one function,
//! `aas_plugin`, which returns a pointer to a [`Descriptor`]. The daemon
//! loads the library once and never unloads it, so the descriptor, the
//! strings and the parameter list it points to must live as long as the
//! library and never change.
//!
//! A plugin processes stereo audio in place, in one of the formats it
//! names. Stereo in place is fixed in version 1: `channels_in` and
//! `channels_out` must both be 2. A plugin that changes the channel count
//! needs a separate output buffer, which is ABI version 2. Float samples are scaled so that full scale is 1.0 and are not
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
//! use aas_plugin::{
//!     ABI, Descriptor, FORMAT_F32, PARAM_ENUMERATION, PARAM_FLOAT, ParamInfo, ValueNames,
//! };
//! use std::ffi::c_void;
//!
//! static CURVES: ValueNames<2> = ValueNames([c"flat".as_ptr(), c"riaa".as_ptr()]);
//!
//! static PARAMS: [ParamInfo; 2] = [
//!     ParamInfo {
//!         name: c"level".as_ptr(),
//!         unit: c"dB".as_ptr(),
//!         kind: PARAM_FLOAT,
//!         min: -20.0,
//!         max: 0.0,
//!         default: 0.0,
//!         value_count: 0,
//!         value_names: std::ptr::null(),
//!     },
//!     ParamInfo {
//!         name: c"curve".as_ptr(),
//!         unit: std::ptr::null(),
//!         kind: PARAM_ENUMERATION,
//!         min: 0.0,
//!         max: 1.0,
//!         default: 0.0,
//!         value_count: 2,
//!         value_names: CURVES.as_ptr(),
//!     },
//! ];
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

/// A parameter that takes any value in its range.
pub const PARAM_FLOAT: u32 = 0;
/// A parameter that takes whole numbers only. Its range and default are
/// whole numbers.
pub const PARAM_INTEGER: u32 = 1;
/// A switch: 0 is off, 1 is on. Its range is 0 to 1, its default 0 or 1.
pub const PARAM_SWITCH: u32 = 2;
/// One of a list of named values, numbered from 0. Its range is 0 to the
/// number of values less one, its default one of them, and `value_names`
/// names each value.
pub const PARAM_ENUMERATION: u32 = 3;

/// One parameter of a plugin. Values always travel as `f64`; the type says
/// which of them the parameter takes, so that a user interface can build
/// the right control for it.
#[repr(C)]
#[derive(Debug)]
pub struct ParamInfo {
    /// A NUL terminated UTF-8 name. Must not be null.
    pub name: *const c_char,
    /// A NUL terminated UTF-8 unit, such as `dB`. Null means none.
    pub unit: *const c_char,
    /// One of the `PARAM_` types.
    pub kind: u32,
    /// The smallest value, inclusive. Not NaN.
    pub min: f64,
    /// The largest value, inclusive. Not NaN, and not below `min`.
    pub max: f64,
    /// The value an instance starts with. It lies in `min` to `max`.
    pub default: f64,
    /// For [`PARAM_ENUMERATION`], the number of values, at least 1 and at
    /// most 4096. 0 for every other type.
    pub value_count: u32,
    /// For [`PARAM_ENUMERATION`], `value_count` pointers to the NUL
    /// terminated UTF-8 name of each value, from value 0 on. Null for every
    /// other type. Like the descriptor, they live as long as the library.
    pub value_names: *const *const c_char,
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
    /// below `param_count` and values inside the parameter's range, whole
    /// numbers for every type but [`PARAM_FLOAT`].
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

/// The names of an enumeration's values, as a `static` can hold them: a
/// plain array of pointers cannot be one, since pointers are not `Sync`.
#[repr(transparent)]
#[derive(Debug)]
pub struct ValueNames<const N: usize>(pub [*const c_char; N]);

impl<const N: usize> ValueNames<N> {
    /// The pointer for [`ParamInfo::value_names`].
    pub const fn as_ptr(&self) -> *const *const c_char {
        self.0.as_ptr()
    }
}

// SAFETY: as for `ParamInfo`: the names are constant for the life of the
// library.
unsafe impl<const N: usize> Sync for ValueNames<N> {}

// SAFETY: the pointers in a parameter are only read, and the data they
// point to is constant for the life of the library (see the crate docs).
// Sharing the pointer values between threads is sound on its own; reading
// through them needs `unsafe` that relies on that rule.
unsafe impl Sync for ParamInfo {}

// SAFETY: as for `ParamInfo`. The functions are plain code pointers.
unsafe impl Sync for Descriptor {}
