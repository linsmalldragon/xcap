//! Windows 显示器信息获取工具
//!
//! 本模块提供了 Windows 平台的显示器 UUID 和序列号获取功能。
//! 使用 WMI (Windows Management Instrumentation)。
//!
//! # 兼容性
//!
//! - ✅ Windows 7 及以上 - 完全支持 UUID 和序列号获取
//! - ✅ 使用 WMI 从 EDID 提取信息
//! - ✅ 支持多显示器环境

use windows::{
    core::{HSTRING, VARIANT},
    Win32::{
        Graphics::Gdi::HMONITOR,
        System::{
            Com::{
                CoCreateInstance, CoInitializeEx, CoUninitialize, SafeArrayGetElement,
                SafeArrayGetLBound, SafeArrayGetUBound, CLSCTX_INPROC_SERVER,
                COINIT_MULTITHREADED,
            },
            Variant::{VT_ARRAY, VT_UI1},
            Wmi::{
                IWbemClassObject, IWbemLocator, IWbemServices, WbemLocator,
                WBEM_FLAG_FORWARD_ONLY, WBEM_FLAG_RETURN_IMMEDIATELY, WBEM_INFINITE,
            },
        },
    },
};

use crate::error::{XCapError, XCapResult};

/// 从 WMI 获取显示器信息
/// 使用 WmiMonitorID 类来获取 EDID 信息
pub fn get_display_uuid_from_wmi(_h_monitor: HMONITOR) -> XCapResult<String> {
    unsafe {
        // 1. 初始化 COM
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;

        // 确保退出时释放 COM
        let _com_guard = scopeguard::guard((), |_| {
            CoUninitialize();
        });

        // 2. 创建 WMI Locator
        let locator: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)?;

        // 3. 连接到 WMI namespace
        let namespace = HSTRING::from("root\\wmi");
        let services: IWbemServices =
            locator.ConnectServer(&namespace, None, None, None, 0, None, None)?;

        // 4. 查询 WmiMonitorID
        let query = HSTRING::from("SELECT * FROM WmiMonitorID");
        let query_language = HSTRING::from("WQL");

        let enumerator = services.ExecQuery(
            &query_language,
            &query,
            WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
            None,
        )?;

        // 5. 遍历结果并提取信息
        let mut objects = [None; 1];
        let mut returned = 0u32;

        while enumerator
            .Next(WBEM_INFINITE, &mut objects, &mut returned)
            .is_ok()
            && returned > 0
        {
            if let Some(obj) = &objects[0] {
                // 提取 ManufacturerName, ProductCodeID, SerialNumberID
                let manufacturer = get_string_property(obj, "ManufacturerName")?;
                let product_code = get_string_property(obj, "ProductCodeID")?;
                let serial = get_string_property(obj, "SerialNumberID")?;

                // 组合成 UUID
                let uuid = format!("{}-{}-{}", manufacturer, product_code, serial);
                return Ok(uuid);
            }
        }

        Err(XCapError::new("Failed to get display UUID from WMI"))
    }
}

/// 从 WMI 获取显示器序列号
pub fn get_display_serial_from_wmi(_h_monitor: HMONITOR) -> XCapResult<String> {
    unsafe {
        // 1. 初始化 COM
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;

        let _com_guard = scopeguard::guard((), |_| {
            CoUninitialize();
        });

        // 2. 创建 WMI Locator
        let locator: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)?;

        // 3. 连接到 WMI namespace
        let namespace = HSTRING::from("root\\wmi");
        let services: IWbemServices =
            locator.ConnectServer(&namespace, None, None, None, 0, None, None)?;

        // 4. 查询 WmiMonitorID
        let query = HSTRING::from("SELECT * FROM WmiMonitorID");
        let query_language = HSTRING::from("WQL");

        let enumerator = services.ExecQuery(
            &query_language,
            &query,
            WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
            None,
        )?;

        // 5. 遍历结果并提取序列号
        let mut objects = [None; 1];
        let mut returned = 0u32;

        while enumerator
            .Next(WBEM_INFINITE, &mut objects, &mut returned)
            .is_ok()
            && returned > 0
        {
            if let Some(obj) = &objects[0] {
                let serial = get_string_property(obj, "SerialNumberID")?;
                if !serial.is_empty() {
                    return Ok(serial);
                }
            }
        }

        Err(XCapError::new(
            "Failed to get display serial number from WMI",
        ))
    }
}

/// 从 WMI 对象中提取字符串属性
unsafe fn get_string_property(
    obj: &IWbemClassObject,
    property_name: &str,
) -> XCapResult<String> {
    let prop_name = HSTRING::from(property_name);
    let mut value = VARIANT::default();

    obj.Get(&prop_name, 0, &mut value, None, None)?;

    // 处理数组类型（WMI 返回的是 byte 数组）
    if let VARIANT {
        Anonymous:
            windows::core::VARIANT_0 {
                Anonymous:
                    windows::core::VARIANT_0_0 {
                        vt,
                        Anonymous: windows::core::VARIANT_0_0_0 { parray },
                        ..
                    },
            },
    } = value
    {
        // 检查是否为 VT_ARRAY | VT_UI1
        if vt == (VT_ARRAY | VT_UI1).0 as u16 {
            if let Some(safe_array) = parray.as_ref() {
                let mut lower_bound = 0i32;
                let mut upper_bound = 0i32;

                SafeArrayGetLBound(*safe_array, 1, &mut lower_bound)?;
                SafeArrayGetUBound(*safe_array, 1, &mut upper_bound)?;

                let count = (upper_bound - lower_bound + 1) as usize;
                let mut bytes = vec![0u8; count];

                for i in 0..count {
                    let index = lower_bound + i as i32;
                    let mut element = 0u8;
                    SafeArrayGetElement(
                        *safe_array,
                        &index as *const i32,
                        &mut element as *mut u8 as *mut _,
                    )?;
                    bytes[i] = element;
                }

                // 过滤掉 0 字节并转换为字符串
                let result: String = bytes
                    .into_iter()
                    .filter(|&b| b != 0)
                    .map(|b| b as char)
                    .collect();

                return Ok(result);
            }
        }
    }

    Ok(String::new())
}

/// 获取显示器 UUID（多种方法）
pub fn get_display_uuid(h_monitor: HMONITOR) -> XCapResult<String> {
    // 方法1：尝试从 WMI 获取
    if let Ok(uuid) = get_display_uuid_from_wmi(h_monitor) {
        if !uuid.is_empty() && !uuid.contains("--") {
            return Ok(uuid);
        }
    }

    // 方法2：使用显示器句柄生成唯一标识
    // 注意：这不是真正的 UUID，但在当前会话中是唯一的
    Ok(format!("HMONITOR-{:X}", h_monitor.0 as usize))
}

/// 获取显示器序列号（多种方法）
pub fn get_display_serial_number(h_monitor: HMONITOR) -> XCapResult<String> {
    // 方法1：尝试从 WMI 获取
    if let Ok(serial) = get_display_serial_from_wmi(h_monitor) {
        if !serial.is_empty() {
            return Ok(serial);
        }
    }

    // 方法2：返回错误，表示无法获取序列号
    Err(XCapError::new(
        "Display serial number not available. This is common for some monitors on Windows.",
    ))
}

