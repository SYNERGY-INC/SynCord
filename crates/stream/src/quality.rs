//! Quality presets for screen sharing and cameras: resolution cap, frame rate and bitrate that
//! go together, so the UI only offers a choice like "1440p 60 fps".

#[cfg(feature = "capture")]
use crate::capture::{CameraConfig, ScreenConfig};

/// Screen share presets. Frames are never upscaled: a smaller screen or window is sent at its own
/// size, and with [`crate::SenderConfig::fit_bitrate`] (set by the presets) the bitrate shrinks
/// with it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ScreenQuality {
    /// 1280x720 at 30 fps, 2.5 Mbps.
    Res720p30,
    /// 1920x1080 at 30 fps, 5 Mbps.
    Res1080p30,
    /// 1920x1080 at 60 fps, 8 Mbps.
    Res1080p60,
    /// 2560x1440 at 60 fps, 12 Mbps. The default.
    #[default]
    Res1440p60,
    /// The screen's own resolution up to 3840x2160, at 30 fps (up to 13 Mbps at 4K).
    Source30,
    /// The screen's own resolution up to 3840x2160, at 60 fps (up to 22 Mbps at 4K).
    Source60,
}

impl ScreenQuality {
    pub const ALL: [ScreenQuality; 6] = [
        ScreenQuality::Res720p30,
        ScreenQuality::Res1080p30,
        ScreenQuality::Res1080p60,
        ScreenQuality::Res1440p60,
        ScreenQuality::Source30,
        ScreenQuality::Source60,
    ];

    /// Largest frame size sent.
    pub fn max_size(self) -> (u32, u32) {
        match self {
            ScreenQuality::Res720p30 => (1280, 720),
            ScreenQuality::Res1080p30 | ScreenQuality::Res1080p60 => (1920, 1080),
            ScreenQuality::Res1440p60 => (2560, 1440),
            ScreenQuality::Source30 | ScreenQuality::Source60 => (3840, 2160),
        }
    }

    pub fn fps(self) -> u32 {
        match self {
            ScreenQuality::Res720p30 | ScreenQuality::Res1080p30 | ScreenQuality::Source30 => 30,
            _ => 60,
        }
    }

    /// Target bitrate at [`ScreenQuality::max_size`].
    pub fn bitrate_bps(self) -> u32 {
        match self {
            ScreenQuality::Res720p30 => 2_500_000,
            ScreenQuality::Res1080p30 => 5_000_000,
            ScreenQuality::Res1080p60 => 8_000_000,
            ScreenQuality::Res1440p60 => 12_000_000,
            ScreenQuality::Source30 => 13_000_000,
            ScreenQuality::Source60 => 22_000_000,
        }
    }

    /// For a settings menu, e.g. "1440p 60 fps".
    pub fn label(self) -> &'static str {
        match self {
            ScreenQuality::Res720p30 => "720p 30 fps",
            ScreenQuality::Res1080p30 => "1080p 30 fps",
            ScreenQuality::Res1080p60 => "1080p 60 fps",
            ScreenQuality::Res1440p60 => "1440p 60 fps",
            ScreenQuality::Source30 => "Source (up to 4K) 30 fps",
            ScreenQuality::Source60 => "Source (up to 4K) 60 fps",
        }
    }

    /// The matching capture request.
    #[cfg(feature = "capture")]
    pub fn screen_config(self, show_cursor: bool) -> ScreenConfig {
        let (max_width, max_height) = self.max_size();
        ScreenConfig { max_width, max_height, fps: self.fps(), show_cursor }
    }
}

/// Camera presets (30 fps; cameras rarely do better in good quality, and 60 fps webcams usually
/// drop to MJPEG at reduced exposure).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CameraQuality {
    /// 854x480 at 30 fps, 1 Mbps.
    Res480p,
    /// 1280x720 at 30 fps, 1.8 Mbps. The default.
    #[default]
    Res720p,
    /// 1920x1080 at 30 fps, 3.5 Mbps.
    Res1080p,
}

impl CameraQuality {
    pub const ALL: [CameraQuality; 3] = [CameraQuality::Res480p, CameraQuality::Res720p, CameraQuality::Res1080p];

    pub fn size(self) -> (u32, u32) {
        match self {
            CameraQuality::Res480p => (854, 480),
            CameraQuality::Res720p => (1280, 720),
            CameraQuality::Res1080p => (1920, 1080),
        }
    }

    pub fn fps(self) -> u32 {
        30
    }

    pub fn bitrate_bps(self) -> u32 {
        match self {
            CameraQuality::Res480p => 1_000_000,
            CameraQuality::Res720p => 1_800_000,
            CameraQuality::Res1080p => 3_500_000,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            CameraQuality::Res480p => "480p",
            CameraQuality::Res720p => "720p",
            CameraQuality::Res1080p => "1080p",
        }
    }

    /// The matching capture request (the best camera mode at or above this size is opened).
    #[cfg(feature = "capture")]
    pub fn camera_config(self) -> CameraConfig {
        let (width, height) = self.size();
        CameraConfig { width, height, fps: self.fps() }
    }
}
