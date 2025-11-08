use std::{
    slice,
    sync::mpsc,
    time::Instant,
};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr};
use image::RgbaImage;
use objc2::{
    AllocAnyThread, DefinedClass, Message, define_class, msg_send, rc::Retained,
    runtime::ProtocolObject,
};
use objc2_core_foundation::CGRect;
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGWindowID, CGWindowListOption,
};
use objc2_core_media::CMSampleBuffer;
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{NSError, NSObject, NSObjectProtocol, NSProcessInfo};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamOutput,
    SCStreamOutputType,
};
use scopeguard::defer;

use crate::error::{XCapError, XCapResult};

use super::bgra_to_rgba;
use super::capture_compatible;


// 缓存 shareable_content 以减少重复获取的开销
//
// 注意：
// 1. 由于 Retained<SCShareableContent> 不是 Send + Sync，无法跨线程共享
// 2. 使用 thread_local! 实现线程本地缓存，每个线程有独立的缓存
// 3. 如果使用 spawn_blocking 或其他方式创建新线程，新线程第一次调用需要重新获取（~85ms）
// 4. 但在同一线程内的后续调用都能享受缓存（~3-6µs）
// 5. 缓存会一直存在直到线程结束（thread_local 会自动清理）
//
// 性能影响：
// - 单线程应用：完美，第一次后所有调用都很快
// - 多线程但每个线程只调用一次：第一次有开销，可接受
// - 多线程且每个线程多次调用：每个线程都有自己的缓存，性能仍然很好
thread_local! {
    static SHAREABLE_CONTENT_CACHE: std::cell::RefCell<Option<(Retained<SCShareableContent>, bool)>> = std::cell::RefCell::new(None);
}

// 流缓存结构：复用 SCStream 以避免重复创建和启动
struct StreamCache {
    stream: Retained<SCStream>,
    display_id: CGDirectDisplayID,
    width: usize,
    height: usize,
    is_started: bool,
}

// 线程本地流缓存，按 display_id 和尺寸缓存
thread_local! {
    static STREAM_CACHE: std::cell::RefCell<Option<StreamCache>> = std::cell::RefCell::new(None);
}

pub async fn capture(
    cg_rect: CGRect,
    list_option: CGWindowListOption,
    window_id: CGWindowID,
    display_id: Option<CGDirectDisplayID>,
) -> XCapResult<RgbaImage> {
    // 优先使用 ScreenCaptureKit（如果可用）
    // 深度优化：快速失败，如果 ScreenCaptureKit 超时或失败，立即回退
    if is_screencapturekit_available() {
        // 尝试使用 ScreenCaptureKit，但设置较短的超时以便快速回退
        match capture_with_screencapturekit(cg_rect, list_option, window_id, display_id).await {
            Ok(image) => return Ok(image),
            Err(_) => {
                // ScreenCaptureKit 不可用或失败，快速回退到 CGWindowListCreateImage
                // 注意：这里不打印错误，因为 ScreenCaptureKit 可能还未完全实现或超时
            }
        }
    }
    // 回退到传统的 CGWindowListCreateImage 方法（通常更快但已废弃）
    capture_compatible::capture_with_cgwindowlist(cg_rect, list_option, window_id).await
}

/// 检查 macOS 版本是否 >= 12.3 (ScreenCaptureKit 可用)
fn is_screencapturekit_available() -> bool {
    unsafe {
        let process_info = NSProcessInfo::processInfo();
        let version = process_info.operatingSystemVersion();

        // macOS 12.3 = major 12, minor 3
        // 或者 major > 12
        // NSOperatingSystemVersion 是结构体，使用字段访问
        let major = version.majorVersion;
        let minor = version.minorVersion;

        major > 12 || (major == 12 && minor >= 3)
    }
}

