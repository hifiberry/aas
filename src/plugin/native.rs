//! Plugins from shared libraries, through the C ABI of `aas-plugin`.
//!
//! A library is loaded once and never unloaded, so its code and statics
//! outlive every instance built from it. Loading checks everything the
//! ABI promises before a single instance is built, and an instance copies
//! what it reports into Rust values when it is built, so the calls on the
//! audio thread allocate nothing.

use super::{
    Block, BuildError, Channels, Failure, Formats, Param, ParamKind, Plugin, Samples, Status,
};
use crate::matrix::MAX_CHANNELS;
use aas_plugin::{
    ABI, CREATE_OK, CREATE_UNSUPPORTED, DESCRIPTOR_SIZE_V1, DESCRIPTOR_SIZE_V2, Descriptor,
    FORMAT_F32, FORMAT_F64, FORMAT_I32, MAX_STRUCT_SIZE, PARAM_ENUMERATION, PARAM_FLOAT,
    PARAM_INFO_SIZE_V1, PARAM_INTEGER, PARAM_SWITCH, PARAM_TEXT, ParamInfo, STATUS_NO_SIGNAL,
    TEXT_OK, TEXT_UNSUPPORTED,
};
use std::ffi::{CStr, CString, c_char, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// A loaded plugin library, from which instances are built.
pub struct Library {
    /// The path, for messages.
    path: String,
    fns: Fns,
    name: String,
    formats: Formats,
    channels: Channels,
    params: Vec<Param>,
}

impl Library {
    /// Loads the library at `path` and checks its descriptor. Every
    /// refusal names the path.
    pub fn open(path: &Path) -> Result<Library, String> {
        let shown = path.display().to_string();
        let fail = |why: String| format!("plugin {shown}: {why}");
        let c_path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| fail("the path contains a NUL byte".into()))?;
        // SAFETY: c_path is a valid C string. Loading runs the library's
        // initialisers; loading a library is trusting its code.
        let handle = unsafe { libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            return Err(fail(format!("cannot load it: {}", dl_error())));
        }
        // The handle is never closed: the library stays loaded for the life
        // of the daemon, refused or not. A refusal stops the daemon.
        // SAFETY: handle is a live handle and the name a valid C string.
        let sym = unsafe { libc::dlsym(handle, c"aas_plugin".as_ptr()) };
        if sym.is_null() {
            return Err(fail("it has no symbol aas_plugin".into()));
        }
        // SAFETY: the ABI defines `aas_plugin` as this function.
        let entry: extern "C" fn() -> *const Descriptor = unsafe { std::mem::transmute(sym) };
        let desc = entry();
        if desc.is_null() {
            return Err(fail("aas_plugin returned no descriptor".into()));
        }
        if !desc.is_aligned() {
            return Err(fail("aas_plugin returned a misaligned descriptor".into()));
        }
        // Every version starts with the size and the ABI version, so these
        // two are read before the rest.
        // SAFETY: not null and aligned; the ABI promises a descriptor that
        // lives as long as the library, which is never unloaded.
        let (size, abi) = unsafe { (desc.cast::<u32>().read(), desc.cast::<u32>().add(1).read()) };
        if abi != 1 && abi != ABI {
            return Err(fail(format!(
                "it has ABI version {abi}, this daemon takes 1 and {ABI}"
            )));
        }
        let first = if abi == 1 {
            DESCRIPTOR_SIZE_V1
        } else {
            DESCRIPTOR_SIZE_V2
        };
        let align = std::mem::align_of::<Descriptor>() as u32;
        if !(first..=MAX_STRUCT_SIZE).contains(&size) || size % align != 0 {
            return Err(fail(format!(
                "its descriptor has {size} bytes, not version {abi}'s {first} to \
                 {MAX_STRUCT_SIZE} in steps of {align}"
            )));
        }
        // The daemon's `Descriptor` is version 2's. A version 1 descriptor is
        // shorter, so it is copied into a zeroed one, and only as far as its
        // own version goes: a version 1 library's `process_into` is never
        // read, and a later field is not either.
        let known = if abi == 1 {
            DESCRIPTOR_SIZE_V1
        } else {
            DESCRIPTOR_SIZE_V2
        } as usize;
        let mut copy = std::mem::MaybeUninit::<Descriptor>::zeroed();
        // SAFETY: `size` bytes, at least `known`, are there as the ABI
        // promises for as long as the library is loaded, and the copy has
        // room for `known`, which is at most the size of a `Descriptor`.
        // Zero bytes are a valid `Descriptor`: null pointers and `None`.
        let desc: Descriptor = unsafe {
            std::ptr::copy_nonoverlapping(desc.cast::<u8>(), copy.as_mut_ptr().cast(), known);
            copy.assume_init()
        };
        let desc = &desc;
        let channels = if abi == 1 {
            if desc.channels_in != 2 || desc.channels_out != 2 {
                return Err(fail(format!(
                    "it has ABI version 1, which is stereo, but takes {} channels in and {} out",
                    desc.channels_in, desc.channels_out
                )));
            }
            Channels::Fixed {
                input: 2,
                output: 2,
            }
        } else {
            channels(desc).map_err(fail)?
        };
        let formats = Formats {
            i32: desc.formats & FORMAT_I32 != 0,
            f32: desc.formats & FORMAT_F32 != 0,
            f64: desc.formats & FORMAT_F64 != 0,
        };
        if !(formats.i32 || formats.f32 || formats.f64) {
            return Err(fail("it names no format it takes".into()));
        }
        let fns = Fns::of(desc, abi).map_err(|f| fail(format!("it has no function {f}")))?;
        if let Channels::Fixed { input, output } = channels
            && input != output
            && fns.process_into.is_none()
        {
            return Err(fail(format!(
                "it takes {input} channels and gives {output} but has no function process_into"
            )));
        }
        // SAFETY: the ABI promises the name is null or a C string that
        // lives as long as the library.
        let name = unsafe { text(desc.name) }.map_err(|e| fail(format!("its name {e}")))?;
        let params = if desc.param_count == 0 {
            Vec::new()
        } else if desc.param_count > MAX_PARAMS {
            return Err(fail(format!(
                "it has {} parameters, more than the {MAX_PARAMS} allowed",
                desc.param_count
            )));
        } else if desc.params.is_null() || !desc.params.is_aligned() {
            return Err(fail(format!(
                "it has {} parameters but no aligned list of them",
                desc.param_count
            )));
        } else {
            // The entries are as large as the first says: a later plugin
            // of this version may have appended fields.
            // SAFETY: not null and aligned, and the ABI promises
            // `param_count` entries there for as long as the library is
            // loaded, each starting with its size.
            let stride = unsafe { desc.params.cast::<u32>().read() };
            let align = std::mem::align_of::<ParamInfo>() as u32;
            if !(PARAM_INFO_SIZE_V1..=MAX_STRUCT_SIZE).contains(&stride) || stride % align != 0 {
                return Err(fail(format!(
                    "its parameters have {stride} bytes, not version {ABI}'s \
                     {PARAM_INFO_SIZE_V1} to {MAX_STRUCT_SIZE} in steps of {align}"
                )));
            }
            let base = desc.params.cast::<u8>();
            (0..desc.param_count as usize)
                .map(|i| {
                    // SAFETY: entry `i` of the list, aligned as the stride is.
                    let info = unsafe { &*base.add(i * stride as usize).cast::<ParamInfo>() };
                    if info.size != stride {
                        return Err(fail(format!(
                            "parameter {i} has {} bytes, the first has {stride}",
                            info.size
                        )));
                    }
                    param(info).map_err(|e| fail(format!("parameter {i}: {e}")))
                })
                .collect::<Result<_, _>>()?
        };
        if fns.set_param_text.is_none() && params.iter().any(|p: &Param| p.kind == ParamKind::Text)
        {
            return Err(fail(
                "it has a text parameter but no function set_param_text".into(),
            ));
        }
        Ok(Library {
            path: shown,
            fns,
            name,
            formats,
            channels,
            params,
        })
    }

    /// The channel counts the library's plugin takes and gives.
    pub fn channels(&self) -> Channels {
        self.channels
    }

    /// Builds an instance for `rate` Hz and blocks of up to `max_frames`
    /// frames, with its parameters at their defaults. `channels` is the
    /// count of the blocks that reach the plugin: a plugin for any count is
    /// built for it, a fixed one for its own input count. Runs on the control
    /// thread.
    ///
    /// A plugin that does not support `rate` gives a `Failure::Unsupported`
    /// error; one that failed, or answered something the ABI does not
    /// allow, a `Failure::Failed` one.
    pub fn create(
        &self,
        rate: u32,
        max_frames: usize,
        channels: usize,
    ) -> Result<Box<dyn Plugin>, BuildError> {
        let fail = |kind, why: &str| {
            BuildError::new(
                kind,
                format!("plugin {} from {}: {why}", self.name, self.path),
            )
        };
        let frames = match u32::try_from(max_frames) {
            Ok(0) => return Err(fail(Failure::Failed, "blocks of no frames")),
            Ok(n) => n,
            Err(_) => return Err(fail(Failure::Failed, "blocks too long")),
        };
        let (input, output) = match self.channels {
            Channels::Any => (channels, channels),
            Channels::Fixed { input, output } => (input, output),
        };
        if !(1..=MAX_CHANNELS).contains(&input) {
            return Err(fail(
                Failure::Failed,
                &format!("{input} channels is not from 1 to {MAX_CHANNELS}"),
            ));
        }
        let mut instance = std::ptr::null_mut();
        // `input` is at most `MAX_CHANNELS`.
        let status = match self.fns.create {
            Create::V1(create) => create(rate, frames, &mut instance),
            Create::V2(create) => create(rate, frames, input as u32, &mut instance),
        };
        match status {
            CREATE_OK if !instance.is_null() => {}
            CREATE_OK => {
                return Err(fail(
                    Failure::Failed,
                    &format!("its create at {rate} Hz succeeded but gave no instance"),
                ));
            }
            CREATE_UNSUPPORTED => {
                return Err(fail(
                    Failure::Unsupported,
                    &format!(
                        "it does not support {rate} Hz, which is not used while the daemon runs"
                    ),
                ));
            }
            _ => {
                return Err(fail(
                    Failure::Failed,
                    &format!("its create failed at {rate} Hz (status {status})"),
                ));
            }
        }
        let max_latency = rate.saturating_mul(MAX_LATENCY_S);
        let latency = (self.fns.latency)(instance);
        if latency > max_latency {
            eprintln!(
                "aas: plugin {} from {} reports a latency of {latency} frames; taken as \
                 {MAX_LATENCY_S} s, {max_latency} frames at {rate} Hz",
                self.name, self.path
            );
        }
        Ok(Box::new(Native {
            fns: self.fns,
            instance,
            max_latency,
            name: self.name.clone(),
            formats: self.formats,
            channels: self.channels,
            input,
            output,
            params: self.params.clone(),
            max_frames,
        }))
    }
}

