//! Plugins from shared libraries, through the C ABI of `aas-plugin`.
//!
//! A library is loaded once and never unloaded, so its code and statics
//! outlive every instance built from it. Loading checks everything the
//! ABI promises before a single instance is built, and an instance copies
//! what it reports into Rust values when it is built, so the calls on the
//! audio thread allocate nothing.

use super::{Block, Formats, Param, Plugin, Status};
use aas_plugin::{
    ABI, Descriptor, FORMAT_F32, FORMAT_F64, FORMAT_I32, ParamInfo, STATUS_NO_SIGNAL,
};
use std::ffi::{CStr, CString, c_char, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// A loaded plugin library, from which instances are built.
pub struct Library {
    /// The path, for messages.
    path: String,
    /// Points into the library, which is never unloaded.
    desc: &'static Descriptor,
    name: String,
    formats: Formats,
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
        // SAFETY: the ABI promises a descriptor that lives and stays as it
        // is as long as the library, and the library is never unloaded.
        let desc: &'static Descriptor = unsafe { &*desc };
        if desc.abi != ABI {
            return Err(fail(format!(
                "it has ABI version {}, this daemon takes {ABI}",
                desc.abi
            )));
        }
        if desc.channels_in != 2 || desc.channels_out != 2 {
            return Err(fail(format!(
                "it takes {} channels in and {} out; plugins are stereo, other \
                 channel counts come with the matrix in stage 3b",
                desc.channels_in, desc.channels_out
            )));
        }
        let formats = Formats {
            i32: desc.formats & FORMAT_I32 != 0,
            f32: desc.formats & FORMAT_F32 != 0,
            f64: desc.formats & FORMAT_F64 != 0,
        };
        if !(formats.i32 || formats.f32 || formats.f64) {
            return Err(fail("it names no format it takes".into()));
        }
        // SAFETY: the ABI promises the name is null or a C string that
        // lives as long as the library.
        let name = unsafe { text(desc.name) }.map_err(|e| fail(format!("its name {e}")))?;
        let params = if desc.param_count == 0 {
            Vec::new()
        } else if desc.params.is_null() {
            return Err(fail(format!(
                "it has {} parameters but no list of them",
                desc.param_count
            )));
        } else {
            // SAFETY: the ABI promises `param_count` parameters at
            // `params` for as long as the library is loaded.
            let infos =
                unsafe { std::slice::from_raw_parts(desc.params, desc.param_count as usize) };
            infos
                .iter()
                .enumerate()
                .map(|(i, info)| param(info).map_err(|e| fail(format!("parameter {i}: {e}"))))
                .collect::<Result<_, _>>()?
        };
        Ok(Library {
            path: shown,
            desc,
            name,
            formats,
            params,
        })
    }

    /// Builds an instance for `rate` Hz and blocks of up to `max_frames`
    /// frames, with its parameters at their defaults. Runs on the control
    /// thread.
    pub fn create(&self, rate: u32, max_frames: usize) -> Result<Box<dyn Plugin>, String> {
        let fail = |why: &str| format!("plugin {} from {}: {why}", self.name, self.path);
        let frames = u32::try_from(max_frames).map_err(|_| fail("blocks too long"))?;
        let instance = (self.desc.create)(rate, frames);
        if instance.is_null() {
            return Err(fail("its create returned null"));
        }
        Ok(Box::new(Native {
            desc: self.desc,
            instance,
            name: self.name.clone(),
            formats: self.formats,
            params: self.params.clone(),
            max_frames,
        }))
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
    Ok(Param {
        name,
        unit,
        min: info.min,
        max: info.max,
        default: info.default,
    })
}

/// One instance of a library's plugin.
struct Native {
    desc: &'static Descriptor,
    /// What `create` returned. Freed by `destroy` on drop.
    instance: *mut c_void,
    name: String,
    formats: Formats,
    params: Vec<Param>,
    max_frames: usize,
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

    fn params(&self) -> &[Param] {
        &self.params
    }

    /// An index beyond the parameters never reaches the library, and a
    /// value is clamped to the parameter's range. NaN is ignored.
    fn set_param(&mut self, index: usize, value: f64) {
        if let Some(p) = self.params.get(index)
            && !value.is_nan()
        {
            // The index fits: there are at most `param_count` parameters.
            (self.desc.set_param)(self.instance, index as u32, value.clamp(p.min, p.max));
        }
    }

    fn neutral(&self) -> bool {
        (self.desc.neutral)(self.instance) != 0
    }

    fn latency_frames(&self) -> u32 {
        (self.desc.latency)(self.instance)
    }

    fn process(&mut self, block: Block<'_>) -> Status {
        debug_assert!(self.formats.has(block.format()));
        let (format, samples, frames): (u32, *mut c_void, usize) = match block {
            Block::I32(d) => (FORMAT_I32, d.as_mut_ptr().cast(), d.len()),
            Block::F32(d) => (FORMAT_F32, d.as_mut_ptr().cast(), d.len()),
            Block::F64(d) => (FORMAT_F64, d.as_mut_ptr().cast(), d.len()),
        };
        debug_assert!(frames <= self.max_frames);
        // A frame is two samples with no padding, so the block is the
        // `2 * frames` interleaved samples the ABI passes. `frames` fits:
        // `create` checked that `max_frames` does.
        let bits = (self.desc.process)(self.instance, format, samples, frames as u32);
        Status {
            no_signal: bits & STATUS_NO_SIGNAL != 0,
        }
    }
}

impl Drop for Native {
    fn drop(&mut self) {
        (self.desc.destroy)(self.instance);
    }
}
