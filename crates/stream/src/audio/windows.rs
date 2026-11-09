//! Windows: WASAPI process loopback (Windows 10 2004+). "System" records every process except
//! ours and its children (`EXCLUDE_TARGET_PROCESS_TREE` with our pid), so the voice call is not
//! sent back; "App" records only the process tree that owns the shared window. Plain device
//! loopback is deliberately never used: it would include the call.

use std::sync::mpsc;
use std::time::Duration;

use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{BLOB, COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{HRESULT, IUnknown, Interface, Ref, implement};

use super::{AudioSource, AudioSupport, Chunker, SAMPLE_RATE};
use crate::capture::{Ready, Stop};

/// Process loopback arrived in Windows 10 2004 (build 19041).
fn supported() -> bool {
    let mut v =
        OSVERSIONINFOW { dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32, ..Default::default() };
    // RtlGetVersion reports the real version (GetVersionEx lies to unmanifested programs).
    unsafe { RtlGetVersion(&mut v) }.is_ok()
        && (v.dwMajorVersion > 10 || (v.dwMajorVersion == 10 && v.dwBuildNumber >= 19041))
}

pub(crate) fn support() -> AudioSupport {
    let ok = supported();
    AudioSupport { system: ok, per_app: ok, includes_own_audio: false }
}

/// Signals a channel when the asynchronous activation finished.
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated(mpsc::SyncSender<()>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(&self, _op: Ref<IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        let _ = self.0.try_send(());
        Ok(())
    }
}

/// An `IAudioClient` for the process loopback virtual device.
unsafe fn activate(pid: u32, mode: PROCESS_LOOPBACK_MODE) -> Result<IAudioClient, String> {
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: mode,
            },
        },
    };
    let mut pv = PROPVARIANT::default();
    unsafe {
        let inner = &mut *pv.Anonymous.Anonymous;
        inner.vt = VT_BLOB;
        inner.Anonymous.blob = BLOB {
            cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            pBlobData: (&raw const params).cast_mut().cast(),
        };
    }
    let (tx, rx) = mpsc::sync_channel(1);
    let handler: IActivateAudioInterfaceCompletionHandler = Activated(tx).into();
    let op = unsafe {
        ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&pv), &handler)
    }
    .map_err(|e| format!("audio capture is unavailable: {}", e.message()))?;
    rx.recv_timeout(Duration::from_secs(10)).map_err(|_| "audio capture did not start".to_string())?;
    let mut hr = HRESULT(0);
    let mut unk: Option<IUnknown> = None;
    unsafe { op.GetActivateResult(&mut hr, &mut unk) }.map_err(|e| e.message())?;
    hr.ok().map_err(|e| format!("audio capture is unavailable: {}", e.message()))?;
    unk.ok_or("audio capture is unavailable")?.cast::<IAudioClient>().map_err(|e| e.message())
}

/// Sample layout the client was initialised with.
#[derive(Clone, Copy)]
enum Layout {
    F32,
    I16,
}

unsafe fn initialize(client: &IAudioClient) -> Result<Layout, String> {
    // Process loopback has no mix format to ask for; it converts to what we request. Float at
    // 48 kHz first, 16-bit PCM (what Microsoft's sample uses) as a fallback.
    let flags = AUDCLNT_STREAMFLAGS_LOOPBACK
        | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    let mut last = String::new();
    for (layout, tag, bits) in [(Layout::F32, 3u16 /* WAVE_FORMAT_IEEE_FLOAT */, 32u16), (Layout::I16, 1 /* PCM */, 16)]
    {
        let block = 2 * bits / 8;
        let fmt = WAVEFORMATEX {
            wFormatTag: tag,
            nChannels: 2,
            nSamplesPerSec: SAMPLE_RATE,
            nAvgBytesPerSec: SAMPLE_RATE * block as u32,
            nBlockAlign: block,
            wBitsPerSample: bits,
            cbSize: 0,
        };
        // 20 ms buffer (in 100 ns units).
        match unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 200_000, 0, &fmt, None) } {
            Ok(()) => return Ok(layout),
            Err(e) => last = e.message(),
        }
    }
    Err(format!("audio capture failed to start: {last}"))
}

struct Event(HANDLE);

impl Drop for Event {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

struct Com(bool);

impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

pub(crate) fn run(source: AudioSource, mut out: Chunker, ready: Ready, stop: &Stop) -> Result<(), String> {
    if !supported() {
        ready(Err("sharing audio needs Windows 10 version 2004 or later".into()));
        return Ok(());
    }
    let _com = Com(unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok());
    let (pid, mode) = match &source {
        AudioSource::System => (std::process::id(), PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE),
        AudioSource::App { screen_id } => match crate::capture::window_pid(screen_id) {
            Some(pid) => (pid, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE),
            None => {
                ready(Err("pick a window to share its audio (that window may have closed)".into()));
                return Ok(());
            }
        },
    };
    let setup = unsafe {
        (|| -> Result<_, String> {
            let client = activate(pid, mode)?;
            let layout = initialize(&client)?;
            let event = Event(CreateEventW(None, false, false, None).map_err(|e| e.message())?);
            client.SetEventHandle(event.0).map_err(|e| e.message())?;
            let capture: IAudioCaptureClient = client.GetService().map_err(|e| e.message())?;
            client.Start().map_err(|e| format!("audio capture failed to start: {}", e.message()))?;
            Ok((client, layout, event, capture))
        })()
    };
    let (client, layout, event, capture) = match setup {
        Ok(v) => v,
        Err(e) => {
            ready(Err(e));
            return Ok(());
        }
    };
    ready(Ok(()));
    let mut result = Ok(());
    'run: while !stop.stopped() {
        unsafe { WaitForSingleObject(event.0, 100) };
        loop {
            let n = match unsafe { capture.GetNextPacketSize() } {
                Ok(n) => n,
                Err(e) => {
                    result = Err(format!("audio capture stopped: {}", e.message()));
                    break 'run;
                }
            };
            if n == 0 {
                break;
            }
            let mut data: *mut u8 = std::ptr::null_mut();
            let (mut frames, mut flags) = (0u32, 0u32);
            if let Err(e) = unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None) } {
                result = Err(format!("audio capture stopped: {}", e.message()));
                break 'run;
            }
            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
            if silent || data.is_null() {
                out.push_silence(frames as usize);
            } else {
                let samples = frames as usize * 2;
                // SAFETY: WASAPI hands `frames` frames of the format we initialised.
                match layout {
                    Layout::F32 => out.push_stereo(unsafe { std::slice::from_raw_parts(data as *const f32, samples) }),
                    Layout::I16 => {
                        out.push_i16(unsafe { std::slice::from_raw_parts(data as *const i16, samples) }, 2, SAMPLE_RATE)
                    }
                }
            }
            let _ = unsafe { capture.ReleaseBuffer(frames) };
        }
        // Loopback delivers nothing while nothing plays; keep the stream's clock going.
        out.keep_alive();
    }
    let _ = unsafe { client.Stop() };
    drop(event);
    result
}