/// The longest latency a plugin is taken to have, in seconds. A longer one
/// is a plugin's error, and would only make the reported delay absurd.
const MAX_LATENCY_S: u32 = 10;

/// The most parameters a plugin may have. Far more than any needs; a
/// larger count is a broken descriptor.
const MAX_PARAMS: u32 = 4096;

/// The functions of a descriptor, each checked to be there.
#[derive(Clone, Copy)]
struct Fns {
    create: Create,
    destroy: extern "C" fn(*mut c_void),
    set_param: extern "C" fn(*mut c_void, u32, f64),
    neutral: extern "C" fn(*const c_void) -> u32,
    latency: extern "C" fn(*const c_void) -> u32,
    process: extern "C" fn(*mut c_void, u32, *mut c_void, u32) -> u32,
    /// Given for counts that differ; `None` for version 1.
    process_into: Option<ProcessInto>,
    /// Optional.
    reset: Option<extern "C" fn(*mut c_void)>,
    /// Given if any parameter is text.
    set_param_text: Option<extern "C" fn(*mut c_void, u32, *const u8, u32) -> u32>,
}

/// `create` as the ABI version has it. A version 1 library has no channel
/// count in it.
#[derive(Clone, Copy)]
enum Create {
    V1(extern "C" fn(u32, u32, *mut *mut c_void) -> u32),
    V2(extern "C" fn(u32, u32, u32, *mut *mut c_void) -> u32),
}