/// 从 CVPixelBuffer 转换为 RgbaImage（优化版本）
fn pixel_buffer_to_rgba_image(pixel_buffer: &CVPixelBuffer) -> XCapResult<RgbaImage> {
    unsafe {
        // 优化：在 debug 模式下减少字符串格式化开销
        let format_type = CVPixelBufferGetPixelFormatType(pixel_buffer);
        if format_type != kCVPixelFormatType_32BGRA {
            return Err(XCapError::new("Unsupported pixel format"));
        }

        CVPixelBufferLockBaseAddress(pixel_buffer, CVPixelBufferLockFlags::ReadOnly);
        defer! {
            CVPixelBufferUnlockBaseAddress(pixel_buffer, CVPixelBufferLockFlags::ReadOnly);
        }

        let width = CVPixelBufferGetWidth(pixel_buffer);
        let height = CVPixelBufferGetHeight(pixel_buffer);
        let bytes_per_row = CVPixelBufferGetBytesPerRow(pixel_buffer);
        let base_address = CVPixelBufferGetBaseAddress(pixel_buffer);

        if base_address.is_null() {
            return Err(XCapError::new("CVPixelBuffer base address is null"));
        }

        // 优化：如果 bytes_per_row == width * 4，可以直接使用，无需逐行拷贝
        let expected_row_size = width * 4;
        let mut buffer = Vec::with_capacity(width * height * 4);

        if bytes_per_row == expected_row_size {
            // 最优情况：行对齐，直接拷贝并转换
            let data = slice::from_raw_parts(base_address as *const u8, width * height * 4);
            buffer.reserve_exact(width * height * 4);

            // SIMD 优化：使用 SIMD 指令批量转换 BGRA -> RGBA
            bgra_to_rgba::convert_bgra_to_rgba_simd(data, &mut buffer);
        } else {
            // 需要处理行对齐的情况（较少见，但也可以使用 SIMD 优化）
            let data = slice::from_raw_parts(base_address as *const u8, bytes_per_row * height);
            buffer.reserve_exact(width * height * 4);

            // 逐行处理，每行使用 SIMD 优化
            unsafe {
                let mut dst_offset = 0;
                for row_idx in 0..height {
                    let row_start = row_idx * bytes_per_row;
                    let row_data = &data[row_start..row_start + expected_row_size];

                    // 对每行使用 SIMD 转换
                    let row_pixel_count = width;
                    let dst_ptr = buffer.as_mut_ptr().add(dst_offset);
                    let src_ptr = row_data.as_ptr();
                    bgra_to_rgba::convert_bgra_to_rgba_row(src_ptr, dst_ptr, row_pixel_count);

                    dst_offset += expected_row_size;
                }
                buffer.set_len(width * height * 4);
            }
        }

        RgbaImage::from_raw(width as u32, height as u32, buffer)
            .ok_or_else(|| XCapError::new("RgbaImage::from_raw failed"))
    }
}

/// 根据 CGRect 找到对应的显示器 ID
fn find_display_for_rect(cg_rect: CGRect) -> XCapResult<CGDirectDisplayID> {
    unsafe {
        // 获取所有活动显示器
        let max_displays: u32 = 16;
        let mut active_displays: Vec<CGDirectDisplayID> = vec![0; max_displays as usize];
        let mut display_count: u32 = 0;

        use objc2_core_graphics::CGGetActiveDisplayList;
        let cg_error = CGGetActiveDisplayList(
            max_displays,
            active_displays.as_mut_ptr(),
            &mut display_count,
        );

        if cg_error != objc2_core_graphics::CGError::Success {
            return Err(XCapError::new("Failed to get active display list"));
        }

        active_displays.truncate(display_count as usize);

        // 找到包含指定区域的显示器
        let rect_center_x = cg_rect.origin.x + cg_rect.size.width / 2.0;
        let rect_center_y = cg_rect.origin.y + cg_rect.size.height / 2.0;

        for display_id in active_displays {
            let display_bounds = CGDisplayBounds(display_id);
            if rect_center_x >= display_bounds.origin.x
                && rect_center_x < display_bounds.origin.x + display_bounds.size.width
                && rect_center_y >= display_bounds.origin.y
                && rect_center_y < display_bounds.origin.y + display_bounds.size.height
            {
                return Ok(display_id);
            }
        }

        // 如果没找到，返回主显示器
        use objc2_core_graphics::CGMainDisplayID;
        Ok(CGMainDisplayID())
    }
}

// SCStreamOutput 协议实现的数据结构
#[derive(Debug)]
struct SCStreamOutputDelegateVars {
    tx: mpsc::Sender<Result<Retained<CVPixelBuffer>, Retained<NSError>>>,
}

