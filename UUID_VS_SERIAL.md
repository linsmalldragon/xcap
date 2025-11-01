# UUID vs 序列号 - 使用指南和持久性分析

## 📋 快速答案

### 🎯 **优先使用序列号 (Serial Number)**

**序列号是硬件属性，是唯一的、持久的标识符。**

---

## 📊 持久性对比表

| 场景 | UUID | 序列号 | 推荐 |
|------|------|--------|------|
| **重启电脑** | ✅ 不变 | ✅ 不变 | ✅ 两者都可用 |
| **重新插拔显示器** | ✅ 不变 | ✅ 不变 | ✅ 两者都可用 |
| **重装系统** | ⚠️ **可能变化** | ✅ **不变** | ✅ **优先用序列号** |
| **不同电脑插同一台显示器** | ⚠️ **可能不同** | ✅ **相同** | ✅ **优先用序列号** |
| **同一显示器换端口** | ✅ 通常不变 | ✅ 不变 | ✅ 两者都可用 |

---

## 🔍 详细分析

### UUID (通用唯一标识符)

#### macOS
- **来源**: `CGDisplayCreateUUIDFromDisplayID()` 或 IOKit
- **生成方式**: 基于显示器硬件特性由系统生成
- **持久性**:
  - ✅ **重启电脑后**: 保持不变
  - ✅ **重新插拔后**: 保持不变
  - ⚠️ **重装系统后**: **可能变化**（取决于系统如何重新识别显示器）
  - ⚠️ **不同电脑**: **可能不同**（不同系统可能生成不同 UUID）
- **示例**: `37D8832A-2D66-02CA-B9F7-8F30A301B230`

#### Windows
- **来源**: WMI `WmiMonitorID` (制造商 + 产品代码 + 序列号)
- **生成方式**: 从 EDID 数据组合生成
- **持久性**:
  - ✅ **重启电脑后**: 保持不变
  - ✅ **重新插拔后**: 保持不变
  - ✅ **重装系统后**: **保持不变**（因为基于 EDID）
  - ✅ **不同电脑**: **相同**（因为基于硬件 EDID）
- **示例**: `DEL-4070-12345678`

#### Linux
- **来源**: EDID (制造商ID + 产品代码 + 序列号)
- **生成方式**: 从 EDID 数据组合生成
- **持久性**:
  - ✅ **重启电脑后**: 保持不变（基于 EDID）
  - ✅ **重新插拔后**: 保持不变
  - ✅ **重装系统后**: **保持不变**（因为基于 EDID）
  - ✅ **不同电脑**: **相同**（因为基于硬件 EDID）
- **示例**: `DEL-4070-46C3A3B4`

**结论**:
- ✅ **Windows/Linux 的 UUID**: 基于 EDID，非常可靠，可以作为主要标识符
- ⚠️ **macOS 的 UUID**: 系统生成，在重装系统或不同电脑上可能不同

---

### 序列号 (Serial Number)

#### 所有平台
- **来源**: EDID 数据（显示器硬件中存储）
- **生成方式**: 制造商在生产时写入显示器硬件
- **持久性**:
  - ✅ **重启电脑后**: **绝对不变**
  - ✅ **重新插拔后**: **绝对不变**
  - ✅ **重装系统后**: **绝对不变**（硬件属性）
  - ✅ **不同电脑**: **绝对相同**（硬件属性）
  - ✅ **换端口/换线**: **绝对不变**
- **特点**:
  - 这是**硬件唯一标识符**
  - 就像显示器的"身份证号"
  - **最可靠的选择**

---

## 🎯 推荐使用策略

### 方案 1: 优先使用序列号（推荐）⭐⭐⭐⭐⭐

```rust
use xcap::Monitor;

fn get_display_identifier(monitor: &Monitor) -> Option<String> {
    // 优先使用序列号（硬件属性，最可靠）
    if let Ok(serial) = monitor.serial_number() {
        if !serial.is_empty() {
            return Some(format!("SERIAL:{}", serial));
        }
    }

    // 备用：使用 UUID
    monitor.uuid().ok()
}
```

**优点**:
- ✅ 绝对持久化
- ✅ 跨平台一致
- ✅ 跨系统一致
- ✅ 硬件属性，不可伪造

**缺点**:
- ⚠️ 某些显示器（特别是内置显示器）可能不提供序列号

---

### 方案 2: 组合使用（最可靠）⭐⭐⭐⭐⭐

```rust
use xcap::Monitor;

#[derive(Debug)]
struct DisplayIdentity {
    serial: Option<String>,
    uuid: Option<String>,
}

impl DisplayIdentity {
    fn from_monitor(monitor: &Monitor) -> Self {
        Self {
            serial: monitor.serial_number().ok(),
            uuid: monitor.uuid().ok(),
        }
    }

    /// 获取唯一标识符（优先序列号）
    fn identifier(&self) -> Option<String> {
        self.serial
            .as_ref()
            .or(self.uuid.as_ref())
            .map(|s| s.clone())
    }

    /// 用于数据库索引的组合键
    fn composite_key(&self) -> String {
        format!(
            "{}-{}",
            self.serial.as_deref().unwrap_or("NO_SERIAL"),
            self.uuid.as_deref().unwrap_or("NO_UUID")
        )
    }
}
```

**优点**:
- ✅ 最大兼容性
- ✅ 即使序列号不可用也能工作
- ✅ 可以提供双重验证

---

### 方案 3: 平台特定策略

