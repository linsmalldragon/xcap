use std::{
    fmt, slice,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use objc2_core_foundation::CFRetained;
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelFormatType_32BGRA,
};
use scopeguard::defer;

use crate::{XCapError, XCapResult};

/// A retained ScreenCaptureKit Core Video surface.
///
/// Core Video pixel buffers are reference-counted and may be retained across
/// the capture callback. The application keeps at most its latest in-flight
/// surfaces and only locks the buffer while producing a thumbnail or a
/// compatibility BGRA copy.
#[derive(Clone)]
pub struct NativeFrameSurface {
    pixel_buffer: CFRetained<CVPixelBuffer>,
    _lease: Option<Arc<NativeFrameLease>>,
}

// Core Video documents CVPixelBuffer as a thread-safe CFType. Access to its
// base address is additionally serialized by CVPixelBufferLockBaseAddress.
unsafe impl Send for NativeFrameSurface {}
unsafe impl Sync for NativeFrameSurface {}

#[derive(Debug)]
struct NativeFrameLease {
    pool: Arc<NativeFramePool>,
}

impl Drop for NativeFrameLease {
    fn drop(&mut self) {
        self.pool.in_use.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Debug)]
pub(crate) struct NativeFramePool {
    capacity: usize,
    in_use: AtomicUsize,
    dropped: AtomicUsize,
}

impl NativeFramePool {
    pub(crate) fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            capacity: capacity.max(1),
            in_use: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
        })
    }

    pub(crate) fn try_wrap(
        self: &Arc<Self>,
        pixel_buffer: CFRetained<CVPixelBuffer>,
    ) -> XCapResult<Option<NativeFrameSurface>> {
        if CVPixelBufferGetPixelFormatType(&pixel_buffer) != kCVPixelFormatType_32BGRA {
            return Err(XCapError::new(
                "native ScreenCaptureKit surface is not 32-bit BGRA",
            ));
        }
        let acquired = self
            .in_use
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |in_use| {
                (in_use < self.capacity).then_some(in_use + 1)
            })
            .is_ok();
        if !acquired {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return Ok(None);
        }
        let lease = Arc::new(NativeFrameLease { pool: self.clone() });
        Ok(Some(NativeFrameSurface {
            pixel_buffer,
            _lease: Some(lease),
        }))
    }

    #[cfg(test)]
    fn in_use(&self) -> usize {
        self.in_use.load(Ordering::Acquire)
    }

    pub(crate) fn dropped_frames(&self) -> usize {
        self.dropped.load(Ordering::Acquire)
    }
}

impl fmt::Debug for NativeFrameSurface {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeFrameSurface")
            .field("width", &self.width())
            .field("height", &self.height())
            .field("stride", &self.stride())
            .finish_non_exhaustive()
    }
}

impl NativeFrameSurface {
    pub(crate) fn new(pixel_buffer: CFRetained<CVPixelBuffer>) -> XCapResult<Self> {
        if CVPixelBufferGetPixelFormatType(&pixel_buffer) != kCVPixelFormatType_32BGRA {
            return Err(XCapError::new(
                "native ScreenCaptureKit surface is not 32-bit BGRA",
            ));
        }
        Ok(Self {
            pixel_buffer,
            _lease: None,
        })
    }

    pub fn width(&self) -> u32 {
        CVPixelBufferGetWidth(&self.pixel_buffer) as u32
    }

    pub fn height(&self) -> u32 {
        CVPixelBufferGetHeight(&self.pixel_buffer) as u32
    }

    pub fn stride(&self) -> usize {
        CVPixelBufferGetBytesPerRow(&self.pixel_buffer)
    }

    pub fn byte_len(&self) -> usize {
        self.stride().saturating_mul(self.height() as usize)
    }

    /// Produces a compact compatibility copy only when a non-native encoder
    /// must take over after the native backend failed.
    pub fn copy_bgra(&self) -> XCapResult<Vec<u8>> {
        self.with_locked_bgra(|data, width, height, stride| {
            let row_bytes = width
                .checked_mul(4)
                .ok_or_else(|| XCapError::new("native surface row size overflow"))?;
            let output_len = row_bytes
                .checked_mul(height)
                .ok_or_else(|| XCapError::new("native surface size overflow"))?;
            let mut output = vec![0_u8; output_len];
            if stride == row_bytes {
                output.copy_from_slice(&data[..output_len]);
            } else {
                for row in 0..height {
                    let source_start = row * stride;
                    let destination_start = row * row_bytes;
                    output[destination_start..destination_start + row_bytes]
                        .copy_from_slice(&data[source_start..source_start + row_bytes]);
                }
            }
            Ok(output)
        })
    }

    /// Samples BGRA directly from the IOSurface-backed CVPixelBuffer into a
    /// low-resolution luma plane. No full-resolution intermediate is created.
    pub fn luma_thumbnail(&self, target_width: u32, target_height: u32) -> XCapResult<Vec<u8>> {
        if target_width == 0 || target_height == 0 {
            return Ok(Vec::new());
        }
        self.with_locked_bgra(|data, width, height, stride| {
            let mut output =
                vec![0_u8; (target_width as usize).saturating_mul(target_height as usize)];
            for target_y in 0..target_height {
                let source_y = u64::from(target_y) * height as u64 / u64::from(target_height);
                for target_x in 0..target_width {
                    let source_x = u64::from(target_x) * width as u64 / u64::from(target_width);
                    let index = source_y as usize * stride + source_x as usize * 4;
                    let blue = u32::from(data[index]);
                    let green = u32::from(data[index + 1]);
                    let red = u32::from(data[index + 2]);
                    output[target_y as usize * target_width as usize + target_x as usize] =
                        ((red * 77 + green * 150 + blue * 29) >> 8) as u8;
                }
            }
            Ok(output)
        })
    }

