use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, OnceLock, mpsc::Receiver},
};

use image::RgbaImage;

use crate::{
    VideoRecorder, VideoRecorderConfig, VideoRecorderOutputSize, XCapError, error::XCapResult,
    platform::impl_monitor::ImplMonitor, video_recorder::Frame,
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

    /// Capture image of the monitor with a custom scale factor.
    /// scale=1.0 captures at logical resolution (default behavior).
    /// scale=2.0 captures at 2x resolution (physical pixels on Retina).
    /// On Windows/Linux this parameter is ignored as they already capture at physical resolution.
    pub fn capture_image_with_scale(&self, scale: f32) -> XCapResult<RgbaImage> {
        self.impl_monitor.capture_image_with_scale(scale)
    }

    /// Captures a monitor image using the same orientation-independent output
    /// sizing policy as persistent video recording.
    ///
    /// macOS requests the target size from ScreenCaptureKit. Other platforms
    /// currently resize the captured image when their one-shot APIs cannot
    /// negotiate an output size at the capture boundary.
    pub fn capture_image_with_output_size(
        &self,
        output_size: VideoRecorderOutputSize,
    ) -> XCapResult<RgbaImage> {
        #[cfg(target_os = "macos")]
        {
            if let Ok((native_width, native_height)) = self.impl_monitor.native_pixel_dimensions() {
                let target_dimensions = output_size.output_dimensions(native_width, native_height);
                let image = self
                    .impl_monitor
                    .capture_image_with_dimensions(target_dimensions.0, target_dimensions.1)?;
                return resize_rgba_image_to_dimensions(image, target_dimensions);
            }

            // A disappearing/reconfiguring display may temporarily have no
            // valid CGDisplayMode. Preserve one-shot capture by using the
            // actual fallback image dimensions instead of guessing native
            // pixels. Persistent capture deliberately returns the mode error
            // so its caller can rebuild or choose another backend.
            let image = self.impl_monitor.capture_image()?;
            let target_dimensions = output_size.output_dimensions(image.width(), image.height());
            return resize_rgba_image_to_dimensions(image, target_dimensions);
        }

        #[cfg(not(target_os = "macos"))]
        {
            let image = self.impl_monitor.capture_image()?;
            let target_dimensions = output_size.output_dimensions(image.width(), image.height());
            resize_rgba_image_to_dimensions(image, target_dimensions)
        }
    }

    pub fn capture_region(&self, x: u32, y: u32, width: u32, height: u32) -> XCapResult<RgbaImage> {
        self.impl_monitor.capture_region(x, y, width, height)
    }

    pub fn video_recorder(&self) -> XCapResult<(VideoRecorder, Receiver<Frame>)> {
        self.video_recorder_with_config(VideoRecorderConfig::default())
    }

    /// Creates a persistent platform recorder with bounded latest-frame
    /// delivery and producer-side frame-rate throttling.
    pub fn video_recorder_with_fps(
        &self,
        fps: f64,
    ) -> XCapResult<(VideoRecorder, Receiver<Frame>)> {
        self.video_recorder_with_config(VideoRecorderConfig {
            fps,
            ..VideoRecorderConfig::default()
        })
    }

    pub fn video_recorder_with_config(
        &self,
        config: VideoRecorderConfig,
    ) -> XCapResult<(VideoRecorder, Receiver<Frame>)> {
        let (impl_video_recorder, sx) = self.impl_monitor.video_recorder_with_config(config)?;

        Ok((VideoRecorder::new(impl_video_recorder), sx))
    }
}

