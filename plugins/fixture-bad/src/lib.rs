//! Test fixture: plugins the aas loader must refuse.
//!
//! `aas_plugin` reads `AAS_FIXTURE` when it is called and picks the case:
//! `abi` has ABI version 99, `mono` takes one channel, `null` has a
//! `create` that fails, and anything else, `nodesc` among them, returns no
//! descriptor at all.

use aas_plugin::{ABI, Descriptor, FORMAT_F32};
use std::ffi::c_void;

/// A fixture descriptor: stereo, `f32`, no parameters, `create` as given.
const fn fixture(abi: u32, channels_in: u32, create: CreateFn) -> Descriptor {
    Descriptor {
        abi,
        name: c"fixture".as_ptr(),
        formats: FORMAT_F32,
        channels_in,
        channels_out: 2,
        param_count: 0,
        params: std::ptr::null(),
        create,
        destroy,
        set_param,
        neutral,
        latency,
        process,
    }
}

type CreateFn = extern "C" fn(u32, u32) -> *mut c_void;

static ABI_99: Descriptor = fixture(99, 2, create);
static MONO: Descriptor = fixture(ABI, 1, create);
static NULL_CREATE: Descriptor = fixture(ABI, 2, create_null);

/// The entry point. Returns the case `AAS_FIXTURE` names.
#[unsafe(no_mangle)]
pub extern "C" fn aas_plugin() -> *const Descriptor {
    match std::env::var("AAS_FIXTURE").as_deref() {
        Ok("abi") => &ABI_99,
        Ok("mono") => &MONO,
        Ok("null") => &NULL_CREATE,
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
    1
}

extern "C" fn latency(_: *const c_void) -> u32 {
    0
}

extern "C" fn process(_: *mut c_void, _: u32, _: *mut c_void, _: u32) -> u32 {
    0
}