// 实现 SCStreamOutput 协议的对象
define_class!(
    #[unsafe(super(NSObject))]
    #[name = "SCStreamOutputDelegate"]
    #[ivars = SCStreamOutputDelegateVars]
    #[derive(Debug)]
    struct SCStreamOutputDelegate;

    unsafe impl SCStreamOutput for SCStreamOutputDelegate {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn stream_did_output_sample_buffer_of_type(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            _type: SCStreamOutputType,
        ) {
            if let Some(pixel_buffer) = unsafe { CMSampleBuffer::image_buffer(sample_buffer) } {
                let retained: Retained<CVPixelBuffer> = pixel_buffer.into();
                let _ = self.ivars().tx.send(Ok(retained));
            }
        }

        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn stream_did_stop_with_error(&self, _stream: &SCStream, error: Option<&NSError>) {
            if let Some(err) = error {
                let _ = self.ivars().tx.send(Err(err.retain()));
            }
        }
    }
);

unsafe impl NSObjectProtocol for SCStreamOutputDelegate {}

impl SCStreamOutputDelegate {
    fn new(tx: mpsc::Sender<Result<Retained<CVPixelBuffer>, Retained<NSError>>>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(SCStreamOutputDelegateVars { tx });
        unsafe { msg_send![super(this), init] }
    }
}

async fn fetch_shareable_content(
    excluding_desktop_windows: bool,
) -> XCapResult<Retained<SCShareableContent>> {
    // 优化：检查线程本地缓存，如果缓存有效且参数匹配，直接返回
    // 缓存会一直存在直到程序结束，最大化性能提升
    let cache_hit = SHAREABLE_CONTENT_CACHE.with(|cache| {
        let cache_ref = cache.borrow();
        if let Some((ref content, cached_excluding)) = *cache_ref {
            if cached_excluding == excluding_desktop_windows {
                // 缓存命中，返回克隆的内容
                return Some(content.clone());
            }
        }
        None
    });

    if let Some(content) = cache_hit {
        return Ok(content);
    }

    // 缓存未命中或已过期，重新获取
    let (tx, rx) = mpsc::channel();
    let completion = RcBlock::new(
        move |content_ptr: *mut SCShareableContent, error_ptr: *mut NSError| {
            let result = unsafe {
                if let Some(err) = error_ptr.as_ref() {
                    Err(Some(err.retain()))
                } else if let Some(content) = content_ptr.as_ref() {
                    Ok(content.retain())
                } else {
                    Err(None)
                }
            };

            let _ = tx.send(result);
        },
    );

    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            excluding_desktop_windows,
            false,
            &completion,
        );
    }

    // 深度优化：减少等待时间到 500ms，通常系统响应很快
    // 注意：Retained<SCShareableContent> 在 Objective-C 运行时中是线程安全的
    // 由于 Retained 类型不是 Send，我们在当前线程上等待，使用 yield_now 来让出控制权
    let mut content = None;
    let start = std::time::Instant::now();
    while content.is_none() && start.elapsed() < std::time::Duration::from_millis(500) {
        match rx.try_recv() {
            Ok(result) => {
                content = Some(result);
                break;
            }
            Err(mpsc::TryRecvError::Empty) => {
                tokio::task::yield_now().await;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err(XCapError::new("Channel disconnected"));
            }
        }
    }
    let content = content.ok_or_else(|| XCapError::new("Timed out while fetching ScreenCaptureKit shareable content"))?;

    let content = match content {
        Ok(content) => content,
        Err(Some(err)) => return Err(XCapError::new(err.localizedDescription().to_string())),
        Err(None) => return Err(XCapError::new("ScreenCaptureKit returned no content")),
    };

    // 更新线程本地缓存（长期存在，直到程序结束）
    SHAREABLE_CONTENT_CACHE.with(|cache| {
        *cache.borrow_mut() = Some((content.clone(), excluding_desktop_windows));
    });

    Ok(content)
}

