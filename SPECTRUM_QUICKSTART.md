# 频谱可视化快速开始

## 🚀 快速启动

### 1. 安装 Python 依赖

```bash
pip install -r requirements.txt
```

### 2. 启动 Rust 后端（频谱服务器）

```bash
cargo run --example spectrum_server
```

你应该看到：
```
=== Pluto SDR 频谱服务器 ===

启动 TCP 服务器 localhost:5555...
等待 Python 客户端连接...
运行: python app.py --mode spectrum

连接到 Pluto SDR...
✓ Pluto SDR 已连接

开始处理数据...
```

### 3. 启动 Python 前端（可视化）

在**另一个终端**运行：

```bash
# 功率谱模式
python app.py --mode spectrum

# 或瀑布图模式
python app.py --mode waterfall
```

## 📊 可视化模式

### 功率谱模式（推荐入门）

```bash
python app.py --mode spectrum
```

显示实时功率谱：
- X 轴：频率（MHz）
- Y 轴：功率（dB）
- 更新率：~30 FPS

**适合**：查看当前频段的信号强度

### 瀑布图模式

```bash
python app.py --mode waterfall
```

显示频谱随时间变化：
- X 轴：频率
- Y 轴：时间（新数据在顶部）
- 颜色：功率强度
- 更新率：~10 FPS

**适合**：观察信号随时间的变化，检测间歇性信号

## 🎛️ 修改参数

### 更改中心频率

编辑 [examples/spectrum_server.rs](examples/spectrum_server.rs)：

```rust
let rx_config = LibiioRxConfig {
    center_freq_hz: 433_000_000,  // 改为 433 MHz
    // ...
};
```

### 更改 FFT 大小

```rust
let spectrum_config = SpectrumConfig {
    fft_size: 2048,  // 更高的频率分辨率
    // ...
};
```

**权衡**：
- 小 FFT (512-1024)：快速更新，低频率分辨率
- 大 FFT (2048-4096)：慢更新，高频率分辨率

### 更改增益

```rust
let rx_config = LibiioRxConfig {
    rx_gain_db: 60.0,  // 更高增益可以看到更弱的信号
    // ...
};
```

## 📈 示例场景

### 场景 1: 监测 ISM 频段（915 MHz）

```rust
center_freq_hz: 915_000_000,
sample_rate_hz: 2_084_000,
rx_gain_db: 50.0,
```

```bash
cargo run --example spectrum_server
python app.py --mode waterfall
```

### 场景 2: 扫描 433 MHz 遥控器

```rust
center_freq_hz: 433_920_000,
sample_rate_hz: 2_000_000,
rx_gain_db: 55.0,
```

### 场景 3: FM 广播（88-108 MHz）

```rust
center_freq_hz: 98_000_000,
sample_rate_hz: 2_400_000,
rx_gain_db: 60.0,
```

## 🔧 故障排查

### Python 端连接失败

**症状**：`Connection refused`

**解决**：
1. 确保 Rust 后端先启动
2. 检查端口是否被占用：`lsof -i :5555`

### 画面卡顿

**原因**：FFT 计算太慢（简化版 DFT）

**解决**：
1. 减小 FFT 大小
2. 降低更新率
3. **推荐**：添加 `rustfft` 依赖（性能提升 100x）

在 `Cargo.toml` 中：
```toml
rustfft = "6.2"
```

### 信号太弱看不到

**解决**：
1. 增加增益：`rx_gain_db: 70.0`
2. 检查天线连接
3. 靠近信号源

### Python 画面不更新

**检查**：
- Rust 端是否在打印 "缓冲区 N" 消息？
- Python 端是否显示错误？

## ⚡ 性能优化路线图

### 当前性能（演示版）

- FFT：简化 DFT，**非常慢**
- 数据传输：JSON over TCP
- 更新率：~5-10 FPS
- 延迟：~100-200ms

### 第一步优化：添加 rustfft

```toml
[dependencies]
rustfft = "6.2"
```

**效果**：
- FFT 速度：100x 提升
- 更新率：30-60 FPS
- 延迟：~30-50ms

### 第二步优化：使用 ZMQ + MessagePack

```toml
zmq = "0.10"
rmp-serde = "1.3"
```

```bash
pip install pyzmq msgpack
```

**效果**：
- 数据传输：10x 提升
- 延迟：<20ms
- CPU 占用：减少 50%

### 第三步优化：使用 PyQtGraph

```bash
pip install pyqtgraph PyQt5
```

**效果**：
- 渲染速度：10x 提升
- 更新率：60+ FPS
- 更流畅的动画

## 📚 下一步

1. **实现录制功能**：保存频谱数据到文件
2. **添加峰值检测**：自动标注信号峰值
3. **频谱扫描**：自动扫描频段
4. **Web 界面**：使用 Flask + Chart.js

参考 [SPECTRUM_VISUALIZATION.md](SPECTRUM_VISUALIZATION.md) 了解更多细节。

## 🎯 关键性能指标

当前目标（演示版）：
- ✅ 功能完整
- ⚠️ 性能一般（~10 FPS）

优化后目标：
- ✅ 30+ FPS 更新率
- ✅ <30ms 延迟
- ✅ <10% CPU 占用
