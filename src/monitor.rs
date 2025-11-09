use std::sync::mpsc::Receiver;

use image::RgbaImage;

use crate::{
    VideoRecorder, error::XCapResult, platform::impl_monitor::ImplMonitor, video_recorder::Frame,
};

#[derive(Debug, Clone)]
pub struct Monitor {
    pub(crate) impl_monitor: ImplMonitor,
}

impl Monitor {
    pub(crate) fn new(impl_monitor: ImplMonitor) -> Monitor {
        Monitor { impl_monitor }
    }
}

impl Monitor {
    pub fn all() -> XCapResult<Vec<Monitor>> {
        let monitors = ImplMonitor::all()?
            .iter()
            .map(|impl_monitor| Monitor::new(impl_monitor.clone()))
            .collect();

        Ok(monitors)
    }
    pub fn from_unique_key(unique_key: String) -> XCapResult<Monitor> {
        let impl_monitor = ImplMonitor::from_unique_key(unique_key)?;

        Ok(Monitor::new(impl_monitor))
    }

    pub fn from_point(x: i32, y: i32) -> XCapResult<Monitor> {
        let impl_monitor = ImplMonitor::from_point(x, y)?;

        Ok(Monitor::new(impl_monitor))
    }
}

impl Monitor {
    /// Unique identifier associated with the screen.
    pub fn id(&self) -> XCapResult<u32> {
        self.impl_monitor.id()
    }
    pub fn unique_key(&self) -> XCapResult<String> {
        // 1. 优先使用序列号（硬件属性，最可靠）
        if let Ok(serial) = self.serial_number() {
            if !serial.is_empty() {
                return Ok(serial);
            }
        }

        // 2. 备用：使用 UUID
        if let Ok(uuid) = self.uuid() {
            return Ok(uuid);
        }

        // 3. 最后：使用显示器 ID
        return self.id().map(|id| id.to_string());
    }
    /// Unique identifier associated with the screen.
    pub fn name(&self) -> XCapResult<String> {
        self.impl_monitor.name()
    }
    /// The screen x coordinate.
    pub fn x(&self) -> XCapResult<i32> {
        self.impl_monitor.x()
    }
    /// The screen x coordinate.
    pub fn y(&self) -> XCapResult<i32> {
        self.impl_monitor.y()
    }
    /// The screen pixel width.
    pub fn width(&self) -> XCapResult<u32> {
        self.impl_monitor.width()
    }
    /// The screen pixel height.
    pub fn height(&self) -> XCapResult<u32> {
        self.impl_monitor.height()
    }
    /// Can be 0, 90, 180, 270, represents screen rotation in clock-wise degrees.
    pub fn rotation(&self) -> XCapResult<f32> {
        self.impl_monitor.rotation()
    }
    /// Output device's pixel scale factor.
    pub fn scale_factor(&self) -> XCapResult<f32> {
        self.impl_monitor.scale_factor()
    }
    /// The screen refresh rate.
    pub fn frequency(&self) -> XCapResult<f32> {
        self.impl_monitor.frequency()
    }
    /// Whether the screen is the main screen
    pub fn is_primary(&self) -> XCapResult<bool> {
        self.impl_monitor.is_primary()
    }

    /// Whether the screen is builtin
    pub fn is_builtin(&self) -> XCapResult<bool> {
        self.impl_monitor.is_builtin()
    }

    /// Get the display UUID (persistent unique identifier)
    /// This UUID remains constant across system restarts and display reconnections.
    /// Currently only supported on macOS.
    #[cfg(target_os = "macos")]
    pub fn uuid(&self) -> XCapResult<String> {
        self.impl_monitor.uuid()
    }

    /// Get the display serial number
    /// Some displays may not provide serial number information.
    /// Currently only supported on macOS.
    #[cfg(target_os = "macos")]
    pub fn serial_number(&self) -> XCapResult<String> {
        self.impl_monitor.serial_number()
    }

    /// Get the display UUID (persistent unique identifier)
    /// This UUID remains constant across system restarts and display reconnections.
    /// Supported on macOS, Windows, and Linux.
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    pub fn uuid(&self) -> XCapResult<String> {
        self.impl_monitor.uuid()
    }

    /// Get the display serial number
    /// Some displays may not provide serial number information.
    /// Supported on macOS, Windows, and Linux.
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    pub fn serial_number(&self) -> XCapResult<String> {
        self.impl_monitor.serial_number()
    }

    /// Get the display UUID (not supported on this platform)
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    pub fn uuid(&self) -> XCapResult<String> {
        Err(crate::XCapError::new(
            "UUID is not supported on this platform",
        ))
    }

    /// Get the display serial number (not supported on this platform)
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    pub fn serial_number(&self) -> XCapResult<String> {
        Err(crate::XCapError::new(
            "Serial number is not supported on this platform",
        ))
    }
}

impl Monitor {
    /// Capture image of the monitor
    pub fn capture_image(&self) -> XCapResult<RgbaImage> {
        self.impl_monitor.capture_image()
    }

    pub fn capture_region(&self, x: u32, y: u32, width: u32, height: u32) -> XCapResult<RgbaImage> {
        self.impl_monitor.capture_region(x, y, width, height)
    }

    pub fn video_recorder(&self) -> XCapResult<(VideoRecorder, Receiver<Frame>)> {
        let (impl_video_recorder, sx) = self.impl_monitor.video_recorder()?;

        Ok((VideoRecorder::new(impl_video_recorder), sx))
    }
}

#[cfg(test)]
mod tests {
    use crate::XCapError;

    use super::*;

    #[test]
    fn test_capture_region_out_of_bounds() {
        let monitors = Monitor::all().unwrap();
        let monitor = &monitors[0]; // Get first monitor

        // Try to capture a region that extends beyond monitor bounds
        let x = monitor.width().unwrap() / 2;
        let y = monitor.height().unwrap() / 2;
        let width = monitor.width().unwrap();
        let height = monitor.height().unwrap();

        let result = monitor.capture_region(x, y, width, height);

        match result {
            Err(XCapError::InvalidCaptureRegion(_)) => (),
            _ => panic!("Expected InvalidCaptureRegion error"),
        }
    }
}
