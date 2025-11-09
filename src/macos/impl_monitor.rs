use std::sync::mpsc::Receiver;

use image::RgbaImage;
use objc2::MainThreadMarker;
use objc2_app_kit::NSScreen;
use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayIsActive,
    CGDisplayIsBuiltin, CGDisplayIsMain, CGDisplayMode, CGDisplayRotation, CGError,
    CGGetActiveDisplayList, CGGetDisplaysWithPoint, CGWindowListOption,
};
use objc2_foundation::{NSNumber, NSString};

use crate::{
    error::{XCapError, XCapResult},
    video_recorder::Frame,
};

use super::{capture::capture, display_info, impl_video_recorder::ImplVideoRecorder};

#[derive(Debug, Clone)]
pub(crate) struct ImplMonitor {
    pub cg_direct_display_id: CGDirectDisplayID,
}

fn get_display_friendly_name(display_id: CGDirectDisplayID) -> XCapResult<String> {
    let screens = NSScreen::screens(unsafe { MainThreadMarker::new_unchecked() });
    for screen in screens {
        let device_description = screen.deviceDescription();
        let screen_number = device_description
            .objectForKey(&NSString::from_str("NSScreenNumber"))
            .ok_or(XCapError::new("Get NSScreenNumber failed"))?;

        let screen_id = screen_number
            .downcast::<NSNumber>()
            .map_err(|err| XCapError::new(format!("{:?}", err)))?
            .unsignedIntValue();

        if screen_id == display_id {
            unsafe { return Ok(screen.localizedName().to_string()) };
        }
    }

    Err(XCapError::new(format!(
        "Get display {} friendly name failed",
        display_id
    )))
}

impl ImplMonitor {
    pub fn new(cg_direct_display_id: CGDirectDisplayID) -> ImplMonitor {
        ImplMonitor {
            cg_direct_display_id,
        }
    }
    pub fn all() -> XCapResult<Vec<ImplMonitor>> {
        let max_displays: u32 = 16;
        let mut active_displays: Vec<CGDirectDisplayID> = vec![0; max_displays as usize];
        let mut display_count: u32 = 0;

        let cg_error = unsafe {
            CGGetActiveDisplayList(
                max_displays,
                active_displays.as_mut_ptr(),
                &mut display_count,
            )
        };

        if cg_error != CGError::Success {
            return Err(XCapError::new(format!(
                "CGGetActiveDisplayList failed: {:?}",
                cg_error
            )));
        }

        active_displays.truncate(display_count as usize);

        let mut impl_monitors = Vec::with_capacity(active_displays.len());

        for display in active_displays {
            impl_monitors.push(ImplMonitor::new(display));
        }

        Ok(impl_monitors)
    }

    pub fn from_point(x: i32, y: i32) -> XCapResult<ImplMonitor> {
        let point = CGPoint {
            x: x as f64,
            y: y as f64,
        };

        let max_displays: u32 = 16;
        let mut display_ids: Vec<CGDirectDisplayID> = vec![0; max_displays as usize];
        let mut display_count: u32 = 0;

        let cg_error = unsafe {
            CGGetDisplaysWithPoint(
                point,
                max_displays,
                display_ids.as_mut_ptr(),
                &mut display_count,
            )
        };

        if cg_error != CGError::Success {
            return Err(XCapError::new(format!(
                "CGGetDisplaysWithPoint failed: {:?}",
                cg_error
            )));
        }

        if display_count == 0 {
            return Err(XCapError::new("Monitor not found"));
        }

        if let Some(&display_id) = display_ids.first() {
            if unsafe { !CGDisplayIsActive(display_id) } {
                return Err(XCapError::new("Monitor is not active"));
            }
            Ok(ImplMonitor::new(display_id))
        } else {
            Err(XCapError::new("Monitor not found"))
        }
    }

    pub fn from_unique_key(unique_key: String) -> XCapResult<ImplMonitor> {
        let monitors = ImplMonitor::all()?;

        for monitor in monitors {
            // 1. 优先检查序列号（硬件属性，最可靠）
            if let Ok(serial) = monitor.serial_number() {
                if !serial.is_empty() && serial == unique_key {
                    return Ok(monitor);
                }
            }

            // 2. 检查 UUID
            if let Ok(uuid) = monitor.uuid() {
                if uuid == unique_key {
                    return Ok(monitor);
                }
            }

            // 3. 最后检查显示器 ID
            if let Ok(id) = monitor.id() {
                if id.to_string() == unique_key {
                    return Ok(monitor);
                }
            }
        }

        Err(XCapError::new(format!(
            "Monitor with unique_key '{}' not found",
            unique_key
        )))
    }
}