const RESIZE_COORDINATE_CACHE_CAPACITY: usize = 8;
static RESIZE_COORDINATE_CACHE: OnceLock<Mutex<VecDeque<CachedResizeCoordinates>>> =
    OnceLock::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResizeCoordinate {
    lower: usize,
    upper: usize,
    weight: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResizeCoordinateKey {
    source_width: u32,
    source_height: u32,
    output_width: u32,
    output_height: u32,
}

struct CachedResizeCoordinates {
    key: ResizeCoordinateKey,
    x: Arc<[ResizeCoordinate]>,
    y: Arc<[ResizeCoordinate]>,
}

/// Resizes one RGBA frame with the same allocation-bounded fallback scaler
/// used by monitor capture. Coordinate maps are reused for stable geometry.
pub fn resize_rgba_image_to_dimensions(
    image: RgbaImage,
    dimensions: (u32, u32),
) -> XCapResult<RgbaImage> {
    if image.dimensions() == dimensions {
        return Ok(image);
    }

    let (source_width, source_height) = image.dimensions();
    if source_width == 0 || source_height == 0 {
        return Err(XCapError::new("cannot resize an empty monitor image"));
    }
    let (output_width, output_height) = dimensions;
    let output_len = usize::try_from(output_width)
        .ok()
        .and_then(|width| {
            usize::try_from(output_height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| XCapError::new("monitor output image size overflow"))?;
    let source = image.as_raw();
    let mut output = vec![0; output_len];
    resize_packed_pixels_into(
        source,
        source_width,
        source_height,
        source_width as usize * 4,
        &mut output,
        output_width,
        output_height,
    )?;

    RgbaImage::from_raw(output_width, output_height, output)
        .ok_or_else(|| XCapError::new("failed to construct resized monitor image"))
}

pub(crate) fn resize_packed_pixels_into(
    source: &[u8],
    source_width: u32,
    source_height: u32,
    source_stride: usize,
    output: &mut [u8],
    output_width: u32,
    output_height: u32,
) -> XCapResult<()> {
    let source_row_bytes = source_width as usize * 4;
    let required_source_len = if source_height == 0 {
        0
    } else {
        source_stride
            .checked_mul(source_height as usize - 1)
            .and_then(|prefix| prefix.checked_add(source_row_bytes))
            .ok_or_else(|| XCapError::new("source monitor frame size overflow"))?
    };
    let required_output_len = output_width
        .checked_mul(output_height)
        .and_then(|pixels| pixels.checked_mul(4))
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or_else(|| XCapError::new("output monitor frame size overflow"))?;
    if source_width == 0 || source_height == 0 || output_width == 0 || output_height == 0 {
        return Err(XCapError::new("cannot resize an empty monitor frame"));
    }
    if source_stride < source_row_bytes || source.len() < required_source_len {
        return Err(XCapError::new(
            "source monitor frame stride or buffer length is invalid",
        ));
    }
    if output.len() < required_output_len {
        return Err(XCapError::new("output monitor frame buffer is too small"));
    }

    let (x_coordinates, y_coordinates) =
        cached_resize_coordinates(source_width, source_height, output_width, output_height);

    for (output_y, y_coordinate) in y_coordinates.iter().enumerate() {
        for (output_x, x_coordinate) in x_coordinates.iter().enumerate() {
            let top_left = y_coordinate.lower * source_stride + x_coordinate.lower * 4;
            let top_right = y_coordinate.lower * source_stride + x_coordinate.upper * 4;
            let bottom_left = y_coordinate.upper * source_stride + x_coordinate.lower * 4;
            let bottom_right = y_coordinate.upper * source_stride + x_coordinate.upper * 4;
            let destination = (output_y * output_width as usize + output_x) * 4;

            for channel in 0..4 {
                output[destination + channel] = bilinear_channel(
                    source[top_left + channel],
                    source[top_right + channel],
                    source[bottom_left + channel],
                    source[bottom_right + channel],
                    x_coordinate.weight,
                    y_coordinate.weight,
                );
            }
        }
    }
    Ok(())
}

fn cached_resize_coordinates(
    source_width: u32,
    source_height: u32,
    output_width: u32,
    output_height: u32,
) -> (Arc<[ResizeCoordinate]>, Arc<[ResizeCoordinate]>) {
    let key = ResizeCoordinateKey {
        source_width,
        source_height,
        output_width,
        output_height,
    };
    let cache = RESIZE_COORDINATE_CACHE.get_or_init(|| Mutex::new(VecDeque::new()));
    let mut cache = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(index) = cache.iter().position(|entry| entry.key == key) {
        let entry = cache
            .remove(index)
            .expect("coordinate cache index came from the same queue");
        let result = (entry.x.clone(), entry.y.clone());
        cache.push_back(entry);
        return result;
    }

    let x = axis_resize_coordinates(source_width, output_width);
    let y = axis_resize_coordinates(source_height, output_height);
    if cache.len() >= RESIZE_COORDINATE_CACHE_CAPACITY {
        cache.pop_front();
    }
    cache.push_back(CachedResizeCoordinates {
        key,
        x: x.clone(),
        y: y.clone(),
    });
    (x, y)
}

fn axis_resize_coordinates(source_len: u32, output_len: u32) -> Arc<[ResizeCoordinate]> {
    (0..output_len)
        .map(|index| bilinear_coordinate(index, source_len, output_len))
        .collect::<Vec<_>>()
        .into()
}

fn bilinear_coordinate(index: u32, source_len: u32, output_len: u32) -> ResizeCoordinate {
    const FIXED_ONE: u64 = 1 << 16;

    if source_len <= 1 || output_len <= 1 {
        return ResizeCoordinate {
            lower: 0,
            upper: 0,
            weight: 0,
        };
    }
    let fixed =
        u64::from(index) * u64::from(source_len - 1) * FIXED_ONE / u64::from(output_len - 1);
    let lower = (fixed / FIXED_ONE) as usize;
    let upper = (lower + 1).min(source_len as usize - 1);
    ResizeCoordinate {
        lower,
        upper,
        weight: fixed % FIXED_ONE,
    }
}

fn bilinear_channel(
    top_left: u8,
    top_right: u8,
    bottom_left: u8,
    bottom_right: u8,
    weight_x: u64,
    weight_y: u64,
) -> u8 {
    const FIXED_ONE: u64 = 1 << 16;
    const FIXED_HALF: u64 = FIXED_ONE / 2;

    let top = (u64::from(top_left) * (FIXED_ONE - weight_x)
        + u64::from(top_right) * weight_x
        + FIXED_HALF)
        / FIXED_ONE;
    let bottom = (u64::from(bottom_left) * (FIXED_ONE - weight_x)
        + u64::from(bottom_right) * weight_x
        + FIXED_HALF)
        / FIXED_ONE;
    ((top * (FIXED_ONE - weight_y) + bottom * weight_y + FIXED_HALF) / FIXED_ONE) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedicated_rgba_scaler_preserves_corners_without_intermediate_images() {
        let image = RgbaImage::from_raw(
            3,
            3,
            vec![
                1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
                24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36,
            ],
        )
        .unwrap();

        let resized = resize_rgba_image_to_dimensions(image, (2, 2)).unwrap();

        assert_eq!(
            resized.into_raw(),
            vec![1, 2, 3, 4, 9, 10, 11, 12, 25, 26, 27, 28, 33, 34, 35, 36]
        );
    }

    #[test]
    fn dedicated_rgba_scaler_reuses_coordinate_maps_for_stable_geometry() {
        let first = cached_resize_coordinates(3456, 2234, 1670, 1080);
        let second = cached_resize_coordinates(3456, 2234, 1670, 1080);

        assert!(Arc::ptr_eq(&first.0, &second.0));
        assert!(Arc::ptr_eq(&first.1, &second.1));
    }

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
