//! macOS 13+: ScreenCaptureKit audio. A stream on the main display with `capturesAudio` and
//! `excludesCurrentProcessAudio` records what the Mac plays minus our own call ("System"); a
//! filter that only includes the application owning a shared window records just that app.
//! Needs the Screen Recording permission, like screen capture. The stream's video is kept tiny
//! and slow since only its audio is used.

use std::ptr::NonNull;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send, sel};
use objc2_core_audio_types::{AudioBuffer, AudioBufferList, kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{
    CMAudioFormatDescriptionGetStreamBasicDescription, CMBlockBuffer, CMSampleBuffer, CMTime,
    kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
};
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCRunningApplication, SCStream, SCStreamConfiguration, SCStreamDelegate,
    SCStreamOutput, SCStreamOutputType, SCWindow,
};

use super::{AudioSource, AudioSupport, Chunker, SAMPLE_RATE};
use crate::capture::{Ready, Stop, shareable_content};

/// `SCStreamConfiguration.capturesAudio` exists (macOS 13+). The app's deployment target is
/// 12.3, so this is checked at run time.
fn supported() -> bool {
    let cfg = unsafe { SCStreamConfiguration::new() };
    cfg.respondsToSelector(sel!(setCapturesAudio:)) && cfg.respondsToSelector(sel!(setExcludesCurrentProcessAudio:))
}

pub(crate) fn support() -> AudioSupport {
    let ok = supported();
    AudioSupport { system: ok, per_app: ok, includes_own_audio: false }
}

struct AudioIvars {
    out: Mutex<Chunker>,
    stopped: Arc<Mutex<Option<String>>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSObject))]
    #[ivars = AudioIvars]
    struct AudioDelegate;

    unsafe impl NSObjectProtocol for AudioDelegate {}

    unsafe impl SCStreamOutput for AudioDelegate {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            if kind == SCStreamOutputType::Audio {
                let mut out = self.ivars().out.lock().unwrap_or_else(|e| e.into_inner());
                unsafe { deliver(sample, &mut out) };
            }
        }
    }

    unsafe impl SCStreamDelegate for AudioDelegate {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _stream: &SCStream, error: &NSError) {
            let msg = format!("audio capture stopped: {}", error.localizedDescription());
            *self.ivars().stopped.lock().unwrap_or_else(|e| e.into_inner()) = Some(msg);
        }
    }
);

impl AudioDelegate {
    fn new(ivars: AudioIvars) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ivars);
        unsafe { msg_send![super(this), init] }
    }
}

/// Hands one audio sample buffer (32-bit float, planar or interleaved) to the chunker.
unsafe fn deliver(sample: &CMSampleBuffer, out: &mut Chunker) {
    unsafe {
        let Some(desc) = sample.format_description() else { return };
        let Some(asbd) = CMAudioFormatDescriptionGetStreamBasicDescription(&desc).as_ref().copied() else { return };
        if asbd.mFormatFlags & kAudioFormatFlagIsFloat == 0 || asbd.mBitsPerChannel != 32 {
            return;
        }
        let rate = asbd.mSampleRate.round() as u32;
        let mut needed = 0usize;
        sample.audio_buffer_list_with_retained_block_buffer(
            &mut needed,
            std::ptr::null_mut(),
            0,
            None,
            None,
            0,
            std::ptr::null_mut(),
        );
        // AudioBufferList is variable length; u64 storage keeps it aligned.
        let mut storage = vec![0u64; needed.max(std::mem::size_of::<AudioBufferList>() * 2).div_ceil(8)];
        let list = storage.as_mut_ptr() as *mut AudioBufferList;
        let mut block: *mut CMBlockBuffer = std::ptr::null_mut();
        let r = sample.audio_buffer_list_with_retained_block_buffer(
            std::ptr::null_mut(),
            list,
            storage.len() * 8,
            None,
            None,
            kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
            &mut block,
        );
        // Keeps the samples alive while we read them, released at the end of this scope.
        let _block = NonNull::new(block).map(|b| CFRetained::from_raw(b));
        if r != 0 {
            return;
        }
        let n = (*list).mNumberBuffers as usize;
        let buffers: &[AudioBuffer] = std::slice::from_raw_parts((&raw const (*list).mBuffers).cast(), n);
        let floats = |b: &AudioBuffer| -> &[f32] {
            if b.mData.is_null() {
                &[]
            } else {
                std::slice::from_raw_parts(b.mData as *const f32, b.mDataByteSize as usize / 4)
            }
        };
        if asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0 {
            let planes: Vec<&[f32]> = buffers.iter().map(floats).collect();
            out.push_planar(&planes, rate);
        } else if let Some(b) = buffers.first() {
            out.push_interleaved(floats(b), b.mNumberChannels.max(1) as usize, rate);
        }
    }
}

