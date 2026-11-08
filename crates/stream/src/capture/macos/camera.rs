//! AVFoundation camera capture. Needs `NSCameraUsageDescription` in the app's Info.plist; the
//! first open shows the system permission prompt.

use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_av_foundation::{
    AVAuthorizationStatus, AVCaptureConnection, AVCaptureDevice, AVCaptureDeviceFormat, AVCaptureDeviceInput,
    AVCaptureOutput, AVCaptureSession, AVCaptureVideoDataOutput, AVCaptureVideoDataOutputSampleBufferDelegate,
    AVMediaTypeVideo,
};
use objc2_core_media::{CMSampleBuffer, CMTime, CMVideoFormatDescriptionGetDimensions};
use objc2_core_video::{
    kCVPixelBufferHeightKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_foundation::{NSDictionary, NSNumber, NSString};

use super::FrameHandler;
use crate::capture::{CameraConfig, DeviceInfo, Mode, Ready, Sink, Stop, choose_mode};
use crate::convert::fit_size;

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSObject))]
    #[ivars = FrameHandler]
    struct CameraDelegate;

    unsafe impl NSObjectProtocol for CameraDelegate {}

    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for CameraDelegate {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        fn did_output(&self, _output: &AVCaptureOutput, sample: &CMSampleBuffer, _connection: &AVCaptureConnection) {
            self.ivars().handle(sample);
        }
    }
);

impl CameraDelegate {
    fn new(handler: FrameHandler) -> Retained<Self> {
        let this = Self::alloc().set_ivars(handler);
        unsafe { msg_send![super(this), init] }
    }
}

fn video() -> &'static objc2_av_foundation::AVMediaType {
    unsafe { AVMediaTypeVideo }.expect("AVMediaTypeVideo")
}

#[allow(deprecated)] // AVCaptureDeviceDiscoverySession's external type needs macOS 14
fn devices() -> Vec<Retained<AVCaptureDevice>> {
    unsafe { AVCaptureDevice::devicesWithMediaType(video()) }.to_vec()
}

pub(crate) fn list_cameras() -> Result<Vec<DeviceInfo>, String> {
    // Listing does not need (or prompt for) camera permission.
    let mut out: Vec<DeviceInfo> = devices()
        .iter()
        .map(|d| unsafe { DeviceInfo { id: d.uniqueID().to_string(), name: d.localizedName().to_string() } })
        .collect();
    let names: Vec<String> = out.iter().map(|d| d.name.clone()).collect();
    for (i, d) in out.iter_mut().enumerate() {
        if names.iter().filter(|n| **n == d.name).count() > 1 {
            d.name = format!("{} ({})", d.name, i + 1);
        }
    }
    Ok(out)
}

/// Asks for camera permission if needed (blocks until the user answers).
fn ensure_permission() -> Result<(), String> {
    let denied = "camera access is off for SynCord: allow it in System Settings > Privacy & Security > Camera";
    match unsafe { AVCaptureDevice::authorizationStatusForMediaType(video()) } {
        AVAuthorizationStatus::Authorized => Ok(()),
        AVAuthorizationStatus::NotDetermined => {
            let (tx, rx) = mpsc::channel();
            let block = RcBlock::new(move |granted: Bool| {
                let _ = tx.send(granted.as_bool());
            });
            unsafe { AVCaptureDevice::requestAccessForMediaType_completionHandler(video(), &block) };
            match rx.recv_timeout(Duration::from_secs(120)) {
                Ok(true) => Ok(()),
                _ => Err(denied.into()),
            }
        }
        _ => Err(denied.into()),
    }
}

fn subtype_rank(fourcc: u32) -> u32 {
    match &fourcc.to_be_bytes() {
        b"420v" | b"420f" => 0,
        b"yuvs" | b"2vuy" => 1,
        _ => 2,
    }
}

