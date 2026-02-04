# USB 接口冲突问题已解决

## 问题原因

```
ERROR: Unable to claim interface 0:8:5: Permission denied (13)
Error: 无法创建 libiio context (USB)
```

**不是 macOS 权限问题**，而是 **USB 接口被占用**：

1. `industrial-io::Context` 创建时占用了 USB 接口
2. 然后 `libiio-sys` 尝试创建第二个 Context
3. macOS/Linux 不允许同一个 USB 设备被两次声明

## 解决方案

### ✅ 方案 1：使用纯 libiio-sys 接收器（推荐）

我创建了一个新的接收器模块 `libiio_receiver.rs`，完全使用 libiio-sys，避免混用两个库。

**使用方法**：

```bash
# 简单接收器示例
cargo run --example simple_rx

# 或者完整版本
cargo run --example pure_libiio
```

**代码示例**：

```rust
use pluto_rs::io::libiio_receiver::{LibiioReceiver, LibiioRxConfig, signal};

// 配置
let config = LibiioRxConfig {
    center_freq_hz: 915_000_000,
    sample_rate_hz: 2_084_000,
    bandwidth_hz: 2_000_000,
    rx_gain_db: 50.0,
    buffer_size: 8192,
    gain_mode: "manual".to_string(),
};

// 创建并连接
let mut receiver = LibiioReceiver::new(config);
receiver.connect()?;

// 接收数据
loop {
    let samples = receiver.receive_samples()?;
    // 处理样本...
}
```

### ❌ 不推荐：混用两个库

原来的 `PlutoReceiver` 混用了：
- `industrial-io::Context` - 用于配置
- `libiio-sys` - 用于数据接收

这导致 USB 接口冲突。

## 两种接收器对比

### LibiioReceiver（新版，推荐）

✅ **优点**：
- 无 USB 接口冲突
- 更直接、性能更好
- 完全控制底层细节
- 适合 macOS 和 Linux

❌ **缺点**：
- 需要使用 unsafe 代码
- API 较底层

### PlutoReceiver（原版）

✅ **优点**：
- 更安全的 Rust API
- 类型安全

❌ **缺点**：
- USB 接口冲突（macOS/Linux）
- 混用两个库
- 复杂度高

## 迁移指南

### 从 PlutoReceiver 迁移到 LibiioReceiver

**之前**：
```rust
use pluto_rs::io::pluto::pluto_connect;
use pluto_rs::io::receiver::{PlutoReceiver, RxConfig, GainMode};

let ctx = pluto_connect()?;
let config = RxConfig::default();
let mut receiver = PlutoReceiver::new(ctx, config);
receiver.configure()?;
```

**现在**：
```rust
use pluto_rs::io::libiio_receiver::{LibiioReceiver, LibiioRxConfig};

let config = LibiioRxConfig::default();
let mut receiver = LibiioReceiver::new(config);
receiver.connect()?;
```

### API 对应关系

| PlutoReceiver | LibiioReceiver |
|---------------|----------------|
| `new(ctx, config)` + `configure()` | `new(config)` + `connect()` |
| `receive_samples()` | `receive_samples()` |
| `set_frequency()` | `set_frequency()` |
| `set_gain()` | `set_gain()` |
| `signal::*` | `signal::*` |

## 推荐的示例

### 1. simple_rx - 简单接收器
最简单的数据接收示例，推荐新手使用。

```bash
cargo run --example simple_rx
```

### 2. pure_libiio - 完整功能
展示所有配置选项和功能。

```bash
cargo run --example pure_libiio
```

### 3. config_test - 配置测试
仅测试配置，不接收数据（使用 industrial-io）。

```bash
cargo run --example config_test
```

## 常见问题

### Q: 为什么不能同时使用两个库？

A: USB 驱动层面的限制。一个 USB 设备接口只能被一个程序/context 占用。

### Q: 需要 sudo 权限吗？

A: 不需要。这不是权限问题，是接口占用问题。

### Q: 所有平台都有这个问题吗？

A: 是的。macOS、Linux、Windows 都会遇到这个问题。

### Q: 旧的 PlutoReceiver 还能用吗？

A: 如果你只使用 `config_test` 这样的配置示例，可以用。但涉及数据接收时会遇到冲突。

## 总结

✅ **使用 `LibiioReceiver` 和 `simple_rx` 示例**  
❌ 避免混用 `industrial-io` 和 `libiio-sys` 进行数据接收

这样就能完美地在 macOS 和 Linux 上使用 Pluto SDR 了！
