/// 兼容性捕获模块：使用传统的 CGWindowListCreateImage API
/// 作为 ScreenCaptureKit 的回退方案

use image::RgbaImage;
use objc2_core_foundation::CGRect;
use objc2_core_graphics::{
    CGDataProvider, CGImage, CGWindowID, CGWindowImageOption, CGWindowListCreateImage,
    CGWindowListOption,
};

use crate::error::{XCapError, XCapResult};

use super::bgra_to_rgba::convert_bgra_to_rgba_row;

/// 使用 CGWindowListCreateImage 进行屏幕捕获（传统方法，已废弃）
/// 作为 ScreenCaptureKit 的回退方案
#[allow(deprecated)]
pub fn capture_with_cgwindowlist(
    cg_rect: CGRect,
    list_option: CGWindowListOption,
    window_id: CGWindowID,
) -> XCapResult<RgbaImage> {
    capture_with_cgwindowlist_sync(cg_rect, list_option, window_id)
}

/// 同步版本的 CGWindowListCreateImage 捕获
#[allow(deprecated)]
fn capture_with_cgwindowlist_sync(
    cg_rect: CGRect,
    list_option: CGWindowListOption,
    window_id: CGWindowID,
) -> XCapResult<RgbaImage> {
    unsafe {
        let cg_image = CGWindowListCreateImage(
            cg_rect,
            list_option,
            window_id,
            CGWindowImageOption::Default,
        );

        let width = CGImage::width(cg_image.as_deref());
        let height = CGImage::height(cg_image.as_deref());
        let data_provider = CGImage::data_provider(cg_image.as_deref());

        let data = CGDataProvider::data(data_provider.as_deref())
            .ok_or_else(|| XCapError::new("Failed to copy data"))?
            .to_vec();

        let bytes_per_row = CGImage::bytes_per_row(cg_image.as_deref());

        // Some platforms e.g. MacOS can have extra bytes at the end of each row.
        // See
        // https://github.com/nashaofu/xcap/issues/29
        // https://github.com/nashaofu/xcap/issues/38
        // 深度优化：使用预分配和批量操作
        let mut buffer = Vec::with_capacity(width * height * 4);
        buffer.reserve_exact(width * height * 4);

        // 优化：使用 SIMD 优化的 BGRA -> RGBA 转换
        let dst_ptr: *mut u8 = buffer.as_mut_ptr();
        let mut dst_offset = 0;
        for row in data.chunks_exact(bytes_per_row) {
            let row_data = &row[..width * 4];
            let src_ptr = row_data.as_ptr();
            let dst_row_ptr = dst_ptr.add(dst_offset);
            // 使用 SIMD 优化的行转换函数
            convert_bgra_to_rgba_row(src_ptr, dst_row_ptr, width);
            dst_offset += width * 4;
        }
        buffer.set_len(width * height * 4);

        RgbaImage::from_raw(width as u32, height as u32, buffer)
            .ok_or_else(|| XCapError::new("RgbaImage::from_raw failed"))
    }
}

