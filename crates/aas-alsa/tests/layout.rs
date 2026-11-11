//! Proves that `ffi.rs` matches the installed ALSA headers. A mismatch here
//! would corrupt a player's memory, so it fails the build's tests instead.

use aas_alsa::ffi::*;
use std::collections::BTreeMap;
use std::mem::{offset_of, size_of};
use std::path::Path;
use std::process::Command;

macro_rules! size {
    ($t:ident) => {
        (format!("size {}", stringify!($t)), size_of::<$t>() as i64)
    };
}

macro_rules! offset {
    ($t:ident, $($f:ident),+) => {
        [$((
            format!("offset {}.{}", stringify!($t), stringify!($f)),
            offset_of!($t, $f) as i64,
        )),+]
    };
}

macro_rules! value {
    ($($c:ident),+) => {
        [$((format!("value {}", stringify!($c)), $c as i64)),+]
    };
}

/// What the Rust declarations say, keyed as `layout.c` prints it.
fn rust_layout() -> BTreeMap<String, i64> {
    let mut all = Vec::new();
    all.push(size!(snd_pcm_ioplug_t));
    all.extend(offset!(
        snd_pcm_ioplug_t,
        version,
        name,
        flags,
        poll_fd,
        poll_events,
        mmap_rw,
        callback,
        private_data,
        pcm,
        stream,
        state,
        appl_ptr,
        hw_ptr,
        nonblock,
        access,
        format,
        channels,
        rate,
        period_size,
        buffer_size
    ));
    all.push(size!(snd_pcm_ioplug_callback_t));
    all.extend(offset!(
        snd_pcm_ioplug_callback_t,
        start,
        stop,
        pointer,
        transfer,
        close,
        hw_params,
        hw_free,
        sw_params,
        prepare,
        drain,
        pause,
        resume,
        poll_descriptors_count,
        poll_descriptors,
        poll_revents,
        dump,
        delay,
        query_chmaps,
        get_chmap,
        set_chmap
    ));
    all.push(size!(snd_pcm_channel_area_t));
    all.extend(offset!(snd_pcm_channel_area_t, addr, first, step));
    all.extend([
        size!(snd_pcm_uframes_t),
        size!(snd_pcm_sframes_t),
        size!(snd_pcm_stream_t),
        size!(snd_pcm_state_t),
        size!(snd_pcm_access_t),
        size!(snd_pcm_format_t),
    ]);
    all.extend(value!(
        SND_PCM_IOPLUG_VERSION,
        SND_PCM_IOPLUG_FLAG_BOUNDARY_WA,
        SND_PCM_IOPLUG_HW_ACCESS,
        SND_PCM_IOPLUG_HW_FORMAT,
        SND_PCM_IOPLUG_HW_CHANNELS,
        SND_PCM_IOPLUG_HW_RATE,
        SND_PCM_IOPLUG_HW_PERIOD_BYTES,
        SND_PCM_IOPLUG_HW_BUFFER_BYTES,
        SND_PCM_IOPLUG_HW_PERIODS,
        SND_PCM_STREAM_PLAYBACK,
        SND_PCM_STREAM_CAPTURE,
        SND_PCM_NONBLOCK,
        SND_PCM_STATE_SETUP,
        SND_PCM_STATE_PREPARED,
        SND_PCM_STATE_RUNNING,
        SND_PCM_STATE_XRUN,
        SND_PCM_STATE_DRAINING,
        SND_PCM_STATE_PAUSED,
        SND_PCM_STATE_DISCONNECTED,
        SND_PCM_ACCESS_MMAP_INTERLEAVED,
        SND_PCM_ACCESS_RW_INTERLEAVED,
        SND_PCM_FORMAT_S16_LE,
        SND_PCM_FORMAT_S24_LE,
        SND_PCM_FORMAT_S24_3LE,
        SND_PCM_FORMAT_S32_LE,
        SND_PCM_FORMAT_FLOAT_LE
    ));
    all.push(size!(snd_ctl_ext_t));
    all.extend(offset!(
        snd_ctl_ext_t,
        version,
        card_idx,
        id,
        driver,
        name,
        longname,
        mixername,
        poll_fd,
        callback,
        private_data,
        handle,
        nonblock,
        subscribed,
        tlv
    ));
    all.push(size!(snd_ctl_ext_callback_t));
    all.extend(offset!(
        snd_ctl_ext_callback_t,
        close,
        elem_count,
        elem_list,
        find_elem,
        free_key,
        get_attribute,
        get_integer_info,
        get_integer64_info,
        get_enumerated_info,
        get_enumerated_name,
        read_integer,
        read_integer64,
        read_enumerated,
        read_bytes,
        read_iec958,
        write_integer,
        write_integer64,
        write_enumerated,
        write_bytes,
        write_iec958,
        subscribe_events,
        read_event,
        poll_descriptors_count,
        poll_descriptors,
        poll_revents
    ));
    all.extend([
        size!(snd_ctl_ext_key_t),
        size!(snd_ctl_elem_type_t),
        size!(snd_ctl_elem_iface_t),
    ]);
    all.extend(value!(
        SND_CTL_EXT_VERSION,
        SND_CTL_EXT_ACCESS_READ,
        SND_CTL_EXT_ACCESS_WRITE,
        SND_CTL_EXT_ACCESS_READWRITE,
        SND_CTL_EXT_ACCESS_TLV_READ,
        SND_CTL_EXT_KEY_NOT_FOUND,
        SND_CTL_ELEM_TYPE_INTEGER,
        SND_CTL_ELEM_IFACE_MIXER,
        SND_CTL_EVENT_MASK_VALUE,
        SND_CTL_TLVT_DB_MINMAX_MUTE
    ));
    let mut map = BTreeMap::new();
    for (key, value) in all {
        assert!(map.insert(key.clone(), value).is_none(), "{key} twice");
    }
    map
}

