use std::sync::mpsc::Receiver;

use crate::{
    XCapResult,
    video_recorder::{Frame, VideoRecorderConfig},
};

use super::{
    impl_monitor::ImplMonitor, utils::wayland_detect, wayland_video_recorder::WaylandVideoRecorder,
    xorg_video_recorder::XorgVideoRecorder,
};

#[cfg(feature = "x11-xshm")]
use super::xshm_video_recorder::XShmVideoRecorder;

#[derive(Debug, Clone)]
pub enum ImplVideoRecorder {
    Xorg(XorgVideoRecorder),
    Wayland(WaylandVideoRecorder),
    #[cfg(feature = "x11-xshm")]
    XShm(XShmVideoRecorder),
}

impl ImplVideoRecorder {
    pub fn new(
        monitor: ImplMonitor,
        config: VideoRecorderConfig,
    ) -> XCapResult<(Self, Receiver<Frame>)> {
        if wayland_detect() {
            let (recorder, receiver) = WaylandVideoRecorder::new(monitor, config)?;
            Ok((ImplVideoRecorder::Wayland(recorder), receiver))
        } else {
            #[cfg(feature = "x11-xshm")]
            if config.prefer_x11_xshm {
                match XShmVideoRecorder::new(monitor.clone(), config) {
                    Ok((recorder, receiver)) => {
                        log::info!("Linux X11 capture backend: MIT-SHM");
                        return Ok((ImplVideoRecorder::XShm(recorder), receiver));
                    }
                    Err(error) => {
                        log::warn!(
                            "MIT-SHM initialization failed, falling back to XGetImage: {error}"
                        );
                    }
                }
            }

            #[cfg(not(feature = "x11-xshm"))]
            if config.prefer_x11_xshm {
                log::warn!(
                    "MIT-SHM runtime gate requested without x11-xshm build feature; using XGetImage"
                );
            }

            let (recorder, receiver) = XorgVideoRecorder::new(monitor, config)?;
            log::info!("Linux X11 capture backend: XGetImage");
            Ok((ImplVideoRecorder::Xorg(recorder), receiver))
        }
    }

    pub fn start(&self) -> XCapResult<()> {
        match self {
            ImplVideoRecorder::Xorg(recorder) => recorder.start(),
            ImplVideoRecorder::Wayland(recorder) => recorder.start(),
            #[cfg(feature = "x11-xshm")]
            ImplVideoRecorder::XShm(recorder) => recorder.start(),
        }
    }

    pub fn stop(&self) -> XCapResult<()> {
        match self {
            ImplVideoRecorder::Xorg(recorder) => recorder.stop(),
            ImplVideoRecorder::Wayland(recorder) => recorder.stop(),
            #[cfg(feature = "x11-xshm")]
            ImplVideoRecorder::XShm(recorder) => recorder.stop(),
        }
    }

    pub(crate) fn dropped_frames(&self) -> usize {
        match self {
            ImplVideoRecorder::Xorg(recorder) => recorder.dropped_frames(),
            ImplVideoRecorder::Wayland(recorder) => recorder.dropped_frames(),
            #[cfg(feature = "x11-xshm")]
            ImplVideoRecorder::XShm(recorder) => recorder.dropped_frames(),
        }
    }
}