/// `process_into` of the ABI.
type ProcessInto = extern "C" fn(*mut c_void, u32, *const c_void, *mut c_void, u32) -> u32;

impl Fns {
    /// The functions of a descriptor of version `abi`, or the name of the
    /// first one that is missing. Version 1's `create` is read as the
    /// version 1 function, and its `process_into` is `None`: the descriptor
    /// was copied only as far as version 1 goes.
    fn of(d: &Descriptor, abi: u32) -> Result<Fns, &'static str> {
        let create = d.create.ok_or("create")?;
        let create = if abi == 1 {
            // SAFETY: a version 1 library has the version 1 function at this
            // offset, which takes no channel count; both are `extern "C"`
            // function pointers of the same size.
            Create::V1(unsafe {
                std::mem::transmute::<
                    extern "C" fn(u32, u32, u32, *mut *mut c_void) -> u32,
                    extern "C" fn(u32, u32, *mut *mut c_void) -> u32,
                >(create)
            })
        } else {
            Create::V2(create)
        };
        Ok(Fns {
            create,
            destroy: d.destroy.ok_or("destroy")?,
            set_param: d.set_param.ok_or("set_param")?,
            neutral: d.neutral.ok_or("neutral")?,
            latency: d.latency.ok_or("latency")?,
            process: d.process.ok_or("process")?,
            process_into: d.process_into,
            reset: d.reset,
            set_param_text: d.set_param_text,
        })
    }
}

