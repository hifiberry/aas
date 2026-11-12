//! Test fixture: a plugin of ABI version 2 with two channels in and four
//! out. It copies left to channels 1 and 3 and right to channels 2 and 4,
//! through `process_into`.

use aas_plugin::{ABI, CREATE_OK, DESCRIPTOR_SIZE, Descriptor, FORMAT_F32, FORMAT_F64, FORMAT_I32};
use std::ffi::c_void;

static DESCRIPTOR: Descriptor = Descriptor {
    size: DESCRIPTOR_SIZE,
    abi: ABI,
    name: c"wide".as_ptr(),
    formats: FORMAT_I32 | FORMAT_F32 | FORMAT_F64,
    channels_in: 2,
    channels_out: 4,
    param_count: 0,
    params: std::ptr::null(),
    create: Some(create),
    destroy: Some(destroy),
    set_param: Some(set_param),
    neutral: Some(neutral),
    latency: Some(latency),
    process: Some(process),
    reset: None,
    set_param_text: None,
    process_into: Some(process_into),
};

/// The plugin's entry point.
#[unsafe(no_mangle)]
pub extern "C" fn aas_plugin() -> *const Descriptor {
    &DESCRIPTOR
}

/// An instance with no state: a pointer that is not null and never read.
extern "C" fn create(_: u32, _: u32, channels: u32, instance: *mut *mut c_void) -> u32 {
    if channels != 2 {
        return aas_plugin::CREATE_FAILED;
    }
    // SAFETY: the ABI passes a place for the instance.
    unsafe { *instance = std::ptr::NonNull::<u8>::dangling().as_ptr().cast() };
    CREATE_OK
}

extern "C" fn destroy(_: *mut c_void) {}

extern "C" fn set_param(_: *mut c_void, _: u32, _: f64) {}

extern "C" fn neutral(_: *const c_void) -> u32 {
    0
}

extern "C" fn latency(_: *const c_void) -> u32 {
    0
}

/// Never called: the counts differ, so the daemon uses `process_into`.
extern "C" fn process(_: *mut c_void, _: u32, _: *mut c_void, _: u32) -> u32 {
    0
}

/// Widens `frames` frames of `T`.
///
/// # Safety
///
/// `input` holds `2 * frames` samples of `T`, `output` has room for
/// `4 * frames`, and they do not overlap.
unsafe fn widen<T: Copy>(input: *const c_void, output: *mut c_void, frames: usize) {
    // SAFETY: as the caller promises.
    let (i, o) = unsafe {
        (
            std::slice::from_raw_parts(input.cast::<T>(), 2 * frames),
            std::slice::from_raw_parts_mut(output.cast::<T>(), 4 * frames),
        )
    };
    for (i, o) in i.chunks_exact(2).zip(o.chunks_exact_mut(4)) {
        o.copy_from_slice(&[i[0], i[1], i[0], i[1]]);
    }
}

extern "C" fn process_into(
    _: *mut c_void,
    format: u32,
    input: *const c_void,
    output: *mut c_void,
    frames: u32,
) -> u32 {
    let frames = frames as usize;
    // SAFETY: the ABI passes `2 * frames` samples in `format` at `input`
    // and room for `4 * frames` at `output`, not overlapping.
    unsafe {
        match format {
            FORMAT_I32 => widen::<i32>(input, output, frames),
            FORMAT_F32 => widen::<f32>(input, output, frames),
            FORMAT_F64 => widen::<f64>(input, output, frames),
            _ => {}
        }
    }
    0
}