/// What the C compiler says, from the real headers.
fn header_layout() -> BTreeMap<String, i64> {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/layout.c");
    let binary = Path::new(env!("CARGO_TARGET_TMPDIR")).join("layout");
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let status = Command::new(&cc)
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .status()
        .unwrap_or_else(|e| panic!("cannot run {cc}: {e}"));
    assert!(status.success(), "{cc} failed on {}", source.display());
    let output = Command::new(&binary).output().expect("run layout");
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).expect("utf-8");
    let mut map = BTreeMap::new();
    for line in text.lines() {
        let (key, value) = line.rsplit_once(' ').expect("key and value");
        let value = value.parse().expect("a number");
        assert!(map.insert(key.to_string(), value).is_none(), "{key} twice");
    }
    map
}

#[test]
fn ffi_layout_matches_the_headers() {
    let header = header_layout();
    let rust = rust_layout();
    let mut wrong = Vec::new();
    for key in header.keys().chain(rust.keys()) {
        let (c, r) = (header.get(key), rust.get(key));
        if c != r && !wrong.iter().any(|(k, _, _)| k == key) {
            wrong.push((key.clone(), c.copied(), r.copied()));
        }
    }
    assert!(
        wrong.is_empty(),
        "header and ffi.rs differ (key, header, rust): {wrong:#?}"
    );
}

/// Rust type text, as `stringify!` gives it, in tokens.
fn tokens(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c.is_whitespace() {
            i += 1;
        } else if c.is_alphanumeric() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            out.push(chars[start..i].iter().collect());
        } else if c == '"' {
            let end = i + 1 + chars[i + 1..].iter().position(|&c| c == '"').unwrap();
            out.push(chars[i..=end].iter().collect());
            i = end + 1;
        } else if (c, next) == ('-', Some('>')) || (c, next) == (':', Some(':')) {
            out.push(format!("{c}{}", next.unwrap()));
            i += 2;
        } else {
            out.push(c.to_string());
            i += 1;
        }
    }
    out
}

/// The C name of a type ffi.rs names.
fn c_name(rust: &str) -> &str {
    match rust {
        "Io" => "snd_pcm_ioplug_t *",
        "Ext" => "snd_ctl_ext_t *",
        "Key" => "snd_ctl_ext_key_t",
        "c_int" => "int",
        "c_uint" => "unsigned int",
        "c_long" => "long",
        "c_ulong" => "unsigned long",
        "c_ushort" => "unsigned short",
        "c_char" => "char",
        "c_uchar" => "unsigned char",
        "c_void" => "void",
        "i64" => "int64_t",
        "usize" => "size_t",
        "pollfd" => "struct pollfd",
        other => other,
    }
}

