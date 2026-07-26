mod error;
mod monitor;
#[cfg(target_os = "linux")]
mod monitor_topology;
mod video_recorder;
mod window;

#[cfg(target_os = "macos")]
#[path = "macos/mod.rs"]
pub mod platform;

#[cfg(target_os = "windows")]
#[path = "windows/mod.rs"]
mod platform;

#[cfg(target_os = "linux")]
#[path = "linux/mod.rs"]
mod platform;

#[cfg(target_os = "android")]
#[path = "android/mod.rs"]
mod platform;

pub use image;

pub use error::{XCapError, XCapResult};
pub use monitor::{Monitor, resize_rgba_image_to_dimensions};
#[cfg(target_os = "linux")]
pub use monitor_topology::MonitorTopologyWatcher;
pub use window::Window;

pub use video_recorder::CaptureBackendKind;
pub use video_recorder::Frame;
pub use video_recorder::FrameBuffer;
pub use video_recorder::FramePixelFormat;
pub use video_recorder::VideoRecorder;
pub use video_recorder::VideoRecorderConfig;
pub use video_recorder::VideoRecorderOutputSize;

#[cfg(target_os = "macos")]
pub use platform::native_frame::NativeFrameSurface;
#[cfg(target_os = "macos")]
pub use platform::native_video_writer::{
    NativeVideoCodec, NativeVideoWriter, NativeVideoWriterFinish,
};
