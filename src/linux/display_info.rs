//! Linux 显示器信息获取工具
//!
//! 本模块提供了 Linux 平台的显示器 UUID 和序列号获取功能。
//! 使用 XCB RandR 扩展获取 EDID 信息。
//!
//! # 兼容性
//!
//! - ✅ X11 - 完全支持通过 RandR 扩展获取 EDID
//! - ✅ Wayland - 有限支持（取决于混成器）
//! - ✅ 无需特殊权限

use std::ffi::CStr;
use xcb::{
    randr::{GetOutputInfo, GetOutputProperty, Output},
    x::ATOM_INTEGER,
    Xid,
};

use crate::error::{XCapError, XCapResult};

use super::utils::{get_atom, get_xcb_connection_and_index};

/// EDID 数据结构
#[derive(Debug)]
struct EdidInfo {
    manufacturer_id: String,
    product_code: u16,
    serial_number: u32,
}

/// 解析 EDID 数据
/// EDID 格式参考: https://en.wikipedia.org/wiki/Extended_Display_Identification_Data
fn parse_edid(edid_data: &[u8]) -> XCapResult<EdidInfo> {
    if edid_data.len() < 128 {
        return Err(XCapError::new("EDID data too short"));
    }

    // 验证 EDID 头部 (00 FF FF FF FF FF FF 00)
    if &edid_data[0..8] != &[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00] {
        return Err(XCapError::new("Invalid EDID header"));
    }

    // 提取制造商 ID (字节 8-9)
    let manufacturer_bytes = u16::from_be_bytes([edid_data[8], edid_data[9]]);
    let char1 = (((manufacturer_bytes >> 10) & 0x1F) + 64) as u8 as char;
    let char2 = (((manufacturer_bytes >> 5) & 0x1F) + 64) as u8 as char;
    let char3 = ((manufacturer_bytes & 0x1F) + 64) as u8 as char;
    let manufacturer_id = format!("{}{}{}", char1, char2, char3);

    // 提取产品代码 (字节 10-11, 小端序)
    let product_code = u16::from_le_bytes([edid_data[10], edid_data[11]]);

    // 提取序列号 (字节 12-15, 小端序)
    let serial_number = u32::from_le_bytes([
        edid_data[12],
        edid_data[13],
        edid_data[14],
        edid_data[15],
    ]);

    Ok(EdidInfo {
        manufacturer_id,
        product_code,
        serial_number,
    })
}

/// 获取显示器的 EDID 数据
fn get_edid_data(output: Output) -> XCapResult<Vec<u8>> {
    let (conn, _) = get_xcb_connection_and_index()?;

    // 获取 EDID 属性的 Atom
    let edid_atom = get_atom(&conn, "EDID")?;

    // 请求 EDID 属性
    let cookie = conn.send_request(&GetOutputProperty {
        output,
        property: edid_atom,
        r#type: ATOM_INTEGER,
        long_offset: 0,
        long_length: 128, // EDID 基本块大小为 128 字节
        delete: false,
        pending: false,
    });

    let reply = conn.wait_for_reply(cookie)?;

    if reply.format() == 8 && reply.num_items() >= 128 {
        Ok(reply.data().to_vec())
    } else {
        Err(XCapError::new("Failed to get valid EDID data"))
    }
}

/// 获取显示器 UUID
/// 使用 EDID 信息生成唯一标识符
pub fn get_display_uuid(output: Output) -> XCapResult<String> {
    // 方法1：尝试从 EDID 获取
    match get_edid_data(output) {
        Ok(edid_data) => {
            if let Ok(edid_info) = parse_edid(&edid_data) {
                // 使用制造商ID、产品代码和序列号生成 UUID 格式的字符串
                let uuid = format!(
                    "{}-{:04X}-{:08X}",
                    edid_info.manufacturer_id, edid_info.product_code, edid_info.serial_number
                );
                return Ok(uuid);
            }
        }
        Err(_) => {}
    }

    // 方法2：如果无法获取 EDID，使用 Output ID 作为标识
    // 注意：Output ID 在 X server 重启后可能会变化
    let (conn, _) = get_xcb_connection_and_index()?;

    // 尝试获取输出名称作为更稳定的标识
    let screen_buf = super::utils::get_current_screen_buf()?;
    let screen_res = super::utils::get_monitor_info_buf()?;

    let output_info_cookie = conn.send_request(&GetOutputInfo {
        output,
        config_timestamp: screen_res.config_timestamp(),
    });

    if let Ok(output_info_reply) = conn.wait_for_reply(output_info_cookie) {
        let name_bytes = output_info_reply.name();
        if let Ok(name) = CStr::from_bytes_until_nul(name_bytes) {
            if let Ok(name_str) = name.to_str() {
                // 使用输出名称和 ID 组合
                return Ok(format!("OUTPUT-{}-{:X}", name_str, output.resource_id()));
            }
        }
    }

    // 方法3：最后的回退方案
    Ok(format!("OUTPUT-{:X}", output.resource_id()))
}

/// 获取显示器序列号
/// 从 EDID 中提取序列号
pub fn get_display_serial_number(output: Output) -> XCapResult<String> {
    // 方法1：尝试从 EDID 获取序列号
    match get_edid_data(output) {
        Ok(edid_data) => {
            if let Ok(edid_info) = parse_edid(&edid_data) {
                if edid_info.serial_number != 0 {
                    return Ok(edid_info.serial_number.to_string());
                }

                // 有些显示器序列号为 0，尝试提取描述符中的序列号字符串
                // EDID 字节 54-125 包含 4 个 18 字节的描述符块
                for i in 0..4 {
                    let offset = 54 + i * 18;
                    if offset + 18 <= edid_data.len() {
                        let descriptor = &edid_data[offset..offset + 18];
                        // 检查是否为序列号描述符 (tag = 0xFF)
                        if descriptor[0] == 0x00
                            && descriptor[1] == 0x00
                            && descriptor[2] == 0x00
                            && descriptor[3] == 0xFF
                        {
                            // 提取序列号字符串 (字节 5-17)
                            let serial_bytes = &descriptor[5..18];
                            let serial_str: String = serial_bytes
                                .iter()
                                .filter(|&&b| b >= 0x20 && b <= 0x7E) // 可打印 ASCII
                                .map(|&b| b as char)
                                .collect();

                            if !serial_str.is_empty() {
                                return Ok(serial_str.trim().to_string());
                            }
                        }
                    }
                }

                // 如果找不到字符串序列号，返回数字序列号
                if edid_info.serial_number != 0 {
                    return Ok(edid_info.serial_number.to_string());
                }

                // 返回制造商和产品代码组合作为标识
                return Ok(format!(
                    "{}-{:04X}",
                    edid_info.manufacturer_id, edid_info.product_code
                ));
            }
        }
        Err(_) => {}
    }

    // 方法2：如果无法获取 EDID，返回错误
    Err(XCapError::new(
        "Display serial number not available. EDID data could not be read.",
    ))
}