impl ImplMonitor {
    pub fn id(&self) -> XCapResult<u32> {
        Ok(self.cg_direct_display_id)
    }

    pub fn name(&self) -> XCapResult<String> {
        let name = get_display_friendly_name(self.cg_direct_display_id)
            .unwrap_or(format!("Unknown Monitor {}", self.cg_direct_display_id));

        Ok(name)
    }

    pub fn x(&self) -> XCapResult<i32> {
        let rect = unsafe { CGDisplayBounds(self.cg_direct_display_id) };

        Ok(rect.origin.x as i32)
    }

    pub fn y(&self) -> XCapResult<i32> {
        let cg_rect = unsafe { CGDisplayBounds(self.cg_direct_display_id) };

        Ok(cg_rect.origin.y as i32)
    }

    pub fn width(&self) -> XCapResult<u32> {
        let cg_rect = unsafe { CGDisplayBounds(self.cg_direct_display_id) };

        Ok(cg_rect.size.width as u32)
    }

    pub fn height(&self) -> XCapResult<u32> {
        let cg_rect = unsafe { CGDisplayBounds(self.cg_direct_display_id) };

        Ok(cg_rect.size.height as u32)
    }

    pub fn rotation(&self) -> XCapResult<f32> {
        let rotation = unsafe { CGDisplayRotation(self.cg_direct_display_id) };

        Ok(rotation as f32)
    }

    pub fn scale_factor(&self) -> XCapResult<f32> {
        let display_mode = unsafe { CGDisplayCopyDisplayMode(self.cg_direct_display_id) };
        let pixel_width = unsafe { CGDisplayMode::pixel_width(display_mode.as_deref()) };
        let width = self.width()?;

        Ok(pixel_width as f32 / width as f32)
    }

    pub fn frequency(&self) -> XCapResult<f32> {
        let frequency = unsafe {
            let display_mode = CGDisplayCopyDisplayMode(self.cg_direct_display_id);
            CGDisplayMode::refresh_rate(display_mode.as_deref())
        };

        Ok(frequency as f32)
    }

    pub fn is_primary(&self) -> XCapResult<bool> {
        let is_primary = unsafe { CGDisplayIsMain(self.cg_direct_display_id) };

        Ok(is_primary)
    }

    pub fn is_builtin(&self) -> XCapResult<bool> {
        let is_builtin = unsafe { CGDisplayIsBuiltin(self.cg_direct_display_id) };

        Ok(is_builtin)
    }

    pub fn capture_image(&self) -> XCapResult<RgbaImage> {
        let cg_rect = unsafe { CGDisplayBounds(self.cg_direct_display_id) };

        // 优化：直接传递 display_id，避免在 capture 函数中重复查找显示器
        capture(cg_rect, CGWindowListOption::OptionAll, 0, Some(self.cg_direct_display_id))
    }

    pub fn capture_region(&self, x: u32, y: u32, width: u32, height: u32) -> XCapResult<RgbaImage> {
        // Validate region bounds
        let monitor_x = self.x()?;
        let monitor_y = self.y()?;
        let monitor_width = self.width()?;
        let monitor_height = self.height()?;

        if width > monitor_width
            || height > monitor_height
            || x + width > monitor_width
            || y + height > monitor_height
        {
            return Err(XCapError::InvalidCaptureRegion(format!(
                "Region ({}, {}, {}, {}) is outside monitor bounds ({}, {}, {}, {})",
                x, y, width, height, monitor_x, monitor_y, monitor_width, monitor_height
            )));
        }

        // Create a CGRect for the region to capture
        let cg_rect = objc2_core_foundation::CGRect {
            origin: objc2_core_foundation::CGPoint {
                x: (monitor_x + x as i32) as f64,
                y: (monitor_y + y as i32) as f64,
            },
            size: objc2_core_foundation::CGSize {
                width: width as f64,
                height: height as f64,
            },
        };

        // 优化：直接传递 display_id，避免在 capture 函数中重复查找显示器
        capture(cg_rect, CGWindowListOption::OptionAll, 0, Some(self.cg_direct_display_id))
    }

    pub fn video_recorder(&self) -> XCapResult<(ImplVideoRecorder, Receiver<Frame>)> {
        ImplVideoRecorder::new(self.cg_direct_display_id)
    }

    /// 获取显示器的 UUID（持久化唯一标识符）
    /// 这个 UUID 在系统重启和显示器重新连接后保持不变
    pub fn uuid(&self) -> XCapResult<String> {
        display_info::get_display_uuid(self.cg_direct_display_id)
    }

    /// 获取显示器的序列号
    /// 某些显示器可能不提供序列号信息
    pub fn serial_number(&self) -> XCapResult<String> {
        display_info::get_display_serial_number(self.cg_direct_display_id)
    }
}
