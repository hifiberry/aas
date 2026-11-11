//! Test fixture: plugins the aas loader must refuse, and one that shows
//! how the loader calls `process`.
//!
//! `aas_plugin` reads `AAS_FIXTURE` when it is called and picks the case:
//! `abi` has ABI version 99, `mono` takes one channel, `null` has a
//! `create` that fails, `nofn` has no `process`, `default` has a parameter
//! whose default lies outside its range, and `chunks` is a working `f32`
//! plugin that writes the frame count of each `process` call into the
//! samples it is given and reports no signal when that count is odd.
//! `typed` has one parameter of each type. `badkind` has a parameter of
//! type 9, `badint` an integer with a fraction, `badswitch` a switch from
//! 0 to 2, and `badenum` an enumeration without names. `namesonfloat` has
//! value names on a float, `enumrange` an enumeration of three values from
//! 0 to 3, `manyvalues` one of 5000 values, and `nullname` one with a null
//! name in its list.
//! Anything else, `nodesc` among them, returns no descriptor at all.

use aas_plugin::{
    ABI, Descriptor, FORMAT_F32, PARAM_ENUMERATION, PARAM_FLOAT, PARAM_INTEGER, PARAM_SWITCH,
    ParamInfo, STATUS_NO_SIGNAL, ValueNames,
};
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

/// A parameter of type `kind`, with no unit and no value names, and its
/// range and default.
const fn param(name: &'static std::ffi::CStr, kind: u32, range: (f64, f64, f64)) -> ParamInfo {
    ParamInfo {
        name: name.as_ptr(),
        unit: std::ptr::null(),
        kind,
        min: range.0,
        max: range.1,
        default: range.2,
        value_count: 0,
        value_names: std::ptr::null(),
    }
}

static BAD_DEFAULT: [ParamInfo; 1] = [param(c"level", PARAM_FLOAT, (0.0, 10.0, 30.0))];
static DEFAULT: Descriptor = Descriptor {
    param_count: 1,
    params: BAD_DEFAULT.as_ptr(),
    ..fixture(ABI, 2)
};

static CURVES: ValueNames<3> = ValueNames([c"riaa".as_ptr(), c"flat".as_ptr(), c"nab".as_ptr()]);
static TYPED_PARAMS: [ParamInfo; 4] = [
    param(c"level", PARAM_FLOAT, (0.0, 10.0, 0.5)),
    param(c"taps", PARAM_INTEGER, (1.0, 16.0, 4.0)),
    param(c"bypass", PARAM_SWITCH, (0.0, 1.0, 0.0)),
    ParamInfo {
        value_count: 3,
        value_names: CURVES.as_ptr(),
        ..param(c"curve", PARAM_ENUMERATION, (0.0, 2.0, 1.0))
    },
];
static TYPED: Descriptor = Descriptor {
    param_count: 4,
    params: TYPED_PARAMS.as_ptr(),
    ..fixture(ABI, 2)
};

/// A descriptor with one parameter.
macro_rules! one_param {
    ($params:ident, $desc:ident, $info:expr) => {
        static $params: [ParamInfo; 1] = [$info];
        static $desc: Descriptor = Descriptor {
            param_count: 1,
            params: $params.as_ptr(),
            ..fixture(ABI, 2)
        };
    };
}
one_param!(KIND_PARAMS, BAD_KIND, param(c"level", 9, (0.0, 1.0, 0.0)));
one_param!(
    INT_PARAMS,
    BAD_INT,
    param(c"taps", PARAM_INTEGER, (1.0, 16.0, 4.5))
);
one_param!(
    SWITCH_PARAMS,
    BAD_SWITCH,
    param(c"bypass", PARAM_SWITCH, (0.0, 2.0, 0.0))
);
static WITH_NULL: ValueNames<3> = ValueNames([c"a".as_ptr(), std::ptr::null(), c"c".as_ptr()]);
one_param!(
    NAMES_PARAMS,
    NAMES_ON_FLOAT,
    ParamInfo {
        value_count: 3,
        value_names: CURVES.as_ptr(),
        ..param(c"level", PARAM_FLOAT, (0.0, 2.0, 0.0))
    }
);
one_param!(
    RANGE_PARAMS,
    ENUM_RANGE,
    ParamInfo {
        value_count: 3,
        value_names: CURVES.as_ptr(),
        ..param(c"curve", PARAM_ENUMERATION, (0.0, 3.0, 0.0))
    }
);
one_param!(
    MANY_PARAMS,
    MANY_VALUES,
    ParamInfo {
        value_count: 5000,
        value_names: CURVES.as_ptr(),
        ..param(c"curve", PARAM_ENUMERATION, (0.0, 4999.0, 0.0))
    }
);
one_param!(
    NULL_PARAMS,
    NULL_NAME,
    ParamInfo {
        value_count: 3,
        value_names: WITH_NULL.as_ptr(),
        ..param(c"curve", PARAM_ENUMERATION, (0.0, 2.0, 0.0))
    }
);
one_param!(
    ENUM_PARAMS,
    BAD_ENUM,
    ParamInfo {
        value_count: 3,
        ..param(c"curve", PARAM_ENUMERATION, (0.0, 2.0, 0.0))
    }
);

/// The entry point. Returns the case `AAS_FIXTURE` names.
#[unsafe(no_mangle)]
pub extern "C" fn aas_plugin() -> *const Descriptor {
    match std::env::var("AAS_FIXTURE").as_deref() {
        Ok("abi") => &ABI_99,
        Ok("mono") => &MONO,
        Ok("null") => &NULL_CREATE,
        Ok("nofn") => &NO_FN,
        Ok("default") => &DEFAULT,
        Ok("typed") => &TYPED,
        Ok("badkind") => &BAD_KIND,
        Ok("badint") => &BAD_INT,
        Ok("badswitch") => &BAD_SWITCH,
        Ok("badenum") => &BAD_ENUM,
        Ok("namesonfloat") => &NAMES_ON_FLOAT,
        Ok("enumrange") => &ENUM_RANGE,
        Ok("manyvalues") => &MANY_VALUES,
        Ok("nullname") => &NULL_NAME,
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
