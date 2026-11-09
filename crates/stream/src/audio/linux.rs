//! Linux: records the default output's monitor source through `libpulse-simple`, loaded at run
//! time (no build dependency; PipeWire's PulseAudio server works the same). The monitor carries
//! everything played, our own call included: there is no portable way to exclude one client.

use std::ffi::{CStr, CString, c_char, c_int, c_void};

use libloading::{Library, Symbol};

use super::{AudioSource, AudioSupport, CHUNK_FRAMES, Chunker, SAMPLE_RATE};
use crate::capture::{Ready, Stop};

const PA_STREAM_RECORD: c_int = 2;
const PA_SAMPLE_FLOAT32LE: c_int = 5;

#[repr(C)]
struct SampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}

#[repr(C)]
struct BufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}

type NewFn = unsafe extern "C" fn(
    server: *const c_char,
    name: *const c_char,
    dir: c_int,
    dev: *const c_char,
    stream_name: *const c_char,
    ss: *const SampleSpec,
    map: *const c_void,
    attr: *const BufferAttr,
    error: *mut c_int,
) -> *mut c_void;
type ReadFn = unsafe extern "C" fn(s: *mut c_void, data: *mut c_void, bytes: usize, error: *mut c_int) -> c_int;
type FreeFn = unsafe extern "C" fn(s: *mut c_void);
type StrErrorFn = unsafe extern "C" fn(error: c_int) -> *const c_char;

fn load() -> Option<Library> {
    // SAFETY: libpulse-simple has no load-time side effects beyond libpulse's own init.
    unsafe { Library::new("libpulse-simple.so.0").ok() }
}

/// A PulseAudio server is reachable without connecting: an explicit server, or the per-user
/// socket (also provided by pipewire-pulse).
fn server_present() -> bool {
    if std::env::var_os("PULSE_SERVER").is_some() {
        return true;
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| format!("/run/user/{}", unsafe { libc::getuid() }).into());
    runtime.join("pulse/native").exists() || std::path::Path::new("/var/run/pulse/native").exists()
}

pub(crate) fn support() -> AudioSupport {
    let system = server_present() && load().is_some();
    AudioSupport { system, per_app: false, includes_own_audio: system }
}

pub(crate) fn run(source: AudioSource, mut out: Chunker, ready: Ready, stop: &Stop) -> Result<(), String> {
    if let AudioSource::App { .. } = source {
        ready(Err("sharing one application's audio is not supported on Linux".into()));
        return Ok(());
    }
    let Some(lib) = load() else {
        ready(Err("audio capture needs PulseAudio or PipeWire (libpulse-simple.so.0 not found)".into()));
        return Ok(());
    };
    // SAFETY: the signatures match <pulse/simple.h> and <pulse/error.h>.
    let syms = unsafe {
        (|| -> Result<_, libloading::Error> {
            let new: Symbol<NewFn> = lib.get(b"pa_simple_new\0")?;
            let read: Symbol<ReadFn> = lib.get(b"pa_simple_read\0")?;
            let free: Symbol<FreeFn> = lib.get(b"pa_simple_free\0")?;
            let strerror: Option<Symbol<StrErrorFn>> = lib.get(b"pa_strerror\0").ok();
            Ok((*new, *read, *free, strerror.map(|s| *s)))
        })()
    };
    let Ok((new, read, free, strerror)) = syms else {
        ready(Err("libpulse-simple is incomplete".into()));
        return Ok(());
    };
    let describe = |err: c_int| -> String {
        strerror
            .map(|f| unsafe { f(err) })
            .filter(|p| !p.is_null())
            .map(|p| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("error {err}"))
    };
    let spec = SampleSpec { format: PA_SAMPLE_FLOAT32LE, rate: SAMPLE_RATE, channels: 2 };
    let chunk_bytes = CHUNK_FRAMES * 2 * 4;
    // Small fragments: the server hands over audio every 10 ms instead of in large blocks.
    let attr = BufferAttr {
        maxlength: u32::MAX,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: chunk_bytes as u32,
    };
    let (name, stream, dev) = (
        CString::new("Celerity").unwrap_or_default(),
        CString::new("Screen share audio").unwrap_or_default(),
        CString::new("@DEFAULT_MONITOR@").unwrap_or_default(),
    );
    let mut err: c_int = 0;
    // SAFETY: all pointers are valid for the call; a null server means the default.
    let s = unsafe {
        new(
            std::ptr::null(),
            name.as_ptr(),
            PA_STREAM_RECORD,
            dev.as_ptr(),
            stream.as_ptr(),
            &spec,
            std::ptr::null(),
            &attr,
            &mut err,
        )
    };
    if s.is_null() {
        ready(Err(format!("cannot record the computer's audio: {}", describe(err))));
        return Ok(());
    }
    ready(Ok(()));
    let mut buf = vec![0f32; CHUNK_FRAMES * 2];
    let mut result = Ok(());
    while !stop.stopped() {
        // Blocks for about one chunk (10 ms).
        // SAFETY: `buf` holds `chunk_bytes` bytes.
        let rc = unsafe { read(s, buf.as_mut_ptr().cast(), chunk_bytes, &mut err) };
        if rc < 0 {
            result = Err(format!("audio capture stopped: {}", describe(err)));
            break;
        }
        out.push_stereo(&buf);
    }
    // SAFETY: `s` came from pa_simple_new and is freed once.
    unsafe { free(s) };
    drop(lib);
    result
}
