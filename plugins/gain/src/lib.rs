//! Example aas plugin: a gain in dB, from -60 to 20.
//!
//! It takes `f32` and `f64` blocks of any channel count, from 1 to 8, and
//! keeps the count. It has no latency and reports no status. At 0 dB it is
//! neutral and leaves the audio alone.

use aas_plugin::{
    ABI, CREATE_FAILED, CREATE_OK, DESCRIPTOR_SIZE, Descriptor, FORMAT_F32, FORMAT_F64,
    PARAM_FLOAT, PARAM_INFO_SIZE, ParamInfo,
};
use std::ffi::c_void;

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
    name: c"gain".as_ptr(),
    formats: FORMAT_F32 | FORMAT_F64,
    // Any count, which `create` is told.
    channels_in: 0,
    channels_out: 0,
    param_count: PARAMS.len() as u32,
    params: PARAMS.as_ptr(),
    create: Some(create),
    destroy: Some(destroy),
    set_param: Some(set_param),
    neutral: Some(neutral),
    latency: Some(latency),
    process: Some(process),
    // A gain keeps nothing from one block to the next.
    reset: None,
    set_param_text: None,
    process_into: None,
};

/// The plugin's entry point.
#[unsafe(no_mangle)]
pub extern "C" fn aas_plugin() -> *const Descriptor {
    &DESCRIPTOR
}

struct Gain {
    /// The channels of the blocks the instance is given.
    channels: usize,
    db: f64,
    /// The linear factor of `db`, worked out when it is set.
    factor: f64,
}

/// Casts an instance pointer back to the `Gain` that `create` boxed.
///
/// # Safety
///
/// `p` must come from `create` and not be destroyed yet, and nothing else
/// may use the instance during the borrow.
unsafe fn gain<'a>(p: *mut c_void) -> &'a mut Gain {
    // SAFETY: as the caller promises.
    unsafe { &mut *p.cast::<Gain>() }
}

extern "C" fn create(
    _rate: u32,
    _max_frames: u32,
    channels: u32,
    instance: *mut *mut c_void,
) -> u32 {
    if !(1..=8).contains(&channels) {
        return CREATE_FAILED;
    }
    let gain = Box::new(Gain {
        channels: channels as usize,
        db: 0.0,
        factor: 1.0,
    });
    // SAFETY: the ABI passes a place for the instance.
    unsafe { *instance = Box::into_raw(gain).cast() };
    CREATE_OK
}

extern "C" fn destroy(p: *mut c_void) {
    // SAFETY: the ABI passes an instance from `create`, once, and uses it no
    // more.
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
    // SAFETY: the ABI passes a live instance from `create`, and nothing
    // changes it during the call.
    let g = unsafe { &*p.cast::<Gain>() };
    u32::from(g.db == 0.0)
}

extern "C" fn latency(_: *const c_void) -> u32 {
    0
}

extern "C" fn process(p: *mut c_void, format: u32, samples: *mut c_void, frames: u32) -> u32 {
    // SAFETY: the ABI passes a live instance that no other call uses now.
    let g = unsafe { gain(p) };
    if g.db == 0.0 {
        return 0;
    }
    let n = g.channels * frames as usize;
    match format {
        FORMAT_F32 => {
            // SAFETY: the ABI passes `frames * channels` samples in `format`,
            // which nothing else touches during the call.
            let s = unsafe { std::slice::from_raw_parts_mut(samples.cast::<f32>(), n) };
            let f = g.factor as f32;
            s.iter_mut().for_each(|x| *x *= f);
        }
        FORMAT_F64 => {
            // SAFETY: as above.
            let s = unsafe { std::slice::from_raw_parts_mut(samples.cast::<f64>(), n) };
            s.iter_mut().for_each(|x| *x *= g.factor);
        }
        // Not a format this plugin names, so the daemon never passes it.
        _ => {}
    }
    0
}