    pub(crate) fn pixel_buffer(&self) -> &CVPixelBuffer {
        &self.pixel_buffer
    }

    fn with_locked_bgra<T>(
        &self,
        operation: impl FnOnce(&[u8], usize, usize, usize) -> XCapResult<T>,
    ) -> XCapResult<T> {
        unsafe {
            let flags = CVPixelBufferLockFlags::ReadOnly;
            let status = CVPixelBufferLockBaseAddress(&self.pixel_buffer, flags);
            if status != 0 {
                return Err(XCapError::new(format!(
                    "failed to lock native CVPixelBuffer: {status}"
                )));
            }
            defer! {
                CVPixelBufferUnlockBaseAddress(&self.pixel_buffer, flags);
            }

            let width = CVPixelBufferGetWidth(&self.pixel_buffer);
            let height = CVPixelBufferGetHeight(&self.pixel_buffer);
            let stride = CVPixelBufferGetBytesPerRow(&self.pixel_buffer);
            let row_bytes = width
                .checked_mul(4)
                .ok_or_else(|| XCapError::new("native surface row size overflow"))?;
            if stride < row_bytes {
                return Err(XCapError::new("native surface stride is too small"));
            }
            let base_address = CVPixelBufferGetBaseAddress(&self.pixel_buffer);
            if base_address.is_null() {
                return Err(XCapError::new(
                    "native CVPixelBuffer has no CPU-visible base address",
                ));
            }
            let length = stride
                .checked_mul(height)
                .ok_or_else(|| XCapError::new("native surface size overflow"))?;
            let data = slice::from_raw_parts(base_address.cast::<u8>(), length);
            operation(data, width, height, stride)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ptr::{self, NonNull};
    use std::slice;

    use objc2_core_foundation::CFRetained;
    use objc2_core_video::{
        CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress,
        CVPixelBufferGetBytesPerRow, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
        CVPixelBufferUnlockBaseAddress, kCVPixelFormatType_32BGRA, kCVReturnSuccess,
    };

    use super::{NativeFramePool, NativeFrameSurface};

    #[test]
    fn thumbnail_samples_the_retained_bgra_surface_without_full_copy() {
        let buffer = make_buffer(2, 2);
        fill_pixel(&buffer, 0, 0, [0, 0, 255, 255]);
        fill_pixel(&buffer, 1, 0, [0, 255, 0, 255]);
        fill_pixel(&buffer, 0, 1, [255, 0, 0, 255]);
        fill_pixel(&buffer, 1, 1, [255, 255, 255, 255]);
        let surface = NativeFrameSurface::new(buffer).unwrap();

        assert_eq!(
            surface.luma_thumbnail(2, 2).unwrap(),
            vec![76, 149, 28, 255]
        );
        assert_eq!(
            &surface.copy_bgra().unwrap()[..8],
            &[0, 0, 255, 255, 0, 255, 0, 255]
        );
    }

    #[test]
    fn native_surface_pool_bounds_distinct_full_frame_buffers_to_two() {
        let pool = NativeFramePool::new(2);
        let first = pool.try_wrap(make_buffer(2, 2)).unwrap().unwrap();
        let first_clone = first.clone();
        let second = pool.try_wrap(make_buffer(2, 2)).unwrap().unwrap();

        assert!(pool.try_wrap(make_buffer(2, 2)).unwrap().is_none());
        assert_eq!(pool.in_use(), 2);
        assert_eq!(pool.dropped_frames(), 1);

        drop(first);
        assert_eq!(pool.in_use(), 2);
        drop(first_clone);
        assert_eq!(pool.in_use(), 1);
        let third = pool.try_wrap(make_buffer(2, 2)).unwrap().unwrap();
        assert_eq!(pool.in_use(), 2);
        drop(third);
        drop(second);
        assert_eq!(pool.in_use(), 0);
    }

    fn make_buffer(width: usize, height: usize) -> CFRetained<CVPixelBuffer> {
        let mut buffer = ptr::null_mut();
        let status = unsafe {
            CVPixelBufferCreate(
                None,
                width,
                height,
                kCVPixelFormatType_32BGRA,
                None,
                NonNull::from(&mut buffer),
            )
        };
        assert_eq!(status, kCVReturnSuccess);
        unsafe { CFRetained::from_raw(NonNull::new(buffer).unwrap()) }
    }

    fn fill_pixel(buffer: &CVPixelBuffer, x: usize, y: usize, value: [u8; 4]) {
        let flags = CVPixelBufferLockFlags::empty();
        assert_eq!(unsafe { CVPixelBufferLockBaseAddress(buffer, flags) }, 0);
        let base_address = CVPixelBufferGetBaseAddress(buffer);
        let stride = CVPixelBufferGetBytesPerRow(buffer);
        let pixels = unsafe { slice::from_raw_parts_mut(base_address.cast::<u8>(), stride * 2) };
        let start = y * stride + x * 4;
        pixels[start..start + 4].copy_from_slice(&value);
        unsafe {
            CVPixelBufferUnlockBaseAddress(buffer, flags);
        }
    }
}
