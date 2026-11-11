//! Test fixture: plugins the aas loader must refuse, and one that shows
//! how the loader calls `process`.
//!
//! `aas_plugin` reads `AAS_FIXTURE` when it is called and picks the case:
//! `abi` has ABI version 99, `mono` takes one channel, `null` has a
//! `create` that fails, `nofn` has no `process`, `default` has a parameter
//! whose default lies outside its range, and `chunks` is a working `f32`
//! plugin that writes the frame count of each `process` call into the
//! samples it is given and reports no signal when that count is odd.
//! Anything else, `nodesc` among them, returns no descriptor at all.

use aas_plugin::{ABI, Descriptor, FORMAT_F32, ParamInfo, STATUS_NO_SIGNAL};
use std::ffi::c_void;

/// A fixture descriptor: stereo, `f32`, no parameters.
const fn fixture(abi: u32, channels_in: u32) -> Descriptor {
    Descriptor {
        abi,
        name: c"fixture".as_ptr(),
        formats: FORMAT_F32,
        channels_in,
        channels_out: 2,
        param_count: 0,
        params: std::ptr::null(),
        create: Some(create),
        destroy: Some(destroy),
        set_param: Some(set_param),
        neutral: Some(neutral),
        latency: Some(latency),
        process: Some(process),
    }
}

static ABI_99: Descriptor = fixture(99, 2);
static MONO: Descriptor = fixture(ABI, 1);
static NULL_CREATE: Descriptor = Descriptor {
    create: Some(create_null),
    ..fixture(ABI, 2)
};
static NO_FN: Descriptor = Descriptor {
    process: None,
    ..fixture(ABI, 2)
};
static CHUNKS: Descriptor = fixture(ABI, 2);

static BAD_DEFAULT: [ParamInfo; 1] = [ParamInfo {
    name: c"level".as_ptr(),
    unit: std::ptr::null(),
    min: 0.0,
    max: 10.0,
    default: 30.0,
}];
static DEFAULT: Descriptor = Descriptor {
    param_count: 1,
    params: BAD_DEFAULT.as_ptr(),
    ..fixture(ABI, 2)
};

/// The entry point. Returns the case `AAS_FIXTURE` names.
#[unsafe(no_mangle)]
pub extern "C" fn aas_plugin() -> *const Descriptor {
    match std::env::var("AAS_FIXTURE").as_deref() {
        Ok("abi") => &ABI_99,
        Ok("mono") => &MONO,
        Ok("null") => &NULL_CREATE,
        Ok("nofn") => &NO_FN,
        Ok("default") => &DEFAULT,
        Ok("chunks") => &CHUNKS,
        _ => std::ptr::null(),
    }
}

/// An instance with no state: a pointer that is not null and never read.
extern "C" fn create(_: u32, _: u32) -> *mut c_void {
    std::ptr::NonNull::<u8>::dangling().as_ptr().cast()
}

extern "C" fn create_null(_: u32, _: u32) -> *mut c_void {
    std::ptr::null_mut()
}

extern "C" fn destroy(_: *mut c_void) {}

extern "C" fn set_param(_: *mut c_void, _: u32, _: f64) {}

extern "C" fn neutral(_: *const c_void) -> u32 {
    0
}

extern "C" fn latency(_: *const c_void) -> u32 {
    0
}

extern "C" fn process(_: *mut c_void, format: u32, samples: *mut c_void, frames: u32) -> u32 {
    if format != FORMAT_F32 {
        return 0;
    }
    // SAFETY: the ABI passes `2 * frames` samples in `format`, which nothing
    // else touches during the call.
    let s = unsafe { std::slice::from_raw_parts_mut(samples.cast::<f32>(), 2 * frames as usize) };
    s.fill(frames as f32);
    if frames % 2 == 1 { STATUS_NO_SIGNAL } else { 0 }
}