/// One type from `t[*i]` on, in C.
fn c_type(t: &[String], i: &mut usize) -> String {
    if t[*i] == "*" {
        let constness = t[*i + 1].clone();
        *i += 2;
        let inner = c_type(t, i);
        return match constness.as_str() {
            "const" => format!("const {inner} *"),
            _ => format!("{inner} *"),
        };
    }
    if t[*i] == "Option" {
        // Option<unsafe extern "C" fn(params) -> ret>: a function pointer.
        let open = *i + t[*i..].iter().position(|s| s == "fn").unwrap() + 1;
        assert_eq!(t[open], "(");
        *i = open + 1;
        let mut params = Vec::new();
        while t[*i] != ")" {
            params.push(c_type(t, i));
            if t[*i] == "," {
                *i += 1;
            }
        }
        *i += 1;
        let ret = if t[*i] == "->" {
            *i += 1;
            c_type(t, i)
        } else {
            "void".to_string()
        };
        // rustfmt leaves a comma after a type broken over lines.
        if t[*i] == "," {
            *i += 1;
        }
        assert_eq!(t[*i], ">");
        *i += 1;
        return format!("{ret} (*)({})", params.join(", "));
    }
    let mut name = t[*i].clone();
    *i += 1;
    while t.get(*i).is_some_and(|s| s == "::") {
        name = t[*i + 1].clone();
        *i += 2;
    }
    c_name(&name).to_string()
}

fn in_c(rust: &str) -> String {
    let t = tokens(rust);
    let mut i = 0;
    let c = c_type(&t, &mut i);
    assert_eq!(i, t.len(), "not all of {rust} was read");
    c
}

/// The callback tables and the TLV union, field by field, must have the
/// types the headers give them, parameter for parameter. Lengths alone
/// would not catch a swapped or wrongly typed parameter.
///
/// Tried once on the Pi 3 test board, with `pause` declared to take an
/// unsigned int. Changed in `ffi.rs` alone, the crate does not build: the
/// plugin's own callback no longer fits the table (E0308, mismatched
/// types). Changed in `ffi.rs` and in the callback, so that Rust agrees
/// with itself, the crate builds and this test fails with `static
/// assertion failed: "((snd_pcm_ioplug_callback_t *)0)->pause"`, while
/// `ffi_layout_matches_the_headers` still passes, since the size of the
/// table is the same.
#[test]
fn callback_signatures_match_the_headers() {
    let tables: [(&str, &[(&str, &str)]); 3] = [
        (
            "((snd_pcm_ioplug_callback_t *)0)->",
            snd_pcm_ioplug_callback_t::SIGNATURES,
        ),
        (
            "((snd_ctl_ext_callback_t *)0)->",
            snd_ctl_ext_callback_t::SIGNATURES,
        ),
        ("((snd_ctl_ext_t *)0)->tlv.", snd_ctl_ext_tlv::SIGNATURES),
    ];
    let mut source = String::from(
        "#include <stdint.h>\n\
         #include <alsa/asoundlib.h>\n\
         #include <alsa/pcm_external.h>\n\
         #include <alsa/control_external.h>\n\
         #define SAME(field, ...) _Static_assert(\
         __builtin_types_compatible_p(__typeof__(field), __VA_ARGS__), #field);\n",
    );
    let mut count = 0;
    for (path, fields) in tables {
        for (field, rust) in fields {
            source += &format!("SAME({path}{field}, {})\n", in_c(rust));
            count += 1;
        }
    }
    assert_eq!(count, 20 + 2 + 25, "a field is missing from SIGNATURES");
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let file = dir.join("signatures.c");
    std::fs::write(&file, &source).unwrap();
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let output = Command::new(&cc)
        .arg("-fsyntax-only")
        .arg(&file)
        .output()
        .unwrap_or_else(|e| panic!("cannot run {cc}: {e}"));
    assert!(
        output.status.success(),
        "the headers differ from ffi.rs:\n{}\n{source}",
        String::from_utf8_lossy(&output.stderr)
    );
}
