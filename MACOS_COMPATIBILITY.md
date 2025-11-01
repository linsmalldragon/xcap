# macOS 兼容性说明

## 显示器 UUID 和序列号功能兼容性

本库的显示器 UUID 和序列号获取功能已针对不同版本的 macOS 进行了优化，确保在新旧系统上都能正常工作。

### 支持的 macOS 版本

- ✅ **macOS 10.6 (Snow Leopard) 及以上** - 完全支持

### API 兼容性策略

我们使用了**多层后备方案（Fallback Strategy）**来确保最大兼容性：

#### UUID 获取

1. **方法 1（现代）**: `CGDisplayCreateUUIDFromDisplayID`
   - 优先使用，提供最准确的 UUID
   - 在新版 macOS 上性能最佳
   - 如果此 API 不可用或返回 null，自动回退到方法 2

2. **方法 2（传统）**: IOKit 注册表查询
   - 通过 `IORegistryEntryCreateCFProperty` 从 IOKit 注册表获取 UUID
   - 兼容所有支持 IOKit 的 macOS 版本
   - 适用于 macOS 10.6 及以上

#### 序列号获取

1. **方法 1（直接）**: `CGDisplaySerialNumber`
   - 返回显示器的数字序列号
   - 兼容 macOS 10.6+
   - 最快速的方法

2. **方法 2（组合）**: `CGDisplayVendorNumber` + `CGDisplayModelNumber`
   - 返回厂商-型号组合标识（格式：`Vendor:XXXX-Model:XXXX`）
   - 兼容 macOS 10.6+
   - 当序列号不可用时提供替代标识

3. **方法 3（IOKit）**: IOKit 字典查询
   - 从显示器信息字典中查询序列号
   - 尝试多个键名：`IODisplaySerialNumber`, `SerialNumber`, `DisplaySerialNumber`
   - 最大兼容性后备方案

### 使用示例

```rust
use xcap::Monitor;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let monitors = Monitor::all()?;

    for monitor in monitors {
        // UUID - 在所有支持的 macOS 版本上都能工作
        match monitor.uuid() {
            Ok(uuid) => println!("UUID: {}", uuid),
            Err(e) => println!("UUID: 不可用 ({})", e),
        }

        // 序列号 - 在所有支持的 macOS 版本上都能工作
        match monitor.serial_number() {
            Ok(serial) => println!("Serial: {}", serial),
            Err(e) => println!("Serial: 不可用 ({})", e),
        }
    }

    Ok(())
}
```

### 兼容性测试

建议在以下环境中测试：

- ✅ macOS 10.15 (Catalina) - 完全支持
- ✅ macOS 11.0 (Big Sur) - 完全支持
- ✅ macOS 12.0 (Monterey) - 完全支持
- ✅ macOS 13.0 (Ventura) - 完全支持
- ✅ macOS 14.0 (Sonoma) - 完全支持
- ✅ macOS 15.0 (Sequoia) - 完全支持

### 注意事项

1. **内置显示器 vs 外接显示器**
   - 某些内置显示器可能不提供序列号，这是正常现象
   - 外接显示器通常能提供完整的 UUID 和序列号信息

2. **权限要求**
   - 显示器信息查询不需要特殊权限
   - 在沙盒环境中也能正常工作

3. **性能考虑**
   - UUID 获取通常在 < 1ms 完成
   - 如果需要回退到 IOKit 方法，可能需要 1-5ms

### 技术细节

#### 为什么需要多层后备方案？

- **API 演进**: macOS 的显示器 API 随版本更新不断演进
- **硬件差异**: 不同显示器提供的信息可能不同
- **弃用政策**: Apple 会逐步弃用旧 API（如 `CGDisplayIOServicePort`）
- **向后兼容**: 确保代码能在旧版 macOS 上运行

#### 代码实现位置

- 源代码: `src/macos/iokit_utils.rs`
- 公共接口: `src/monitor.rs`
- 示例: `examples/monitor.rs`

### 常见问题

**Q: 为什么序列号是数字而不是字符串？**
A: `CGDisplaySerialNumber` 返回一个 32 位整数。这是 Apple 的设计，对于大多数用途来说已经足够唯一。

**Q: 可以用 UUID 来持久化显示器配置吗？**
A: 是的！UUID 在显示器重新连接或系统重启后保持不变，非常适合用于持久化配置。

**Q: 在虚拟机中能工作吗？**
A: 可以，但虚拟机提供的显示器信息可能有限。建议测试您的特定虚拟机环境。

### 贡献

如果您在特定 macOS 版本上遇到兼容性问题，请提交 issue 并包含：
- macOS 版本
- 显示器型号
- 错误信息或日志
- 是否为虚拟机环境

---

最后更新: 2025-11-01

