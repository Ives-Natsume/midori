# 更新日志

## v2.0.0 - 重构为纯 libiio-sys 实现 (2026-02-03)

### 🚀 主要变更

#### 架构升级
- ✅ **完全采用 libiio-sys**：移除了 `industrial-io` 依赖，避免 USB 接口冲突
- ✅ **新的接收器实现**：`LibiioReceiver` 替代了原有的 `PlutoReceiver`
- ✅ **简化的 API**：更直接、更高效的接口设计

#### 解决的问题
- ✅ 修复了 USB 接口冲突问题 ("Unable to claim interface 0:8:5")
- ✅ 修复了通道未启用的问题
- ✅ 提升了数据接收的稳定性

#### 模块变更
**新增**：
- `src/io/libiio_receiver.rs` - 纯 libiio-sys 接收器实现
- `examples/simple_rx.rs` - 简化的接收器示例
- `USB_CONFLICT_SOLVED.md` - USB 冲突问题解决文档

**移除**：
- `src/io/receiver.rs` - 旧的混合实现（有 USB 冲突）
- `src/io/pluto.rs` - 自动连接函数（功能集成到 LibiioReceiver）
- `src/io/lowlevel.rs` - 底层参考实现
- `examples/real_rx_test.rs` - 使用旧 API
- `examples/signal_monitor.rs` - 使用旧 API
- `examples/fm_receiver.rs` - 使用旧 API
- `examples/spectrum_scan.rs` - 使用旧 API
- `examples/debug_context.rs` - 调试工具（已完成任务）
- `examples/pure_libiio.rs` - 功能被 simple_rx 替代

**保留**：
- `examples/config_test.rs` - 配置测试（使用 industrial-io，仅配置无冲突）

### 📝 API 变更

#### 旧 API (v1.x)
```rust
use pluto_rs::io::pluto::pluto_connect;
use pluto_rs::io::receiver::{PlutoReceiver, RxConfig, GainMode};

let ctx = pluto_connect()?;
let config = RxConfig { ... };
let mut receiver = PlutoReceiver::new(ctx, config);
receiver.configure()?;
```

#### 新 API (v2.0)
```rust
use pluto_rs::io::libiio_receiver::{LibiioReceiver, LibiioRxConfig};

let config = LibiioRxConfig { ... };
let mut receiver = LibiioReceiver::new(config);
receiver.connect()?;
```

### 🎯 升级指南

1. **更新导入**：
   - `use pluto_rs::io::receiver` → `use pluto_rs::io::libiio_receiver`
   - `PlutoReceiver` → `LibiioReceiver`
   - `RxConfig` → `LibiioRxConfig`
   - `GainMode::Manual` → `"manual".to_string()`

2. **更新初始化**：
   - 移除 `pluto_connect()` 调用
   - `new()` + `configure()` → `new()` + `connect()`

3. **增益模式**：
   - `GainMode::Manual` → `"manual"`
   - `GainMode::FastAttack` → `"fast_attack"`
   - `GainMode::SlowAttack` → `"slow_attack"`
   - `GainMode::Hybrid` → `"hybrid"`

### 📊 项目结构

```
pluto_rs/
├── src/
│   ├── lib.rs
│   ├── main.rs              # 使用新 API
│   └── io/
│       ├── mod.rs
│       └── libiio_receiver.rs  # 核心实现
├── examples/
│   ├── simple_rx.rs         # ⭐ 推荐示例
│   └── config_test.rs       # 配置测试
├── Cargo.toml
├── README.md                # 更新后的文档
└── USB_CONFLICT_SOLVED.md   # 技术说明
```

### 🔧 依赖项

```toml
[dependencies]
libiio-sys = "0.4.1"
num-complex = "0.4.6"
anyhow = "1.0"
# 移除了 industrial-io
```

### ✅ 测试

所有功能均已测试并验证：
- ✅ USB 连接
- ✅ PHY 配置
- ✅ IQ 数据接收
- ✅ 动态频率调整
- ✅ 动态增益调整
- ✅ 信号处理工具

### 📚 文档

- 更新了 README.md，反映新的 API
- 新增 USB_CONFLICT_SOLVED.md 解释技术细节
- 代码注释更加详细

---

## v1.x - 初始实现（已废弃）

混合使用 industrial-io 和 libiio-sys 的实现，存在 USB 接口冲突问题。
