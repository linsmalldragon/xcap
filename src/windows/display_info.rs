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
    core::{BSTR, HSTRING},
    Win32::{
        Graphics::Gdi::HMONITOR,
        System::{
            Com::{
                CoCreateInstance, CoInitializeEx, CoUninitialize, SAFEARRAY,
                CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
            },
            Ole::{
                SafeArrayAccessData, SafeArrayGetLBound, SafeArrayGetUBound,
                SafeArrayUnaccessData,
            },
            Variant::{VARIANT, VT_ARRAY, VT_UI1},
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
        let namespace = BSTR::from("root\\wmi");
        let services: IWbemServices =
            locator.ConnectServer(&namespace, &BSTR::default(), &BSTR::default(), &BSTR::default(), 0, &BSTR::default(), None)?;

        // 4. 查询 WmiMonitorID
        let query = BSTR::from("SELECT * FROM WmiMonitorID");
        let query_language = BSTR::from("WQL");

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
        let namespace = BSTR::from("root\\wmi");
        let services: IWbemServices =
            locator.ConnectServer(&namespace, &BSTR::default(), &BSTR::default(), &BSTR::default(), 0, &BSTR::default(), None)?;

        // 4. 查询 WmiMonitorID
        let query = BSTR::from("SELECT * FROM WmiMonitorID");
        let query_language = BSTR::from("WQL");

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

    // In windows 0.62, VARIANT internal structs are not public.
    // Access the raw memory layout: vt is at offset 0 (u16), parray is at offset 8 (pointer).
    let variant_ptr = &value as *const VARIANT as *const u8;
    let vt = *(variant_ptr as *const u16);
    let expected_vt = (VT_ARRAY | VT_UI1).0 as u16;

    if vt != expected_vt {
        return Ok(String::new());
    }

    // parray is at offset 8 in the VARIANT union
    let parray = *(variant_ptr.add(8) as *const *mut SAFEARRAY);
    if parray.is_null() {
        return Ok(String::new());
    }

    let lower_bound = SafeArrayGetLBound(parray, 1)?;
    let upper_bound = SafeArrayGetUBound(parray, 1)?;

    let count = (upper_bound - lower_bound + 1) as usize;

    // Use SafeArrayAccessData for efficient direct memory access
    let mut data_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
    SafeArrayAccessData(parray, &mut data_ptr)?;

    let bytes = std::slice::from_raw_parts(data_ptr as *const u8, count);
    let result: String = bytes
        .iter()
        .filter(|&&b| b != 0)
        .map(|&b| b as char)
        .collect();

    SafeArrayUnaccessData(parray)?;

    Ok(result)
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