```rust
use xcap::Monitor;

fn get_most_reliable_id(monitor: &Monitor) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        // macOS: UUID 可能在不同系统上不同，优先用序列号
        monitor.serial_number()
            .ok()
            .or_else(|| monitor.uuid().ok())
    }

    #[cfg(any(target_os = "windows", target_os = "linux"))]
    {
        // Windows/Linux: UUID 基于 EDID 很可靠，但序列号仍然是最可靠的
        monitor.serial_number()
            .ok()
            .or_else(|| monitor.uuid().ok())
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        None
    }
}
```

---

## 📝 实际应用场景

### 场景 1: 保存显示器配置

```rust
use std::collections::HashMap;

fn save_monitor_config(monitors: Vec<Monitor>) -> HashMap<String, MonitorConfig> {
    let mut configs = HashMap::new();

    for monitor in monitors {
        // 优先使用序列号作为键（最可靠）
        let key = monitor.serial_number()
            .ok()
            .or_else(|| monitor.uuid().ok())
            .unwrap_or_else(|| format!("MONITOR-{}", monitor.id().unwrap_or(0)));

        configs.insert(key, get_config_for_monitor(&monitor));
    }

    configs
}
```

**结果**: 即使在重装系统后，配置也能正确匹配到同一台显示器。

---

### 场景 2: 检测显示器变化

```rust
fn detect_display_changes(previous_ids: &[String]) -> bool {
    let current_ids: Vec<String> = Monitor::all()
        .unwrap()
        .iter()
        .filter_map(|m| {
            // 使用序列号检测（最可靠）
            m.serial_number().ok()
        })
        .collect();

    current_ids != previous_ids
}
```

---

### 场景 3: 数据库存储

```sql
CREATE TABLE monitor_configs (
    serial_number VARCHAR(255) PRIMARY KEY,  -- 主键用序列号
    uuid VARCHAR(255) UNIQUE,                -- UUID 作为辅助索引
    brightness INT,
    color_profile VARCHAR(255),
    -- ...
);

-- 查询示例
SELECT * FROM monitor_configs WHERE serial_number = '4251086178';
```

**优势**: 即使在不同电脑上，只要序列号相同，就能找到对应的配置。

---

## ⚠️ 注意事项

### 1. 序列号可能为空

某些情况下序列号可能不可用：
- **内置显示器**: MacBook 等设备的内置屏幕可能不提供序列号
- **老旧显示器**: 非常老的显示器可能没有 EDID 数据
- **虚拟显示器**: 虚拟机中的虚拟显示器
- **投影仪**: 某些投影仪可能不提供完整 EDID

**解决方案**: 准备后备方案（使用 UUID 或组合键）

---

### 2. macOS UUID 的特殊性

在 macOS 上，UUID 可能是系统生成的，因此：
- ✅ 在同一台电脑上：非常可靠
- ⚠️ 重装系统后：可能变化
- ⚠️ 不同电脑上：可能不同

**建议**: macOS 上优先使用序列号，UUID 作为备用。

---

### 3. 不同平台 UUID 格式不同

| 平台 | UUID 格式 | 示例 |
|------|-----------|------|
| macOS | 标准 UUID | `37D8832A-2D66-02CA-B9F7-8F30A301B230` |
| Windows | 组合格式 | `DEL-4070-12345678` |
| Linux | 组合格式 | `DEL-4070-46C3A3B4` |

**注意**: 如果需要跨平台匹配，建议使用序列号或建立映射表。

---

## 🎯 最终建议

### ✅ **推荐方案：序列号优先**

```rust
// 最佳实践代码
fn get_monitor_key(monitor: &Monitor) -> String {
    // 1. 优先使用序列号（硬件属性，最可靠）
    if let Ok(serial) = monitor.serial_number() {
        if !serial.is_empty() {
            return format!("SERIAL:{}", serial);
        }
    }

    // 2. 备用：使用 UUID
    if let Ok(uuid) = monitor.uuid() {
        return format!("UUID:{}", uuid);
    }

    // 3. 最后：使用显示器 ID（临时方案）
    format!("ID:{}", monitor.id().unwrap_or(0))
}
```

### 📋 决策树

```
需要标识显示器？
├─ 序列号可用？
│  ├─ ✅ 是 → 使用序列号（最可靠）
│  └─ ❌ 否 → 继续
│
├─ UUID 可用？
│  ├─ ✅ 是 → 使用 UUID（可靠，但注意平台差异）
│  └─ ❌ 否 → 继续
│
└─ 使用显示器 ID（仅限当前会话）
```

---

## 📊 总结

| 特性 | 序列号 | UUID (Windows/Linux) | UUID (macOS) |
|------|--------|---------------------|--------------|
| **硬件属性** | ✅ 是 | ❌ 否（基于 EDID） | ❌ 否（系统生成） |
| **跨系统持久** | ✅ 是 | ✅ 是 | ⚠️ 可能变化 |
| **跨电脑一致** | ✅ 是 | ✅ 是 | ⚠️ 可能不同 |
| **可靠性** | ⭐⭐⭐⭐⭐ | ⭐⭐⭐⭐ | ⭐⭐⭐ |
| **推荐度** | ⭐⭐⭐⭐⭐ | ⭐⭐⭐⭐ | ⭐⭐⭐ |

---

## 💡 实践建议

1. **存储显示器配置**: 使用序列号作为主键
2. **用户偏好设置**: 使用序列号关联
3. **显示器管理**: 使用序列号作为唯一标识
4. **兼容性处理**: 序列号不可用时使用 UUID
5. **跨平台应用**: 优先使用序列号，避免平台差异

---

**结论**:
- 🥇 **首选**: 序列号（硬件属性，最可靠）
- 🥈 **备选**: UUID（Windows/Linux 很可靠，macOS 需要注意）
- 🥉 **临时**: 显示器 ID（仅当前会话）

---

**最后更新**: 2025-11-01