fn window_id(screen_id: &str) -> Option<u32> {
    screen_id.strip_prefix("window:")?.parse().ok()
}

pub(crate) fn run(source: AudioSource, out: Chunker, ready: Ready, stop: &Stop) -> Result<(), String> {
    if !supported() {
        ready(Err("sharing audio needs macOS 13 or later".into()));
        return Ok(());
    }
    let content = match shareable_content() {
        Ok(c) => c,
        Err(e) => {
            ready(Err(e));
            return Ok(());
        }
    };
    let displays: Vec<Retained<SCDisplay>> = unsafe { content.displays() }.to_vec();
    let main = objc2_core_graphics::CGMainDisplayID();
    let Some(display) = displays.iter().find(|d| unsafe { d.displayID() } == main).or(displays.first()).cloned() else {
        ready(Err("no display to attach audio capture to".into()));
        return Ok(());
    };
    let no_windows = NSArray::<SCWindow>::new();
    let filter = match &source {
        // Audio is captured system wide whatever the display filter shows.
        AudioSource::System => unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &display, &no_windows)
        },
        AudioSource::App { screen_id } => {
            let windows: Vec<Retained<SCWindow>> = unsafe { content.windows() }.to_vec();
            let app: Option<Retained<SCRunningApplication>> = window_id(screen_id)
                .and_then(|id| windows.into_iter().find(|w| unsafe { w.windowID() } == id))
                .and_then(|w| unsafe { w.owningApplication() });
            let Some(app) = app else {
                ready(Err("pick a window to share its audio (that window may have closed)".into()));
                return Ok(());
            };
            let apps = NSArray::from_retained_slice(&[app]);
            unsafe {
                SCContentFilter::initWithDisplay_includingApplications_exceptingWindows(
                    SCContentFilter::alloc(),
                    &display,
                    &apps,
                    &no_windows,
                )
            }
        }
    };
    let stopped = Arc::new(Mutex::new(None));
    let delegate = AudioDelegate::new(AudioIvars { out: Mutex::new(out), stopped: stopped.clone() });
    let queue = dispatch2::DispatchQueue::new("celerity.audio", None);
    let stream = unsafe {
        let cfg = SCStreamConfiguration::new();
        cfg.setCapturesAudio(true);
        cfg.setExcludesCurrentProcessAudio(true);
        cfg.setSampleRate(SAMPLE_RATE as isize);
        cfg.setChannelCount(2);
        // The (unused) video: as small and rare as allowed.
        cfg.setWidth(64);
        cfg.setHeight(64);
        cfg.setMinimumFrameInterval(CMTime::new(1, 2));
        cfg.setShowsCursor(false);
        let stream = SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &filter,
            &cfg,
            Some(ProtocolObject::from_ref(&*delegate)),
        );
        for kind in [SCStreamOutputType::Audio, SCStreamOutputType::Screen] {
            // Without a screen output ScreenCaptureKit logs every dropped video frame.
            if let Err(e) = stream.addStreamOutput_type_sampleHandlerQueue_error(
                ProtocolObject::from_ref(&*delegate),
                kind,
                Some(&queue),
            ) && kind == SCStreamOutputType::Audio
            {
                ready(Err(format!("audio capture failed: {}", e.localizedDescription())));
                return Ok(());
            }
        }
        stream
    };
    let (tx, rx) = mpsc::channel();
    let started = RcBlock::new(move |error: *mut NSError| {
        let r = match unsafe { error.as_ref() } {
            None => Ok(()),
            Some(e) => Err(e.localizedDescription().to_string()),
        };
        let _ = tx.send(r);
    });
    unsafe { stream.startCaptureWithCompletionHandler(Some(&started)) };
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(())) => ready(Ok(())),
        Ok(Err(e)) => {
            ready(Err(format!("audio capture failed to start: {e}")));
            return Ok(());
        }
        Err(_) => {
            ready(Err("audio capture did not start".into()));
            return Ok(());
        }
    }
    let mut result = Ok(());
    while !stop.sleep(Duration::from_millis(20)) {
        if let Some(e) = stopped.lock().unwrap_or_else(|e| e.into_inner()).take() {
            result = Err(e);
            break;
        }
        // Nothing is delivered while nothing plays; keep the consumer's clock going.
        delegate.ivars().out.lock().unwrap_or_else(|e| e.into_inner()).keep_alive();
    }
    let (tx, rx) = mpsc::channel::<()>();
    let done = RcBlock::new(move |_error: *mut NSError| {
        let _ = tx.send(());
    });
    unsafe { stream.stopCaptureWithCompletionHandler(Some(&done)) };
    let _ = rx.recv_timeout(Duration::from_secs(2));
    result
}