pub(crate) fn run_camera(id: &str, config: CameraConfig, sink: Sink, ready: Ready, stop: &Stop) -> Result<(), String> {
    if let Err(e) = ensure_permission() {
        ready(Err(e));
        return Ok(());
    }
    let devs = devices();
    let found = devs.iter().find(|d| unsafe { d.uniqueID() }.to_string() == id).or(if id.is_empty() {
        devs.first()
    } else {
        None
    });
    let Some(device) = found.cloned() else {
        ready(Err("camera not found (unplugged?)".into()));
        return Ok(());
    };

    // Pick the device format ourselves (size and frame rate).
    let formats: Vec<Retained<AVCaptureDeviceFormat>> = unsafe { device.formats() }.to_vec();
    let modes: Vec<Mode> = formats
        .iter()
        .enumerate()
        .map(|(i, f)| unsafe {
            let desc = f.formatDescription();
            let dims = CMVideoFormatDescriptionGetDimensions(&desc);
            let fps = f.videoSupportedFrameRateRanges().iter().map(|r| r.maxFrameRate()).fold(0.0, f64::max);
            Mode {
                format: i as u32,
                rank: subtype_rank(desc.media_sub_type()),
                width: dims.width.max(0) as u32,
                height: dims.height.max(0) as u32,
                fps: fps as f32,
            }
        })
        .collect();
    let Some(mode) = choose_mode(&modes, config) else {
        ready(Err("the camera offers no usable format".into()));
        return Ok(());
    };
    let format = &formats[mode.format as usize];

    let input = match unsafe { AVCaptureDeviceInput::deviceInputWithDevice_error(&device) } {
        Ok(i) => i,
        Err(e) => {
            ready(Err(format!("cannot open the camera: {}", e.localizedDescription())));
            return Ok(());
        }
    };
    let session = unsafe { AVCaptureSession::new() };
    let output = unsafe { AVCaptureVideoDataOutput::new() };
    let (tw, th) = fit_size(mode.width, mode.height, config.width, config.height);
    let delegate = CameraDelegate::new(FrameHandler::new(sink, config.fps, config.width, config.height));
    let queue = dispatch2::DispatchQueue::new("syncord.camera", None);
    unsafe {
        session.beginConfiguration();
        if !session.canAddInput(&input) {
            session.commitConfiguration();
            ready(Err("the camera is in use by another application".into()));
            return Ok(());
        }
        session.addInput(&input);
        // NV12 at our size: AVFoundation scales (same aspect ratio as the device format).
        let keys: [&NSString; 3] = [
            &*(kCVPixelBufferPixelFormatTypeKey as *const _ as *const NSString),
            &*(kCVPixelBufferWidthKey as *const _ as *const NSString),
            &*(kCVPixelBufferHeightKey as *const _ as *const NSString),
        ];
        let vals = [
            NSNumber::new_u32(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange),
            NSNumber::new_u32(tw),
            NSNumber::new_u32(th),
        ];
        let vals: Vec<&NSNumber> = vals.iter().map(|v| &**v).collect();
        let dict = NSDictionary::<NSString, NSNumber>::from_slices(&keys, &vals);
        let dict = &*(Retained::as_ptr(&dict) as *const NSDictionary<NSString, AnyObject>);
        output.setVideoSettings(Some(dict));
        output.setAlwaysDiscardsLateVideoFrames(true);
        output.setSampleBufferDelegate_queue(Some(ProtocolObject::from_ref(&*delegate)), Some(&queue));
        if !session.canAddOutput(&output) {
            session.commitConfiguration();
            ready(Err("cannot read frames from the camera".into()));
            return Ok(());
        }
        session.addOutput(&output);
        let locked = device.lockForConfiguration().is_ok();
        if locked {
            device.setActiveFormat(format);
            // Only durations inside a supported range are allowed (anything else throws), so
            // take the range's own value unless the request lies strictly inside it.
            let want = config.fps.max(1) as f64;
            let ranges = format.videoSupportedFrameRateRanges().to_vec();
            let duration = ranges
                .iter()
                .find(|r| r.minFrameRate() + 0.5 < want && want + 0.5 < r.maxFrameRate())
                .map(|_| CMTime::new(1, want as i32))
                .or_else(|| {
                    ranges
                        .iter()
                        .filter(|r| r.maxFrameRate() <= want + 0.5)
                        .max_by(|a, b| a.maxFrameRate().total_cmp(&b.maxFrameRate()))
                        .map(|r| r.minFrameDuration())
                });
            if let Some(d) = duration {
                device.setActiveVideoMinFrameDuration(d);
            }
        }
        session.commitConfiguration();
        session.startRunning();
        if locked {
            device.unlockForConfiguration();
        }
        if !session.isRunning() {
            ready(Err("the camera did not start".into()));
            return Ok(());
        }
    }
    ready(Ok(()));
    let mut result = Ok(());
    while !stop.sleep(Duration::from_millis(100)) {
        if !unsafe { device.isConnected() } {
            result = Err("camera disconnected".into());
            break;
        }
    }
    unsafe {
        session.stopRunning();
        output.setSampleBufferDelegate_queue(None, None);
    }
    result
}
