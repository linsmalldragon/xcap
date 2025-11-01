# 显示器 UUID 和序列号 - 快速开始指南

## 🚀 5 分钟上手

### 1. 添加依赖

```toml
[dependencies]
xcap = "0.7"
```

### 2. 基础代码

```rust
use xcap::Monitor;

fn main() {
    // 获取所有显示器
    let monitors = Monitor::all().unwrap();
    
    for monitor in monitors {
        let name = monitor.name().unwrap();
        
        // 获取 UUID（持久化标识符）
        if let Ok(uuid) = monitor.uuid() {
            println!("{}: UUID = {}", name, uuid);
        }
        
        // 获取序列号
        if let Ok(serial) = monitor.serial_number() {
            println!("{}: Serial = {}", name, serial);
        }
    }
}
```

### 3. 运行

```bash
cargo run
```

---

## 📋 常见用例

### 用例 1: 保存显示器特定的配置

```rust
use std::collections::HashMap;
use xcap::Monitor;

struct MonitorConfig {
    brightness: u8,
    color_profile: String,
}

fn save_config_by_uuid() {
    let monitors = Monitor::all().unwrap();
    let mut configs: HashMap<String, MonitorConfig> = HashMap::new();
    
    for monitor in monitors {
        if let Ok(uuid) = monitor.uuid() {
            let config = MonitorConfig {
                brightness: 80,
                color_profile: "sRGB".to_string(),
            };
            configs.insert(uuid, config);
        }
    }
    
    // 保存到文件...
}
```

### 用例 2: 检测显示器变化

```rust
use xcap::Monitor;

fn detect_monitor_changes(previous_uuids: &[String]) -> bool {
    let monitors = Monitor::all().unwrap();
    let current_uuids: Vec<String> = monitors
        .iter()
        .filter_map(|m| m.uuid().ok())
        .collect();
    
    current_uuids != previous_uuids
}
```

### 用例 3: 识别特定显示器

```rust
use xcap::Monitor;

fn find_monitor_by_serial(target_serial: &str) -> Option<Monitor> {
    Monitor::all()
        .ok()?
        .into_iter()
        .find(|m| {
            m.serial_number()
                .map(|s| s == target_serial)
                .unwrap_or(false)
        })
}
```

---

## 🎯 平台差异

| 平台 | UUID 示例 | 序列号示例 |
|------|-----------|-----------|
| **macOS** | `37D8832A-2D66-02CA-B9F7-8F30A301B230` | `4251086178` |
| **Windows** | `DEL-4070-12345678` | `12345678` |
| **Linux** | `DEL-4070-46C3A3B4` | `46C3A3B4` 或 `"CN123456789"` |

---

## ⚡ 性能提示

### DO ✅
```rust
// 启动时获取一次，缓存结果
let monitor_info: Vec<(String, String)> = Monitor::all()?
    .iter()
    .filter_map(|m| {
        let uuid = m.uuid().ok()?;
        let name = m.name().ok()?;
        Some((uuid, name))
    })
    .collect();
```

### DON'T ❌
```rust
// 不要在循环中频繁调用
for _ in 0..1000 {
    let uuid = monitor.uuid()?; // 在 Windows 上很慢！
}
```

---

## 🐛 错误处理

### 推荐方式

```rust
use xcap::Monitor;

fn get_monitor_identity(monitor: &Monitor) -> String {
    // 尝试 UUID（首选）
    if let Ok(uuid) = monitor.uuid() {
        return format!("UUID:{}", uuid);
    }
    
    // 回退到序列号
    if let Ok(serial) = monitor.serial_number() {
        return format!("Serial:{}", serial);
    }
    
    // 最后回退到名称
    if let Ok(name) = monitor.name() {
        return format!("Name:{}", name);
    }
    
    "Unknown".to_string()
}
```

---

## 📖 完整文档

- [跨平台兼容性详情](CROSS_PLATFORM_COMPATIBILITY.md)
- [macOS 平台说明](MACOS_COMPATIBILITY.md)
- [实现总结](IMPLEMENTATION_SUMMARY.md)
- [API 文档](https://docs.rs/xcap)

---

## 🤝 需要帮助？

如果遇到问题：

1. 检查 [故障排除指南](CROSS_PLATFORM_COMPATIBILITY.md#故障排除)
2. 运行 `cargo run --example monitor` 查看输出
3. 在 [GitHub](https://github.com/nashaofu/xcap) 提交 issue

---

**享受编程！** 🎉

