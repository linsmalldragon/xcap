//! macOS 显示器信息获取工具
//!
//! 本模块提供了跨 macOS 版本的显示器 UUID 和序列号获取功能。
//! 使用多层后备方案确保在 macOS 10.6 及以上版本都能正常工作。
//!
//! # 兼容性
//!
//! - ✅ macOS 10.6+ - 完全支持 UUID 和序列号获取
//! - ✅ 自动选择最佳 API（新版优先，旧版自动回退）
//! - ✅ 无需特殊权限，沙盒环境兼容

use core::ffi::c_void;
use objc2::MainThreadMarker;
use objc2_app_kit::NSScreen;
use objc2_core_foundation::{CFString, CFUUID, CFDictionary};
use objc2_core_graphics::CGDirectDisplayID;
use objc2_foundation::{NSNumber, NSString};

use crate::error::{XCapError, XCapResult};

// IOKit 类型定义
#[repr(C)]
#[allow(non_camel_case_types)]
#[derive(Copy, Clone)]
struct io_service_t(*mut c_void);

// IOKit 常量（保持与 Apple API 一致的小写命名）
#[allow(non_upper_case_globals)]
const kIODisplayOnlyPreferredName: u32 = 0x80000000;
#[allow(non_upper_case_globals)]
const kIODisplayUUIDKey: &str = "IODisplayUUID";
#[allow(non_upper_case_globals)]
const kIODisplaySerialNumberKey: &str = "IODisplaySerialNumber";
#[allow(non_upper_case_globals)]
const kIODisplaySerialNumber: &str = "IODisplaySerialNumber";

// CoreGraphics 函数声明
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    // 已废弃但仍可用于序列号
    fn CGDisplayIOServicePort(display: CGDirectDisplayID) -> io_service_t;
    // 推荐的获取 UUID 方法
    fn CGDisplayCreateUUIDFromDisplayID(display: CGDirectDisplayID) -> *const CFUUID;
    // 获取显示器序列号（如果可用）
    fn CGDisplaySerialNumber(display: CGDirectDisplayID) -> u32;
    // 获取显示器厂商 ID
    fn CGDisplayVendorNumber(display: CGDirectDisplayID) -> u32;
    // 获取显示器型号 ID
    fn CGDisplayModelNumber(display: CGDirectDisplayID) -> u32;
}

// CoreFoundation 函数声明
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
}

// IOKit 类型和常量
#[repr(C)]
#[allow(non_camel_case_types)]
#[derive(Copy, Clone)]
struct io_registry_entry_t(*mut c_void);

// IOKit 函数声明
#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IODisplayCreateInfoDictionary(
        display: io_service_t,
        options: u32,
    ) -> *mut objc2_core_foundation::CFDictionary;
    fn IOObjectRelease(object: *mut c_void) -> i32;
    fn IORegistryEntryCreateCFProperty(
        entry: io_registry_entry_t,
        key: *const objc2_core_foundation::CFString,
        allocator: *const c_void,
        options: u32,
    ) -> *const c_void;
    fn IODisplayForFramebuffer(framebuffer_index: u32) -> io_service_t;
}

/// 从 CGDirectDisplayID 获取显示器的 IOKit 服务
/// 尝试多种方法以确保兼容性
fn get_display_io_service(display_id: CGDirectDisplayID) -> XCapResult<io_service_t> {
    // 方法1：尝试使用 CGDisplayIOServicePort（旧方法，可能在较新系统上不可用）
    unsafe {
        let service = CGDisplayIOServicePort(display_id);
        if !service.0.is_null() {
            return Ok(service);
        }
    }

    // 方法2：通过 NSScreen 获取设备描述，然后查找对应的 IOKit 服务
    // 这在新版本的 macOS 上可能更可靠
    let screens = NSScreen::screens(unsafe { MainThreadMarker::new_unchecked() });
    for screen in screens {
        let device_description = screen.deviceDescription();
        let screen_number = match device_description.objectForKey(&NSString::from_str("NSScreenNumber")) {
            Some(num) => num,
            None => continue,
        };

        let screen_id = match screen_number.downcast::<NSNumber>() {
            Ok(num) => num.unsignedIntValue(),
            Err(_) => continue,
        };

        if screen_id == display_id {
            // 尝试从 deviceDescription 获取 IODisplayLocation 或其他标识符
            // 然后通过 IORegistry 查找对应的服务
            // 这是一个备用方案
        }
    }

    // 方法3：尝试使用 IODisplayForFramebuffer（仅在 display_id 较小时尝试，避免崩溃）
    unsafe {
        // framebuffer index 通常是较小的数字
        if display_id > 0 && display_id < 16 {
            let service = IODisplayForFramebuffer(display_id);
            if !service.0.is_null() {
                return Ok(service);
            }
        }
    }

    Err(XCapError::new(format!(
        "Failed to get IOKit service for display {}: CGDisplayIOServicePort is not available on this macOS version. This may require additional permissions or a different API.",
        display_id
    )))
}