/// The channel counts of a version 2 descriptor, or why they are refused.
fn channels(desc: &Descriptor) -> Result<Channels, String> {
    let (input, output) = (desc.channels_in as usize, desc.channels_out as usize);
    match (input, output) {
        (0, 0) => Ok(Channels::Any),
        (0, _) | (_, 0) => Err(format!(
            "it takes {input} channels in and {output} out; 0 means any count and goes with 0 \
             on both sides"
        )),
        (i, o) if i > MAX_CHANNELS || o > MAX_CHANNELS => Err(format!(
            "it takes {input} channels in and {output} out, more than the {MAX_CHANNELS} allowed"
        )),
        _ => Ok(Channels::Fixed { input, output }),
    }
}

/// The text of the last `dlopen` error.
fn dl_error() -> String {
    // SAFETY: dlerror returns null or a C string valid until the next dl
    // call on this thread, and it is copied before that.
    let e = unsafe { libc::dlerror() };
    if e.is_null() {
        "unknown error".into()
    } else {
        // SAFETY: as above.
        unsafe { CStr::from_ptr(e) }.to_string_lossy().into_owned()
    }
}

/// Copies a C string the library gave.
///
/// # Safety
///
/// `p` must be null or point to a NUL terminated string.
unsafe fn text(p: *const c_char) -> Result<String, String> {
    if p.is_null() {
        return Err("is null".into());
    }
    // SAFETY: as the caller promises.
    let s = unsafe { CStr::from_ptr(p) };
    s.to_str()
        .map(str::to_owned)
        .map_err(|_| "is not UTF-8".into())
}

/// Copies and checks one parameter. A unit may be null.
fn param(info: &ParamInfo) -> Result<Param, String> {
    let param = range(info)?;
    let name = &param.name;
    let whole = |v: f64| v.fract() == 0.0;
    let kind = match info.kind {
        PARAM_FLOAT => ParamKind::Float,
        PARAM_INTEGER => ParamKind::Integer,
        PARAM_SWITCH => ParamKind::Switch,
        PARAM_ENUMERATION => ParamKind::Enumeration(value_names(info, name)?),
        PARAM_TEXT => ParamKind::Text,
        other => {
            return Err(format!(
                "{name}: it has type {other}, which is not one of the ABI's"
            ));
        }
    };
    if info.kind != PARAM_ENUMERATION && (info.value_count != 0 || !info.value_names.is_null()) {
        return Err(format!("{name}: only an enumeration has value names"));
    }
    match &kind {
        ParamKind::Float => {}
        ParamKind::Text => {
            if info.min != 0.0 || info.max != 0.0 || info.default != 0.0 {
                return Err(format!("{name}: a text parameter has range and default 0"));
            }
        }
        ParamKind::Integer => {
            if ![info.min, info.max, info.default].into_iter().all(whole) {
                return Err(format!(
                    "{name}: an integer needs a whole range and default, not {} to {} and {}",
                    info.min, info.max, info.default
                ));
            }
        }
        ParamKind::Switch => {
            if info.min != 0.0 || info.max != 1.0 || !whole(info.default) {
                return Err(format!(
                    "{name}: a switch goes from 0 to 1 and starts at one of them, not {} to {} \
                     and {}",
                    info.min, info.max, info.default
                ));
            }
        }
        ParamKind::Enumeration(names) => {
            let last = names.len() as f64 - 1.0;
            if info.min != 0.0 || info.max != last || !whole(info.default) {
                return Err(format!(
                    "{name}: an enumeration of {} values goes from 0 to {last} and starts at \
                     one of them, not {} to {} and {}",
                    names.len(),
                    info.min,
                    info.max,
                    info.default
                ));
            }
        }
    }
    Ok(Param { kind, ..param })
}

/// The most values an enumeration may have.
const MAX_VALUES: u32 = 4096;