/// 使用 ScreenCaptureKit 进行屏幕捕获
async fn capture_with_screencapturekit(
    cg_rect: CGRect,
    _list_option: CGWindowListOption,
    _window_id: CGWindowID,
    display_id: Option<CGDirectDisplayID>,
) -> XCapResult<RgbaImage> {
    unsafe {
        let total_start = Instant::now();

        // 1. 获取可共享内容
        let t1 = Instant::now();
        let shareable_content = fetch_shareable_content(false).await?;
        eprintln!("[耗时] 1. fetch_shareable_content: {:?}", t1.elapsed());

        // 2. 获取显示器列表
        let t2 = Instant::now();
        let displays = shareable_content.displays();
        if displays.count() == 0 {
            return Err(XCapError::new("No displays found"));
        }
        eprintln!("[耗时] 2. 获取显示器列表: {:?}", t2.elapsed());

        // 3. 找到对应的显示器（如果提供了 display_id，直接使用；否则通过 rect 查找）
        let t3 = Instant::now();
        let target_display_id = display_id.unwrap_or_else(|| {
            find_display_for_rect(cg_rect).unwrap_or_else(|_| {
                use objc2_core_graphics::CGMainDisplayID;
                CGMainDisplayID()
            })
        });

        // 深度优化：直接查找显示器，减少变量重命名和 Option 的 unwrap 开销
        let mut display: Option<Retained<objc2_screen_capture_kit::SCDisplay>> = None;
        let display_count = displays.count();
        for i in 0..display_count {
            let d = displays.objectAtIndex(i);
            if d.displayID() == target_display_id {
                display = Some(d);
                break;
            }
        }
        let display = display.ok_or_else(|| XCapError::new("Target display not found"))?;
        eprintln!("[耗时] 3. 查找显示器: {:?}", t3.elapsed());

        // 4. 创建内容过滤器
        let t4 = Instant::now();
        let excluding_windows_filter: Retained<
            objc2_foundation::NSArray<objc2_screen_capture_kit::SCWindow>,
        > = objc2_foundation::NSArray::new();

        let content_filter = SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            display.as_ref(),
            excluding_windows_filter.as_ref(),
        );
        eprintln!("[耗时] 4. 创建内容过滤器: {:?}", t4.elapsed());

        // 5. 创建流配置
        let t5 = Instant::now();
        // 优化：使用 cg_rect 的尺寸，避免重复调用 CGDisplayBounds
        // 如果 cg_rect 尺寸为 0，说明需要获取完整显示器尺寸
        let (width, height) = if cg_rect.size.width > 0.0 && cg_rect.size.height > 0.0 {
            (
                cg_rect.size.width.round() as usize,
                cg_rect.size.height.round() as usize,
            )
        } else {
            // 只有在必要时才调用 CGDisplayBounds
            let bounds = CGDisplayBounds(target_display_id);
            (
                bounds.size.width.round() as usize,
                bounds.size.height.round() as usize,
            )
        };
        eprintln!("[耗时] 5. 创建流配置: {:?}", t5.elapsed());

        // 6. 创建输出处理器（每次都需要新的，因为需要新的 channel）
        let t6 = Instant::now();
        let (frame_tx, frame_rx) = mpsc::channel();
        let output_delegate = SCStreamOutputDelegate::new(frame_tx);
        let output_delegate_protocol =
            ProtocolObject::<dyn SCStreamOutput>::from_ref(&*output_delegate);
        eprintln!("[耗时] 6. 创建输出处理器: {:?}", t6.elapsed());

        // 7-9. 复用流或创建新流
        let t7 = Instant::now();
        let (stream, need_start): (Retained<SCStream>, bool) = STREAM_CACHE.with(|cache| -> XCapResult<(Retained<SCStream>, bool)> {
            let mut cache_ref = cache.borrow_mut();

            // 检查缓存：如果 display_id 和尺寸匹配，且流已启动，直接复用
            if let Some(ref cached) = *cache_ref {
                if cached.display_id == target_display_id
                    && cached.width == width
                    && cached.height == height
                    && cached.is_started
                {
                    eprintln!("[流复用] 复用已启动的流");
                    return Ok((cached.stream.clone(), false));
                }
            }

            // 缓存未命中或需要重新创建，创建新流
            eprintln!("[流复用] 创建新流");
            let stream_config = SCStreamConfiguration::new();
            stream_config.setWidth(width.max(1));
            stream_config.setHeight(height.max(1));
            stream_config.setPixelFormat(kCVPixelFormatType_32BGRA);
            stream_config.setQueueDepth(1);

            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                content_filter.as_ref(),
                stream_config.as_ref(),
                None,
            );

            // 更新缓存
            *cache_ref = Some(StreamCache {
                stream: stream.clone(),
                display_id: target_display_id,
                width,
                height,
                is_started: false, // 稍后会启动
            });

            Ok((stream, true))
        })?;
        eprintln!("[耗时] 7. 创建/复用流: {:?}", t7.elapsed());

        // 8. 添加输出（每次都需要新的 output_delegate 和队列）
        let t8 = Instant::now();
        let output_queue = DispatchQueue::new("SCStreamOutputQueue", DispatchQueueAttr::SERIAL);
        let output_queue_ref: &DispatchQueue = output_queue.as_ref();
        stream
            .addStreamOutput_type_sampleHandlerQueue_error(
                output_delegate_protocol.as_ref(),
                SCStreamOutputType::Screen,
                Some(output_queue_ref),
            )
            .map_err(|err| XCapError::new(err.localizedDescription().to_string()))?;
        eprintln!("[耗时] 8. 添加输出: {:?}", t8.elapsed());

        // 9. 启动流（如果需要）
        let t9 = Instant::now();
        if need_start {
            let (start_tx, start_rx) = mpsc::channel();
            let start_block = RcBlock::new(move |error_ptr: *mut NSError| {
                let result = unsafe {
                    if let Some(err) = error_ptr.as_ref() {
                        Err(err.retain())
                    } else {
                        Ok(())
                    }
                };
                let _ = start_tx.send(result);
            });

            stream.startCaptureWithCompletionHandler(Some(&start_block));

            // 深度优化：减少启动等待时间到 500ms，通常启动很快
            // 注意：Result 类型在 Objective-C 运行时中是线程安全的
            // 由于 Retained 类型不是 Send，我们在当前线程上等待，使用 yield_now 来让出控制权
            let t9_wait = Instant::now();
            let mut start_result = None;
            let start = std::time::Instant::now();
            while start_result.is_none() && start.elapsed() < std::time::Duration::from_millis(500) {
                match start_rx.try_recv() {
                    Ok(result) => {
                        start_result = Some(result);
                        break;
                    }
                    Err(mpsc::TryRecvError::Empty) => {
                        tokio::task::yield_now().await;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        return Err(XCapError::new("Channel disconnected"));
                    }
                }
            }
            match start_result {
                Some(Ok(())) => {
                    // 更新缓存状态为已启动
                    STREAM_CACHE.with(|cache| {
                        if let Some(ref mut cached) = *cache.borrow_mut() {
                            cached.is_started = true;
                        }
                    });
                }
                Some(Err(err)) => {
                    return Err(XCapError::new(err.localizedDescription().to_string()));
                }
                None => {
                    return Err(XCapError::new(
                        "Timed out while starting ScreenCaptureKit stream",
                    ));
                }
            }
            eprintln!(
                "[耗时] 9. 启动流 (等待): {:?}, 总计: {:?}",
                t9_wait.elapsed(),
                t9.elapsed()
            );
        } else {
            eprintln!("[耗时] 9. 启动流: 已复用，跳过启动");
        }

        // 10. 等待一帧数据（深度优化：减少等待时间到 500ms，通常第一帧很快就能获取到）
        // 注意：Retained<CVPixelBuffer> 在 Objective-C 运行时中是线程安全的
        // 由于 Retained 类型不是 Send，我们在当前线程上等待，使用 yield_now 来让出控制权
        let t10 = Instant::now();
        let mut frame_result = None;
        let start = std::time::Instant::now();
        while frame_result.is_none() && start.elapsed() < std::time::Duration::from_millis(500) {
            match frame_rx.try_recv() {
                Ok(result) => {
                    frame_result = Some(result);
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    tokio::task::yield_now().await;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(XCapError::new("Channel disconnected"));
                }
            }
        }
        let frame_result = frame_result.ok_or_else(|| XCapError::new("Timeout waiting for ScreenCaptureKit frame"))?;
        eprintln!("[耗时] 10. 等待一帧数据: {:?}", t10.elapsed());

        let t11 = Instant::now();
        let pixel_buffer = match frame_result {
            Ok(buffer) => buffer,
            Err(err) => {
                return Err(XCapError::new(err.localizedDescription().to_string()));
            }
        };
        // 优化：不停止流，保持运行状态以便下次复用
        // 只移除当前的 output，流继续运行
        let _ = stream
            .removeStreamOutput_type_error(output_delegate_protocol.as_ref(), SCStreamOutputType::Screen);
        eprintln!("[耗时] 11. 处理 pixel_buffer: {:?}", t11.elapsed());

        // 12. 转换为 RgbaImage
        let t12 = Instant::now();
        let result = pixel_buffer_to_rgba_image(pixel_buffer.as_ref());
        eprintln!("[耗时] 12. 转换为 RgbaImage: {:?}", t12.elapsed());
        eprintln!("[耗时] 总计: {:?}", total_start.elapsed());
        result
    }
}

