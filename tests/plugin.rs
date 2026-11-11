//! Plugins loaded from shared libraries: the example gain plugin and a
//! fixture that breaks the ABI in the ways the loader refuses.

use aas::plugin::native::Library;
use aas::plugin::{Block, Formats, ParamKind, Plugin, Status};
use assert_no_alloc::assert_no_alloc;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

// Makes `assert_no_alloc` see every allocation in these tests.
#[global_allocator]
static ALLOCATOR: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

/// The gain plugin and the fixture, built once per test process in the
/// profile of the tests.
struct Libraries {
    gain: PathBuf,
    fixture: PathBuf,
}

/// Builds the libraries and takes their paths from cargo's own report, so
/// a `--target` or a custom target directory finds them too.
fn libraries() -> &'static Libraries {
    static LIBS: OnceLock<Libraries> = OnceLock::new();
    LIBS.get_or_init(|| {
        let mut cmd = Command::new(env!("CARGO"));
        cmd.args([
            "build",
            "--message-format=json",
            "-p",
            "aas-plugin-gain",
            "-p",
            "aas-plugin-fixture-bad",
        ]);
        if !cfg!(debug_assertions) {
            cmd.arg("--release");
        }
        let out = cmd.output().expect("cargo runs");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "building the plugins failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        // The `filenames` of each `compiler-artifact` line. A path is a
        // JSON string, so it lies between two quotes.
        let built: Vec<PathBuf> = stdout
            .lines()
            .filter(|l| l.contains(r#""reason":"compiler-artifact""#))
            .flat_map(|l| l.split('"'))
            .filter(|s| s.ends_with(".so"))
            .map(PathBuf::from)
            .collect();
        let find = |name: &str| {
            built
                .iter()
                .find(|p| p.file_name().is_some_and(|f| f == name))
                .unwrap_or_else(|| panic!("cargo reported no {name}:\n{stdout}"))
                .clone()
        };
        Libraries {
            gain: find("libaas_gain.so"),
            fixture: find("libaas_fixture_bad.so"),
        }
    })
}

fn gain_path() -> PathBuf {
    libraries().gain.clone()
}

fn gain() -> Box<dyn Plugin> {
    Library::open(&gain_path())
        .unwrap()
        .create(48000, 1024)
        .unwrap()
}

fn unwrap_err<T>(r: Result<T, String>) -> String {
    match r {
        Ok(_) => panic!("expected the load to be refused"),
        Err(e) => e,
    }
}

/// Runs `body` on the fixture in `case`. The test re-runs itself as a
/// child process with `AAS_FIXTURE` set, so tests that run at the same
/// time never share the variable. In the child this runs `body` with the
/// fixture's path; the parent checks that the child passed.
fn with_fixture(test: &str, case: &str, body: impl FnOnce(&Path)) {
    if let Some(path) = std::env::var_os("AAS_FIXTURE_LIB") {
        assert_eq!(std::env::var("AAS_FIXTURE").unwrap(), case);
        body(Path::new(&path));
        return;
    }
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env("AAS_FIXTURE", case)
        .env("AAS_FIXTURE_LIB", &libraries().fixture)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("1 passed"),
        "the child failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Loads the fixture in `case` and checks why it was refused with `check`,
/// which is given the message and the path.
fn refused_fixture(test: &str, case: &str, check: impl FnOnce(&str, &str)) {
    with_fixture(test, case, |path| {
        let msg = unwrap_err(Library::open(path).and_then(|l| l.create(48000, 1024)));
        check(&msg, path.to_str().unwrap());
    });
}

#[test]
fn a_loaded_plugin_processes_audio() {
    let mut p = gain();
    assert_eq!(p.name(), "gain");
    assert_eq!(
        p.formats(),
        Formats {
            i32: false,
            f32: true,
            f64: true
        }
    );
    assert_eq!(p.latency_frames(), 0);
    p.set_param(0, -6.02);
    assert!(!p.neutral());
    let mut f = [[0.5f32, -1.0]; 64];
    let mut d = [[0.5f64, -1.0]; 64];
    let (sf, sd) = assert_no_alloc(|| {
        let sf = p.process(Block::F32(&mut f));
        let sd = p.process(Block::F64(&mut d));
        (sf, sd)
    });
    assert!(!sf.no_signal && !sd.no_signal);
    for x in f {
        assert!(
            (x[0] - 0.25).abs() < 1e-4 && (x[1] + 0.5).abs() < 1e-4,
            "{x:?}"
        );
    }
    for x in d {
        assert!(
            (x[0] - 0.25).abs() < 1e-4 && (x[1] + 0.5).abs() < 1e-4,
            "{x:?}"
        );
    }
    // Out of range values are clamped to the range, -60 dB here.
    p.set_param(0, -1000.0);
    let mut d = [[1.0f64, -1.0]];
    p.process(Block::F64(&mut d));
    assert!((d[0][0] - 0.001).abs() < 1e-9, "{d:?}");
    // An index beyond the parameters is ignored.
    p.set_param(1, 0.0);
    let mut d = [[1.0f64, -1.0]];
    p.process(Block::F64(&mut d));
    assert!((d[0][0] - 0.001).abs() < 1e-9, "{d:?}");
}

#[test]
fn its_parameters_are_described() {
    let p = gain();
    let params = p.params();
    assert_eq!(params.len(), 1);
    let q = &params[0];
    assert_eq!(
        (q.name.as_str(), q.unit.as_str(), q.min, q.max, q.default),
        ("gain", "dB", -60.0, 20.0, 0.0)
    );
    assert_eq!(q.kind, ParamKind::Float);
}

#[test]
fn it_is_neutral_at_its_default() {
    let mut p = gain();
    assert!(p.neutral());
    let mut d = [[0.1f64, -0.9], [1.0 / 3.0, f64::MAX]];
    let d0 = d;
    p.process(Block::F64(&mut d));
    assert_eq!(
        d.map(|x| x.map(f64::to_bits)),
        d0.map(|x| x.map(f64::to_bits))
    );
    p.set_param(0, 3.0);
    assert!(!p.neutral());
    p.set_param(0, 0.0);
    assert!(p.neutral());
}

#[test]
fn a_missing_file_is_refused_with_its_path() {
    let path = Path::new("/nonexistent/libaas_missing.so");
    let msg = unwrap_err(Library::open(path));
    assert!(msg.contains("/nonexistent/libaas_missing.so"), "{msg}");
}

#[test]
fn a_library_without_the_symbol_is_refused() {
    // The C library this process already uses has no `aas_plugin`.
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
    let libc = maps
        .lines()
        .filter_map(|l| l.split_whitespace().nth(5))
        .find(|p| p.ends_with("/libc.so.6"))
        .expect("libc.so.6 is mapped");
    let msg = unwrap_err(Library::open(Path::new(libc)));
    assert!(msg.contains(libc), "{msg}");
    assert!(msg.contains("aas_plugin"), "{msg}");
}

#[test]
fn a_library_without_a_descriptor_is_refused() {
    refused_fixture(
        "a_library_without_a_descriptor_is_refused",
        "nodesc",
        |msg, path| {
            assert!(msg.contains(path), "{msg}");
            assert!(msg.contains("no descriptor"), "{msg}");
        },
    );
}

#[test]
fn a_library_of_another_abi_version_is_refused() {
    refused_fixture(
        "a_library_of_another_abi_version_is_refused",
        "abi",
        |msg, path| {
            assert!(msg.contains(path), "{msg}");
            assert!(msg.contains("99"), "{msg}");
        },
    );
}

#[test]
fn a_plugin_that_is_not_stereo_is_refused() {
    refused_fixture(
        "a_plugin_that_is_not_stereo_is_refused",
        "mono",
        |msg, path| {
            assert!(msg.contains(path), "{msg}");
            assert!(msg.contains("stage 3b"), "{msg}");
        },
    );
}

#[test]
fn a_plugin_whose_create_returns_null_is_refused() {
    refused_fixture(
        "a_plugin_whose_create_returns_null_is_refused",
        "null",
        |msg, path| {
            assert!(msg.contains(path), "{msg}");
            assert!(msg.contains("create"), "{msg}");
        },
    );
}

#[test]
fn two_instances_do_not_share_state() {
    let lib = Library::open(&gain_path()).unwrap();
    let mut a = lib.create(48000, 1024).unwrap();
    let mut b = lib.create(48000, 1024).unwrap();
    a.set_param(0, -6.02);
    assert!(!a.neutral());
    assert!(b.neutral());
    let mut da = [[1.0f64, 1.0]];
    let mut db = [[1.0f64, 1.0]];
    a.process(Block::F64(&mut da));
    b.process(Block::F64(&mut db));
    assert!((da[0][0] - 0.5).abs() < 1e-4, "{da:?}");
    assert_eq!(db, [[1.0, 1.0]]);
    drop(a);
    // The other instance still works after the first is destroyed.
    b.set_param(0, 20.0);
    b.process(Block::F64(&mut db));
    assert!((db[0][0] - 10.0).abs() < 1e-9, "{db:?}");
}

#[test]
fn a_block_in_a_format_the_plugin_lacks_is_left_alone() {
    // The gain plugin takes no i32. A chain never hands it one, but if
    // anything did, the block must not reach the library.
    let mut p = gain();
    p.set_param(0, -6.02);
    let mut i = [[1000, -1000]; 4];
    assert_eq!(p.process(Block::I32(&mut i)), Status::default());
    assert_eq!(i, [[1000, -1000]; 4]);
}

#[test]
fn a_block_longer_than_max_frames_is_passed_in_parts() {
    // The `chunks` fixture writes the frame count of each call into the
    // samples it was given, and reports no signal for an odd count.
    with_fixture(
        "a_block_longer_than_max_frames_is_passed_in_parts",
        "chunks",
        |path| {
            let mut p = Library::open(path).unwrap().create(48000, 4).unwrap();
            let mut f = [[0.0f32; 2]; 9];
            let s = assert_no_alloc(|| p.process(Block::F32(&mut f)));
            let counts: Vec<f32> = f.iter().map(|x| x[0]).collect();
            assert_eq!(counts, [4.0, 4.0, 4.0, 4.0, 4.0, 4.0, 4.0, 4.0, 1.0]);
            assert!(f.iter().all(|x| x[0] == x[1]));
            // Only the last part had an odd count; its bit is kept.
            assert!(s.no_signal);
            let mut f = [[0.0f32; 2]; 8];
            assert!(!p.process(Block::F32(&mut f)).no_signal);
            assert!(f.iter().all(|x| x[0] == 4.0));
        },
    );
}

#[test]
fn a_plugin_without_a_function_is_refused() {
    refused_fixture(
        "a_plugin_without_a_function_is_refused",
        "nofn",
        |msg, path| {
            assert!(msg.contains(path), "{msg}");
            assert!(msg.contains("process"), "{msg}");
        },
    );
}

#[test]
fn a_parameter_whose_default_is_outside_its_range_is_refused() {
    refused_fixture(
        "a_parameter_whose_default_is_outside_its_range_is_refused",
        "default",
        |msg, path| {
            assert!(msg.contains(path), "{msg}");
            assert!(msg.contains("default"), "{msg}");
        },
    );
}

#[test]
fn typed_parameters_are_described() {
    with_fixture("typed_parameters_are_described", "typed", |path| {
        let p = Library::open(path).unwrap().create(48000, 1024).unwrap();
        let kinds: Vec<(&str, &ParamKind)> = p
            .params()
            .iter()
            .map(|q| (q.name.as_str(), &q.kind))
            .collect();
        let names = ["riaa", "flat", "nab"].map(String::from).to_vec();
        assert_eq!(
            kinds,
            [
                ("level", &ParamKind::Float),
                ("taps", &ParamKind::Integer),
                ("bypass", &ParamKind::Switch),
                ("curve", &ParamKind::Enumeration(names)),
            ]
        );
    });
}

#[test]
fn a_parameter_of_an_unknown_type_is_refused() {
    refused_fixture(
        "a_parameter_of_an_unknown_type_is_refused",
        "badkind",
        |msg, path| {
            assert!(msg.contains(path), "{msg}");
            assert!(msg.contains("type 9"), "{msg}");
        },
    );
}

#[test]
fn an_integer_parameter_with_a_fraction_is_refused() {
    refused_fixture(
        "an_integer_parameter_with_a_fraction_is_refused",
        "badint",
        |msg, _| assert!(msg.contains("taps") && msg.contains("whole"), "{msg}"),
    );
}

#[test]
fn a_switch_that_is_not_0_to_1_is_refused() {
    refused_fixture(
        "a_switch_that_is_not_0_to_1_is_refused",
        "badswitch",
        |msg, _| assert!(msg.contains("bypass") && msg.contains("0 to 1"), "{msg}"),
    );
}

#[test]
fn an_enumeration_without_a_name_for_each_value_is_refused() {
    refused_fixture(
        "an_enumeration_without_a_name_for_each_value_is_refused",
        "badenum",
        |msg, _| assert!(msg.contains("curve") && msg.contains("names"), "{msg}"),
    );
}

#[test]
fn value_names_on_a_parameter_that_is_no_enumeration_are_refused() {
    refused_fixture(
        "value_names_on_a_parameter_that_is_no_enumeration_are_refused",
        "namesonfloat",
        |msg, _| {
            assert!(
                msg.contains("level") && msg.contains("only an enumeration"),
                "{msg}"
            )
        },
    );
}

#[test]
fn an_enumeration_whose_range_is_not_its_values_is_refused() {
    refused_fixture(
        "an_enumeration_whose_range_is_not_its_values_is_refused",
        "enumrange",
        |msg, _| assert!(msg.contains("curve") && msg.contains("0 to 2"), "{msg}"),
    );
}

#[test]
fn an_enumeration_of_more_than_4096_values_is_refused() {
    refused_fixture(
        "an_enumeration_of_more_than_4096_values_is_refused",
        "manyvalues",
        |msg, _| assert!(msg.contains("curve") && msg.contains("5000"), "{msg}"),
    );
}

#[test]
fn an_enumeration_with_a_null_name_is_refused() {
    refused_fixture(
        "an_enumeration_with_a_null_name_is_refused",
        "nullname",
        |msg, _| assert!(msg.contains("value 1") && msg.contains("null"), "{msg}"),
    );
}