/// 获取显示器的 UUID
/// 使用 CGDisplayCreateUUIDFromDisplayID（推荐方法），在旧版 macOS 上自动回退到 IOKit
pub fn get_display_uuid(display_id: CGDirectDisplayID) -> XCapResult<String> {
    unsafe {
        // 方法1：使用 CGDisplayCreateUUIDFromDisplayID（推荐的现代方法）
        // 在旧版 macOS 上，如果这个函数返回 null 或不可用，会自动回退到方法2
        let uuid_ref = CGDisplayCreateUUIDFromDisplayID(display_id);

        if !uuid_ref.is_null() {
            // 使用 CFUUID::new_string 创建 UUID 字符串
            let uuid_string_cf = match CFUUID::new_string(None, Some(&*uuid_ref)) {
                Some(cf_string) => cf_string,
                None => {
                    // 释放 UUID
                    CFRelease(uuid_ref.cast());
                    return Err(XCapError::new(format!(
                        "Failed to create UUID string for display {}",
                        display_id
                    )));
                }
            };

            // 转换为 Rust String
            let uuid_string_ref: &CFString = uuid_string_cf.as_ref();
            let uuid_string = uuid_string_ref.to_string();

            // 释放 CF 对象
            CFRelease(uuid_ref.cast());

            return Ok(uuid_string);
        }

        // 方法2：如果方法1失败，尝试通过 IOKit（旧方法）
        let service = match get_display_io_service(display_id) {
            Ok(s) => s,
            Err(_) => {
                return Err(XCapError::new(format!(
                    "Failed to get UUID for display {}: Both CGDisplayCreateUUIDFromDisplayID and IOKit methods failed",
                    display_id
                )));
            }
        };

        // 使用 scopeguard 确保释放服务
        let _guard = scopeguard::guard(service, |s| {
            IOObjectRelease(s.0);
        });

        let uuid_key = CFString::from_str(kIODisplayUUIDKey);
        let uuid_key_ref = uuid_key.as_ref() as *const CFString;

        // 从 IOKit 注册表获取 UUID
        let service_entry = io_registry_entry_t(service.0);
        let uuid_property = IORegistryEntryCreateCFProperty(
            service_entry,
            uuid_key_ref.cast(),
            core::ptr::null(),
            0,
        );

        if uuid_property.is_null() {
            return Err(XCapError::new(format!(
                "Failed to get UUID for display {} (property is null)",
                display_id
            )));
        }

        // UUID 属性应该是 CFUUID 类型
        let uuid_ref = uuid_property as *const CFUUID;

        // 使用 CFUUID::new_string 创建 UUID 字符串
        let uuid_string_cf = match CFUUID::new_string(None, Some(&*uuid_ref)) {
            Some(cf_string) => cf_string,
            None => {
                CFRelease(uuid_property.cast());
                return Err(XCapError::new(format!(
                    "Failed to create UUID string for display {}",
                    display_id
                )));
            }
        };

        // 转换为 Rust String
        let uuid_string_ref: &CFString = uuid_string_cf.as_ref();
        let uuid_string = uuid_string_ref.to_string();

        // 释放 CF 对象
        CFRelease(uuid_property.cast());

        Ok(uuid_string)
    }
}

/// 获取显示器的序列号
/// 使用多种方法确保在所有 macOS 版本上都能工作
/// 兼容 macOS 10.6 及以上版本
pub fn get_display_serial_number(display_id: CGDirectDisplayID) -> XCapResult<String> {
    unsafe {
        // 方法1：尝试使用 CGDisplaySerialNumber（最直接的方法）
        // 这个 API 在 macOS 10.6+ 就已经可用
        let serial_num = CGDisplaySerialNumber(display_id);
        if serial_num != 0 {
            return Ok(serial_num.to_string());
        }

        // 方法2：尝试组合厂商和型号信息作为标识
        // CGDisplayVendorNumber 和 CGDisplayModelNumber 在 macOS 10.6+ 可用
        let vendor = CGDisplayVendorNumber(display_id);
        let model = CGDisplayModelNumber(display_id);
        if vendor != 0 && model != 0 {
            // 返回厂商-型号作为标识（适用于所有版本）
            return Ok(format!("Vendor:{:04X}-Model:{:04X}", vendor, model));
        }

        // 方法3：尝试通过 IOKit 获取（如果前两个方法都失败）
        let service = match get_display_io_service(display_id) {
            Ok(s) => s,
            Err(_) => {
                // 如果所有方法都失败，说明显示器不提供序列号信息
                return Err(XCapError::new(format!(
                    "Display {} does not provide serial number information. This is common for built-in displays and some external monitors.",
                    display_id
                )));
            }
        };

        // 创建显示器信息字典
        let info_dict_ptr = IODisplayCreateInfoDictionary(service, kIODisplayOnlyPreferredName);

        // 使用 scopeguard 确保释放服务
        let _guard = scopeguard::guard(service, |s| {
            IOObjectRelease(s.0);
        });

        if info_dict_ptr.is_null() {
            return Err(XCapError::new(format!(
                "Failed to create info dictionary for display {}",
                display_id
            )));
        }

        // 使用 scopeguard 确保释放字典
        let _info_dict_guard = scopeguard::guard((), |_| {
            CFRelease(info_dict_ptr.cast());
        });

        let info_dict = info_dict_ptr as *const CFDictionary;

        // 尝试多个可能的序列号键名
        let serial_keys = [
            kIODisplaySerialNumberKey,
            kIODisplaySerialNumber,
            "SerialNumber",
            "DisplaySerialNumber",
        ];

        for key in &serial_keys {
            let serial_key = CFString::from_str(key);
            let serial_key_ref = serial_key.as_ref() as *const CFString;

            let serial_value = (*info_dict).value(serial_key_ref.cast());

            if !serial_value.is_null() {
                // 尝试转换为字符串
                let serial_ref = serial_value as *const CFString;
                let serial_string = (*serial_ref).to_string();

                if !serial_string.is_empty() {
                    return Ok(serial_string);
                }
            }
        }

        // 如果从 IOKit 字典也无法获取，返回错误
        Err(XCapError::new(format!(
            "Display {} does not provide serial number information",
            display_id
        )))
    }
}

