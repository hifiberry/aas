//! Test fixture: plugins the aas loader must refuse, and one that shows
//! how the loader calls `process`.
//!
//! `aas_plugin` reads `AAS_FIXTURE` when it is called and picks the case:
//! `abi` has ABI version 99, `mono` takes one channel, `null` has a
//! `create` that fails, `unsupported` one that supports no rate,
//! `noinstance` one that succeeds without an instance, `nofn` has no
//! `process`, `default` has a parameter whose default lies outside its range, and `chunks` is a working `f32`
//! plugin that writes the frame count of each `process` call into the
//! samples it is given and reports no signal when that count is odd.
//! `typed` has one parameter of each type. `badkind` has a parameter of
//! type 9, `badint` an integer with a fraction, `badswitch` a switch from
//! 0 to 2, and `badenum` an enumeration without names. `namesonfloat` has
//! value names on a float, `enumrange` an enumeration of three values from
//! 0 to 3, `manyvalues` one of 5000 values, and `nullname` one with a null
//! name in its list. `small` states a descriptor size below version 1's;
//! `bigger` has a descriptor and parameter entries larger than version 1's,
//! as a later plugin of the same version would. `stateful` counts the
//! `process` calls since it was created or reset and writes the count into
//! the samples, and has `reset`. `slow` reports a latency of `u32::MAX`.
//! `text` has a text parameter, `file`, refuses the value `missing.wav`
//! and cannot use `only-44k.wav` at any other rate than 44.1 kHz; `textnosetter` has one without `set_param_text`, and
//! `textnullname` one without a name.
//! Anything else, `nodesc` among them, returns no descriptor at all.

use aas_plugin::{
    ABI, CREATE_FAILED, CREATE_OK, CREATE_UNSUPPORTED, DESCRIPTOR_SIZE, Descriptor, FORMAT_F32,
    PARAM_ENUMERATION, PARAM_FLOAT, PARAM_INFO_SIZE, PARAM_INTEGER, PARAM_SWITCH, PARAM_TEXT,
    ParamInfo, STATUS_NO_SIGNAL, TEXT_OK, TEXT_REFUSED, TEXT_UNSUPPORTED, ValueNames,
};
use std::ffi::c_void;

/// A fixture descriptor: stereo, `f32`, no parameters.
const fn fixture(abi: u32, channels_in: u32) -> Descriptor {
    Descriptor {
        size: DESCRIPTOR_SIZE,
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
        reset: None,
        set_param_text: None,
    }
}

static ABI_99: Descriptor = fixture(99, 2);
static MONO: Descriptor = fixture(ABI, 1);
static NULL_CREATE: Descriptor = Descriptor {
    create: Some(create_failed),
    ..fixture(ABI, 2)
};
static UNSUPPORTED: Descriptor = Descriptor {
    create: Some(create_unsupported),
    ..fixture(ABI, 2)
};
static NO_INSTANCE: Descriptor = Descriptor {
    create: Some(create_ok_without_instance),
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
        size: PARAM_INFO_SIZE,
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

static SMALL: Descriptor = Descriptor {
    size: DESCRIPTOR_SIZE - 8,
    ..fixture(ABI, 2)
};

/// A descriptor and a parameter entry as a later plugin of version 1
/// could have them, with a field the daemon does not know appended.
#[repr(C)]
struct Grown<T> {
    known: T,
    later: u64,
}
// SAFETY: as for the crate's own: the pointers are only read, and point to
// constants.
unsafe impl<T> Sync for Grown<T> {}
const GROWN_PARAM: u32 = std::mem::size_of::<Grown<ParamInfo>>() as u32;
static BIGGER_PARAMS: [Grown<ParamInfo>; 2] = [
    Grown {
        known: ParamInfo {
            size: GROWN_PARAM,
            ..param(c"level", PARAM_FLOAT, (0.0, 10.0, 0.5))
        },
        later: u64::MAX,
    },
    Grown {
        known: ParamInfo {
            size: GROWN_PARAM,
            ..param(c"taps", PARAM_INTEGER, (1.0, 16.0, 4.0))
        },
        later: u64::MAX,
    },
];
static BIGGER: Grown<Descriptor> = Grown {
    known: Descriptor {
        size: std::mem::size_of::<Grown<Descriptor>>() as u32,
        param_count: 2,
        params: BIGGER_PARAMS.as_ptr().cast(),
        ..fixture(ABI, 2)
    },
    later: u64::MAX,
};

static STATEFUL: Descriptor = Descriptor {
    create: Some(create_counter),
    destroy: Some(destroy_counter),
    process: Some(process_counter),
    reset: Some(reset_counter),
    ..fixture(ABI, 2)
};

static TEXT_PARAMS: [ParamInfo; 2] = [
    param(c"level", PARAM_FLOAT, (0.0, 10.0, 0.5)),
    param(c"file", PARAM_TEXT, (0.0, 0.0, 0.0)),
];
static TEXT: Descriptor = Descriptor {
    param_count: 2,
    params: TEXT_PARAMS.as_ptr(),
    set_param_text: Some(set_param_text),
    ..fixture(ABI, 2)
};
static TEXT_NO_SETTER: Descriptor = Descriptor {
    param_count: 2,
    params: TEXT_PARAMS.as_ptr(),
    ..fixture(ABI, 2)
};
static NAMELESS: [ParamInfo; 1] = [ParamInfo {
    name: std::ptr::null(),
    ..param(c"file", PARAM_TEXT, (0.0, 0.0, 0.0))
}];
static TEXT_NULL_NAME: Descriptor = Descriptor {
    param_count: 1,
    params: NAMELESS.as_ptr(),
    set_param_text: Some(set_param_text),
    ..fixture(ABI, 2)
};

/// Takes any text but `missing.wav`, and `only-44k.wav` at no rate: the
/// fixture's instances do not know their rate.
extern "C" fn set_param_text(_: *mut c_void, _: u32, text: *const u8, len: u32) -> u32 {
    // SAFETY: the ABI passes `len` bytes at `text`, valid for the call; an
    // empty one is not read.
    let text = if len == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(text, len as usize) }
    };
    match text {
        b"missing.wav" => TEXT_REFUSED,
        b"only-44k.wav" => TEXT_UNSUPPORTED,
        _ => TEXT_OK,
    }
}

