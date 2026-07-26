mod capture;
mod d3d11_readback;
mod display_info;
mod utils;

pub mod impl_monitor;
pub mod impl_video_recorder;
pub mod impl_window;
#[cfg(feature = "windows-wgc")]
mod wgc_video_recorder;
