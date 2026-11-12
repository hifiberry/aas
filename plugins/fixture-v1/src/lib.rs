//! Test fixture: the `gain` plugin as it was when the ABI was at version 1,
//! stereo in place, a gain in dB from -60 to 20.
//!
//! It does not depend on the `aas-plugin` crate: the structs below are a
//! copy of version 1 as that crate first described it, so that this library
//! is what one compiled against the old crate looks like, whatever the
//! crate describes now.

use std::ffi::{c_char, c_void};

const ABI: u32 = 1;
const CREATE_OK: u32 = 0;
const FORMAT_F32: u32 = 2;
const FORMAT_F64: u32 = 4;
const PARAM_FLOAT: u32 = 0;

#[repr(C)]
pub struct ParamInfo {
    size: u32,
    name: *const c_char,
    unit: *const c_char,
    kind: u32,
    min: f64,
    max: f64,
    default: f64,
    value_count: u32,
    value_names: *const *const c_char,
}

#[repr(C)]
pub struct Descriptor {
    size: u32,
    abi: u32,
    name: *const c_char,
    formats: u32,
    channels_in: u32,
    channels_out: u32,
    param_count: u32,
    params: *const ParamInfo,
    create: Option<extern "C" fn(rate: u32, max_frames: u32, instance: *mut *mut c_void) -> u32>,
    destroy: Option<extern "C" fn(*mut c_void)>,
    set_param: Option<extern "C" fn(*mut c_void, index: u32, value: f64)>,
    neutral: Option<extern "C" fn(*const c_void) -> u32>,
    latency: Option<extern "C" fn(*const c_void) -> u32>,
    process:
        Option<extern "C" fn(*mut c_void, format: u32, samples: *mut c_void, frames: u32) -> u32>,
    reset: Option<extern "C" fn(*mut c_void)>,
    set_param_text:
        Option<extern "C" fn(*mut c_void, index: u32, text: *const u8, len: u32) -> u32>,
}

// SAFETY: the pointers point to constants and are only read.
unsafe impl Sync for ParamInfo {}
// SAFETY: as above.
unsafe impl Sync for Descriptor {}

const DESCRIPTOR_SIZE: u32 = std::mem::size_of::<Descriptor>() as u32;
const PARAM_INFO_SIZE: u32 = std::mem::size_of::<ParamInfo>() as u32;

// The sizes version 1 froze.
#[cfg(target_pointer_width = "64")]
const _: () = assert!(DESCRIPTOR_SIZE == 104 && PARAM_INFO_SIZE == 72);
#[cfg(target_pointer_width = "32")]
const _: () = assert!(DESCRIPTOR_SIZE == 64 && PARAM_INFO_SIZE == 48);

const MIN_DB: f64 = -60.0;
const MAX_DB: f64 = 20.0;

static PARAMS: [ParamInfo; 1] = [ParamInfo {
    size: PARAM_INFO_SIZE,
    name: c"gain".as_ptr(),
    unit: c"dB".as_ptr(),
    kind: PARAM_FLOAT,
    min: MIN_DB,
    max: MAX_DB,
    default: 0.0,
    value_count: 0,
    value_names: std::ptr::null(),
}];

static DESCRIPTOR: Descriptor = Descriptor {
    size: DESCRIPTOR_SIZE,
    abi: ABI,
    name: c"gain-v1".as_ptr(),
    formats: FORMAT_F32 | FORMAT_F64,
    channels_in: 2,
    channels_out: 2,
    param_count: PARAMS.len() as u32,
    params: PARAMS.as_ptr(),
    create: Some(create),
    destroy: Some(destroy),
    set_param: Some(set_param),
    neutral: Some(neutral),
    latency: Some(latency),
    process: Some(process),
    reset: None,
    set_param_text: None,
};

/// The plugin's entry point.
#[unsafe(no_mangle)]
pub extern "C" fn aas_plugin() -> *const Descriptor {
    &DESCRIPTOR
}

struct Gain {
    db: f64,
    factor: f64,
}

/// # Safety
///
/// `p` must come from `create` and not be destroyed yet, and nothing else
/// may use the instance during the borrow.
unsafe fn gain<'a>(p: *mut c_void) -> &'a mut Gain {
    // SAFETY: as the caller promises.
    unsafe { &mut *p.cast::<Gain>() }
}

extern "C" fn create(_rate: u32, _max_frames: u32, instance: *mut *mut c_void) -> u32 {
    let gain = Box::new(Gain {
        db: 0.0,
        factor: 1.0,
    });
    // SAFETY: the ABI passes a place for the instance.
    unsafe { *instance = Box::into_raw(gain).cast() };
    CREATE_OK
}

extern "C" fn destroy(p: *mut c_void) {
    // SAFETY: the ABI passes an instance from `create`, once.
    drop(unsafe { Box::from_raw(p.cast::<Gain>()) });
}

extern "C" fn set_param(p: *mut c_void, index: u32, value: f64) {
    // SAFETY: the ABI passes a live instance that no other call uses now.
    let g = unsafe { gain(p) };
    if index == 0 && !value.is_nan() {
        g.db = value.clamp(MIN_DB, MAX_DB);
        g.factor = 10f64.powf(g.db / 20.0);
    }
}

extern "C" fn neutral(p: *const c_void) -> u32 {
    // SAFETY: the ABI passes a live instance from `create`.
    let g = unsafe { &*p.cast::<Gain>() };
    u32::from(g.db == 0.0)
}

extern "C" fn latency(_: *const c_void) -> u32 {
    0
}

extern "C" fn process(p: *mut c_void, format: u32, samples: *mut c_void, frames: u32) -> u32 {
    // SAFETY: the ABI passes a live instance that no other call uses now.
    let g = unsafe { gain(p) };
    let n = 2 * frames as usize;
    match format {
        FORMAT_F32 => {
            // SAFETY: the ABI passes `2 * frames` samples in `format`.
            let s = unsafe { std::slice::from_raw_parts_mut(samples.cast::<f32>(), n) };
            let f = g.factor as f32;
            s.iter_mut().for_each(|x| *x *= f);
        }
        FORMAT_F64 => {
            // SAFETY: as above.
            let s = unsafe { std::slice::from_raw_parts_mut(samples.cast::<f64>(), n) };
            s.iter_mut().for_each(|x| *x *= g.factor);
        }
        _ => {}
    }
    0
}
