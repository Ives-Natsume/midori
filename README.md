# Pluto SDR 接收器 (Rust)

使用 Rust 和 libiio-sys 控制 Pluto SDR 进行信号接收的完整框架。

## ✅ 功能特性

- 🎯 自动检测和连接 Pluto SDR (USB/网络)
- 📡 完整的接收参数配置
- 📊 真实的 IQ 样本数据接收
- 🔧 信号处理工具集
- 🎚️ 动态频率和增益调整
- 🎮 多种增益控制模式
- 🧹 自动资源清理
- ⚡ 纯 libiio-sys 实现，无 USB 冲突
- 📈 **实时频谱可视化**
- 🎛️ **运行时动态频率控制**（NEW! - 适合卫星跟踪）

## 🚀 快速开始

### 基础数据接收

```bash
# 运行主程序
cargo run

# 或运行示例
cargo run --example simple_rx
```

### 📊 频谱可视化

**基础版（固定频率）：**
```bash
# 1. 启动服务器
cargo run --example spectrum_server

# 2. 启动可视化
python app.py --mode spectrum
```

**交互版（动态频率控制）⭐：**
```bash
# 1. 启动交互式服务器
cargo run --example spectrum_interactive

# 2. 启动交互式可视化（带控制面板）
python app_interactive.py --mode spectrum

# 或直接在服务器终端输入命令：
# f 145.8  (设置频率为 145.8 MHz)
# g 60     (设置增益为 60 dB)
```

详见：
- 基础使用：[SPECTRUM_QUICKSTART.md](SPECTRUM_QUICKSTART.md)
- 动态控制：[DYNAMIC_CONTROL.md](DYNAMIC_CONTROL.md) 🛰️

## 使用示例

```rust
use pluto_rs::io::libiio_receiver::{LibiioReceiver, LibiioRxConfig, signal};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 配置接收器
    let config = LibiioRxConfig {
        center_freq_hz: 915_000_000,      // 915 MHz
        sample_rate_hz: 2_084_000,        // 2.084 MSPS
        bandwidth_hz: 2_000_000,          // 2 MHz
        rx_gain_db: 50.0,                 // 50 dB
        buffer_size: 16384,               // 16K 样本
        gain_mode: "manual".to_string(),  // 手动增益
    };
    
    // 创建并连接
    let mut receiver = LibiioReceiver::new(config);
    receiver.connect()?;
    
    // 接收数据
    loop {
        let samples = receiver.receive_samples()?;
        
        // 信号处理
        let power_db = signal::calculate_power_db(&samples);
        println!("信号功率: {:.2} dB", power_db);
    }
    
    Ok(())
}
```

## 核心 API

### `LibiioReceiver` - 接收器结构

**方法**：
- `new(config)` - 创建接收器实例
- `connect()` - 连接到 Pluto SDR 并配置
- `receive_samples()` - 接收 IQ 样本
- `set_frequency(hz)` - 更新中心频率
- `set_gain(db)` - 更新接收增益

### `LibiioRxConfig` - 配置结构

```rust
pub struct LibiioRxConfig {
    pub center_freq_hz: u64,       // 中心频率 (Hz)
    pub sample_rate_hz: u64,       // 采样率 (Hz)
    pub bandwidth_hz: u64,         // RF 带宽 (Hz)
    pub rx_gain_db: f64,           // 接收增益 (dB)
    pub buffer_size: usize,        // 缓冲区大小（样本数）
    pub gain_mode: String,         // 增益模式
}
```

### 增益模式

- `"manual"` - 手动增益控制
- `"fast_attack"` - 快速 AGC
- `"slow_attack"` - 慢速 AGC
- `"hybrid"` - 混合模式

## Pluto SDR 参数范围

### 频率范围
- **接收**: 70 MHz - 6 GHz

### 采样率
- **最小**: 521 kSPS (0.521 MSPS)
- **最大**: 61.44 MSPS
- **推荐**: 2.084 MSPS

### 增益范围
- **RX 增益**: -3 dB 到 71 dB (1 dB 步进)

### RF 带宽
- **最小**: 200 kHz
- **最大**: 56 MHz

## 信号处理工具

### 功率计算

```rust
use pluto_rs::io::libiio_receiver::signal;

let power_linear = signal::calculate_power(&samples);
let power_db = signal::calculate_power_db(&samples);
```

### 幅度和相位提取

```rust
let magnitudes = signal::calculate_magnitude(&samples);
let phases = signal::calculate_phase(&samples);
```

## 依赖项

```toml
[dependencies]
libiio-sys = "0.4.1"
num-complex = "0.4.6"
anyhow = "1.0"
```

## 项目结构

```
pluto_rs/
├── src/
│   ├── lib.rs              # 库入口
│   ├── main.rs             # 主程序
│   └── io/
│       ├── mod.rs          # IO 模块
│       └── libiio_receiver.rs  # 接收器实现
├── examples/
│   ├── simple_rx.rs        # 简单接收器示例
│   └── config_test.rs      # 配置测试
└── Cargo.toml
```

## 常见应用场景

1. **频谱监测** - 扫描频段寻找信号
2. **信号分析** - 分析特定频率的信号特征
3. **FM/AM 接收** - 接收和解调广播信号
4. **ISM 频段监控** - 监测 433/868/915 MHz 设备
5. **卫星信号接收** - 接收卫星下行信号

## 故障排查

### 找不到设备
- 确保 Pluto SDR 已连接（USB 或网络）
- 检查 USB 驱动是否正确安装
- 在 macOS 上，可能需要给予终端完全磁盘访问权限

### 数据接收错误
- 检查缓冲区大小是否合适（推荐 8192-16384）
- 确保采样率在支持范围内
- 验证通道是否正确启用

### 信号功率异常
- 调整增益设置
- 确认天线连接
- 检查频率是否正确

## 技术说明

本项目使用纯 libiio-sys 实现，直接调用 C API，避免了混用多个库导致的 USB 接口冲突问题。详见 [USB_CONFLICT_SOLVED.md](USB_CONFLICT_SOLVED.md)。

## 许可证

MIT

## 参考资料

- [Pluto SDR 官方文档](https://wiki.analog.com/university/tools/pluto)
- [libiio 文档](https://analogdevicesinc.github.io/libiio/)
- [AD9361 数据手册](https://www.analog.com/media/en/technical-documentation/data-sheets/AD9361.pdf)