/// Copies the names of an enumeration's values.
fn value_names(info: &ParamInfo, name: &str) -> Result<Vec<String>, String> {
    let count = info.value_count;
    if count == 0 || count > MAX_VALUES {
        return Err(format!(
            "{name}: an enumeration has 1 to {MAX_VALUES} values, not {count}"
        ));
    }
    if info.value_names.is_null() || !info.value_names.is_aligned() {
        return Err(format!(
            "{name}: an enumeration needs an aligned list of value names"
        ));
    }
    // SAFETY: not null and aligned, and the ABI promises `value_count`
    // names there for as long as the library is loaded.
    let names = unsafe { std::slice::from_raw_parts(info.value_names, count as usize) };
    names
        .iter()
        .enumerate()
        // SAFETY: the ABI promises each is a C string that lives as long
        // as the library; `text` refuses a null one.
        .map(|(i, &p)| unsafe { text(p) }.map_err(|e| format!("{name}: the name of value {i} {e}")))
        .collect()
}

/// Copies one parameter and checks its range, whatever its type.
fn range(info: &ParamInfo) -> Result<Param, String> {
    // SAFETY: the ABI promises the name and unit are null or C strings
    // that live as long as the library.
    let name = unsafe { text(info.name) }.map_err(|e| format!("its name {e}"))?;
    let unit = if info.unit.is_null() {
        String::new()
    } else {
        // SAFETY: as above.
        unsafe { text(info.unit) }.map_err(|e| format!("{name}: its unit {e}"))?
    };
    // Clamping to such a range would panic.
    if info.min.is_nan() || info.max.is_nan() || info.min > info.max {
        return Err(format!(
            "{name}: the range {} to {} is not a range",
            info.min, info.max
        ));
    }
    // Also refuses a NaN default.
    if !(info.min..=info.max).contains(&info.default) {
        return Err(format!(
            "{name}: the default {} lies outside {} to {}",
            info.default, info.min, info.max
        ));
    }
    Ok(Param {
        name,
        unit,
        kind: ParamKind::Float,
        min: info.min,
        max: info.max,
        default: info.default,
    })
}

/// One instance of a library's plugin.
struct Native {
    fns: Fns,
    /// What `create` returned. Freed by `destroy` on drop.
    instance: *mut c_void,
    name: String,
    formats: Formats,
    channels: Channels,
    /// The channels of the blocks the instance was created for, and of the
    /// blocks it gives. They differ only for `Channels::Fixed` counts that
    /// do.
    input: usize,
    output: usize,
    params: Vec<Param>,
    /// Not 0, and fits in a `u32`: `create` checked both.
    max_frames: usize,
    /// `MAX_LATENCY_S` in frames at the instance's rate.
    max_latency: u32,
}

impl Native {
    /// Passes `data` to the plugin in parts of at most `max_frames`
    /// frames, and returns the status bits of all parts together.
    fn run<T>(&mut self, format: u32, data: &mut [T]) -> u32 {
        let mut bits = 0;
        for part in data.chunks_mut(self.max_frames * self.input) {
            // The part is the `frames * channels` interleaved samples the
            // ABI passes. The frame count fits, as it is at most
            // `max_frames`.
            let frames = (part.len() / self.input) as u32;
            bits |= (self.fns.process)(self.instance, format, part.as_mut_ptr().cast(), frames);
        }
        bits
    }

    /// Passes `input` and `output`, the same number of frames, to
    /// `process_into` in parts of at most `max_frames` frames, and returns
    /// the status bits of all parts together.
    fn run_into<T>(
        &mut self,
        process_into: ProcessInto,
        format: u32,
        input: &mut [T],
        output: &mut [T],
    ) -> u32 {
        let mut bits = 0;
        let ins = input.chunks_mut(self.max_frames * self.input);
        let outs = output.chunks_mut(self.max_frames * self.output);
        for (i, o) in ins.zip(outs) {
            let frames = (i.len() / self.input) as u32;
            bits |= process_into(
                self.instance,
                format,
                i.as_ptr().cast(),
                o.as_mut_ptr().cast(),
                frames,
            );
        }
        bits
    }
}

// SAFETY: the ABI lets an instance move between threads as long as two
// never use it at once, which `&mut self` and ownership guarantee here.
// `Native` is not `Sync`.
unsafe impl Send for Native {}

impl Plugin for Native {
    fn name(&self) -> &str {
        &self.name
    }

    fn formats(&self) -> Formats {
        self.formats
    }

    fn channels(&self) -> Channels {
        self.channels
    }

    fn params(&self) -> &[Param] {
        &self.params
    }

