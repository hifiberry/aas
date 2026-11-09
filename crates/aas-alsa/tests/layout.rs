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
        SND_PCM_STATE_PREPARED,
        SND_PCM_STATE_RUNNING,
        SND_PCM_STATE_XRUN,
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
fn ioplug_layout_matches_the_header() {
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