static SLOW: Descriptor = Descriptor {
    latency: Some(latency_max),
    ..fixture(ABI, 2)
};

/// The entry point. Returns the case `AAS_FIXTURE` names.
#[unsafe(no_mangle)]
pub extern "C" fn aas_plugin() -> *const Descriptor {
    match std::env::var("AAS_FIXTURE").as_deref() {
        Ok("abi") => &ABI_99,
        Ok("mono") => &MONO,
        Ok("null") => &NULL_CREATE,
        Ok("unsupported") => &UNSUPPORTED,
        Ok("noinstance") => &NO_INSTANCE,
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
        Ok("small") => &SMALL,
        Ok("bigger") => &BIGGER.known,
        Ok("stateful") => &STATEFUL,
        Ok("slow") => &SLOW,
        Ok("text") => &TEXT,
        Ok("textnosetter") => &TEXT_NO_SETTER,
        Ok("textnullname") => &TEXT_NULL_NAME,
        _ => std::ptr::null(),
    }
}

/// An instance with no state: a pointer that is not null and never read.
extern "C" fn create(_: u32, _: u32, instance: *mut *mut c_void) -> u32 {
    // SAFETY: the ABI passes a place for the instance.
    unsafe { *instance = std::ptr::NonNull::<u8>::dangling().as_ptr().cast() };
    CREATE_OK
}

extern "C" fn create_failed(_: u32, _: u32, _: *mut *mut c_void) -> u32 {
    CREATE_FAILED
}

extern "C" fn create_unsupported(_: u32, _: u32, _: *mut *mut c_void) -> u32 {
    CREATE_UNSUPPORTED
}

extern "C" fn create_ok_without_instance(_: u32, _: u32, _: *mut *mut c_void) -> u32 {
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

extern "C" fn latency_max(_: *const c_void) -> u32 {
    u32::MAX
}

/// An instance of `stateful`: the `process` calls since `create` or
/// `reset`.
extern "C" fn create_counter(_: u32, _: u32, instance: *mut *mut c_void) -> u32 {
    // SAFETY: the ABI passes a place for the instance.
    unsafe { *instance = Box::into_raw(Box::new(0u32)).cast() };
    CREATE_OK
}

extern "C" fn destroy_counter(p: *mut c_void) {
    // SAFETY: an instance from `create_counter`, once.
    drop(unsafe { Box::from_raw(p.cast::<u32>()) });
}

extern "C" fn reset_counter(p: *mut c_void) {
    // SAFETY: a live instance from `create_counter`, used by nothing else.
    unsafe { *p.cast::<u32>() = 0 };
}

extern "C" fn process_counter(
    p: *mut c_void,
    format: u32,
    samples: *mut c_void,
    frames: u32,
) -> u32 {
    // SAFETY: a live instance from `create_counter`, used by nothing else.
    let count = unsafe { &mut *p.cast::<u32>() };
    *count += 1;
    if format == FORMAT_F32 {
        // SAFETY: `2 * frames` samples in `format`, for this call only.
        let s =
            unsafe { std::slice::from_raw_parts_mut(samples.cast::<f32>(), 2 * frames as usize) };
        s.fill(*count as f32);
    }
    0
}