    /// An index beyond the parameters never reaches the library, and a
    /// value is clamped to the parameter's range, and rounded to a whole
    /// number unless the parameter is a float. NaN is ignored.
    fn set_param(&mut self, index: usize, value: f64) {
        if let Some(p) = self.params.get(index)
            && !value.is_nan()
        {
            if p.kind == ParamKind::Text {
                // Text is set with `set_text`, while the chain is built.
                return;
            }
            let mut value = value.clamp(p.min, p.max);
            if p.kind != ParamKind::Float {
                // A whole range keeps a rounded value inside it.
                value = value.round();
            }
            // The index fits: there are at most `param_count` parameters.
            (self.fns.set_param)(self.instance, index as u32, value);
        }
    }

    /// A plugin whose counts differ is never neutral.
    fn neutral(&self) -> bool {
        self.input == self.output && (self.fns.neutral)(self.instance) != 0
    }

    fn set_text(&mut self, index: usize, text: &str) -> Result<(), (Failure, String)> {
        let config = |why: String| (Failure::Config, why);
        let (Some(p), Some(set)) = (self.params.get(index), self.fns.set_param_text) else {
            return Err(config("it takes no text".into()));
        };
        if p.kind != ParamKind::Text {
            return Err(config(format!("{} is not a text parameter", p.name)));
        }
        let len = u32::try_from(text.len()).map_err(|_| config("the text is too long".into()))?;
        // The index fits, as in `set_param`. The text is valid for the
        // call, as the ABI says; an empty one has a dangling pointer, which
        // the ABI says not to read.
        match set(self.instance, index as u32, text.as_ptr(), len) {
            TEXT_OK => Ok(()),
            TEXT_UNSUPPORTED => Err((Failure::Unsupported, "not usable at this rate".into())),
            code => Err(config(format!("the plugin refused it (status {code})"))),
        }
    }

    fn reset(&mut self) {
        if let Some(reset) = self.fns.reset {
            reset(self.instance);
        }
    }

    /// At most `MAX_LATENCY_S` at the rate the instance was built for. A
    /// longer one was logged when it was built, if it was reported then; on
    /// the audio thread it is capped without a word.
    fn latency_frames(&self) -> u32 {
        (self.fns.latency)(self.instance).min(self.max_latency)
    }

    /// A block in a format the plugin did not name never reaches it and is
    /// left as it is, and neither does one whose channel count is not the
    /// one the instance was created for. A chain converts and fits such
    /// blocks, so it does not happen there. A block longer than `max_frames`
    /// is passed in parts.
    fn process(&mut self, block: Block<'_>) -> Status {
        if self.input != self.output
            || block.channels != self.input
            || !self.formats.has(block.format())
        {
            return Status::default();
        }
        let bits = match block.samples {
            Samples::I32(d) => self.run(FORMAT_I32, d),
            Samples::F32(d) => self.run(FORMAT_F32, d),
            Samples::F64(d) => self.run(FORMAT_F64, d),
        };
        Status {
            no_signal: bits & STATUS_NO_SIGNAL != 0,
            changed: false,
        }
    }

    /// As `process`, and `output` must have as many frames as `input` and
    /// be in the same format, else nothing is passed and `output` is
    /// filled with zeros.
    fn process_into(&mut self, input: Block<'_>, mut output: Block<'_>) -> Status {
        let fits = input.channels == self.input
            && output.channels == self.output
            && input.frames() == output.frames()
            && self.formats.has(input.format());
        let bits = match (self.fns.process_into, input.samples, &mut output.samples) {
            (Some(f), Samples::I32(i), Samples::I32(o)) if fits => {
                self.run_into(f, FORMAT_I32, i, o)
            }
            (Some(f), Samples::F32(i), Samples::F32(o)) if fits => {
                self.run_into(f, FORMAT_F32, i, o)
            }
            (Some(f), Samples::F64(i), Samples::F64(o)) if fits => {
                self.run_into(f, FORMAT_F64, i, o)
            }
            _ => {
                // A block that is not passed on is silence, never what the
                // output held before.
                match &mut output.samples {
                    Samples::I32(o) => o.fill(0),
                    Samples::F32(o) => o.fill(0.0),
                    Samples::F64(o) => o.fill(0.0),
                }
                return Status::default();
            }
        };
        Status {
            no_signal: bits & STATUS_NO_SIGNAL != 0,
            changed: false,
        }
    }
}

impl Drop for Native {
    fn drop(&mut self) {
        (self.fns.destroy)(self.instance);
    }
}
