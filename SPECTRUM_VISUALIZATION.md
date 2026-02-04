# 频谱可视化方案设计

## 🎯 推荐架构：Rust + Python 混合方案

### 为什么选择这个方案？

1. **性能关键路径在 Rust**：
   - IQ 数据采集：~2 MSPS
   - FFT 计算：每秒可能需要处理数千个 FFT
   - 数据预处理：平均、归一化

2. **可视化在 Python**：
   - 成熟的库：matplotlib、PyQtGraph、plotly
   - 实时更新简单
   - 易于调整和优化显示

## 📊 性能优化要点

### 1. FFT 计算优化

```rust
// ❌ 错误：每次创建新的 FFT 规划器
fn process_samples(samples: &[Complex<f32>]) {
    let mut planner = FftPlanner::new();  // 慢！
    let fft = planner.plan_fft_forward(samples.len());
    // ...
}

// ✅ 正确：复用 FFT 规划器
struct SpectrumAnalyzer {
    fft: Arc<dyn Fft<f32>>,  // 预先创建，复用
    buffer: Vec<Complex<f32>>,
    window: Vec<f32>,  // 预计算的窗函数
}
```

**性能提升**：10-100x

### 2. 窗函数预计算

```rust
// 汉宁窗、布莱克曼窗等
fn precompute_window(size: usize) -> Vec<f32> {
    (0..size)
        .map(|i| {
            0.5 - 0.5 * (2.0 * PI * i as f32 / (size - 1) as f32).cos()
        })
        .collect()
}
```

### 3. 数据降采样

```rust
// 不需要发送所有 FFT 点
// 2048 FFT → 发送 512 点用于显示
fn downsample_spectrum(spectrum: &[f32], target_size: usize) -> Vec<f32> {
    let step = spectrum.len() / target_size;
    spectrum.chunks(step)
        .map(|chunk| chunk.iter().sum::<f32>() / chunk.len() as f32)
        .collect()
}
```

**带宽节省**：75%

### 4. 批量发送

```rust
// ❌ 每个 FFT 都发送 → 网络开销大
// ✅ 累积 10-20 个 FFT 后批量发送
struct DataBuffer {
    spectrums: Vec<Vec<f32>>,
    batch_size: usize,
}
```

### 5. 数据格式优化

```rust
// ❌ JSON：灵活但慢
// ✅ MessagePack 或二进制：快 10x
use rmp_serde;  // MessagePack
```

## 🔧 推荐的技术栈

### Rust 端

```toml
[dependencies]
rustfft = "6.2"          # FFT 计算
zmq = "0.10"             # 高性能消息队列
serde = "1.0"            # 序列化
serde_json = "1.0"       # JSON（开发阶段）
rmp-serde = "1.3"        # MessagePack（生产）
```

### Python 端

```python
# 实时可视化
pyqtgraph  # 最快的实时绘图库
# 或
matplotlib + blitting  # 经典选择

# 数据接收
pyzmq  # 对应 Rust 的 zmq
msgpack  # 对应 rmp-serde
```

## 📈 三种可视化模式

### 1. 实时功率谱（基础）

```
频率 (MHz)
   │      ╱╲
功率│     ╱  ╲    
(dB)│    ╱    ╲___
   │___╱          ╲___
   └────────────────────→
```

**更新率**：10-30 FPS
**FFT 大小**：1024-4096

### 2. 瀑布图（推荐）⭐

```
   新 ┌───────────────┐
   ↓  │▓▒░░░░░▒▓█▓▒░░│
时 间 │▓▒░░░░░▒▓█▓▒░░│
   ↓  │▓▒░░░░░▒▓█▓▒░░│
   旧 └───────────────┘
        频率 →
```

**更新率**：5-15 FPS
**历史深度**：100-500 行

### 3. 3D 频谱（高级）

```
   ╱│╲
  ╱ │ ╲  功率
 ╱  │  ╲
╱───┴───╲
 频率  时间
```

**更新率**：1-5 FPS
**计算密集**

## 🎨 数据流设计

### 方案 A：ZMQ PUB-SUB（推荐）

**特点**：
- 高性能、低延迟
- 支持多个订阅者
- 自动处理背压

```rust
// Rust (Publisher)
let context = zmq::Context::new();
let publisher = context.socket(zmq::PUB)?;
publisher.bind("tcp://*:5555")?;
publisher.send(&spectrum_data, 0)?;
```

```python
# Python (Subscriber)
context = zmq.Context()
subscriber = context.socket(zmq.SUB)
subscriber.connect("tcp://localhost:5555")
subscriber.setsockopt_string(zmq.SUBSCRIBE, "")
data = subscriber.recv()
```

### 方案 B：共享内存（最快）

**特点**：
- 零拷贝
- 延迟 <1ms
- 仅限本机

```rust
use shared_memory::*;
let shmem = ShmemConf::new().size(4096).create()?;
```

### 方案 C：TCP Socket（简单）

**特点**：
- 无需额外依赖
- 延迟较高
- 适合原型开发

## ⚡ 性能目标

| 指标 | 目标 | 优化后 |
|------|------|--------|
| FFT 计算 | <5ms | <1ms |
| 数据传输 | <10ms | <2ms |
| Python 渲染 | <50ms | <20ms |
| 总延迟 | <100ms | <30ms |
| 刷新率 | >10 FPS | >30 FPS |

## 🔍 内存优化

### 循环缓冲区

```rust
struct CircularBuffer<T> {
    buffer: Vec<T>,
    write_pos: usize,
    capacity: usize,
}

// 避免频繁分配
impl<T: Clone> CircularBuffer<T> {
    fn push(&mut self, item: T) {
        self.buffer[self.write_pos] = item;
        self.write_pos = (self.write_pos + 1) % self.capacity;
    }
}
```

### 对象池

```rust
// 复用 Vec 而不是每次分配新的
struct VecPool {
    pool: Vec<Vec<f32>>,
}

impl VecPool {
    fn acquire(&mut self) -> Vec<f32> {
        self.pool.pop().unwrap_or_default()
    }
    
    fn release(&mut self, mut vec: Vec<f32>) {
        vec.clear();
        self.pool.push(vec);
    }
}
```

## 🎛️ 可调参数

```rust
pub struct SpectrumConfig {
    pub fft_size: usize,           // 1024, 2048, 4096
    pub overlap: f32,              // 0.5 = 50% 重叠
    pub averaging: usize,          // 移动平均窗口
    pub update_rate_hz: f32,       // 显示刷新率
    pub display_points: usize,     // 实际显示的点数
}
```

**权衡**：
- 大 FFT：频率分辨率高 ↔ 时间分辨率低
- 高重叠：平滑 ↔ 计算量大
- 高刷新率：流畅 ↔ CPU 占用高

## 📝 实现优先级

### Phase 1: 基础功能（1-2天）
- [x] Rust FFT 计算
- [x] TCP 数据发送
- [x] Python 基础接收
- [x] matplotlib 静态显示

### Phase 2: 实时优化（2-3天）
- [ ] ZMQ 替代 TCP
- [ ] PyQtGraph 实时显示
- [ ] 窗函数和降采样
- [ ] 可调参数

### Phase 3: 高级功能（3-5天）
- [ ] 瀑布图
- [ ] 峰值检测和标注
- [ ] 录制和回放
- [ ] Web 界面

## 💡 关键代码示例

### Rust：高效 FFT 处理器

```rust
use rustfft::{FftPlanner, num_complex::Complex};
use std::sync::Arc;

pub struct SpectrumProcessor {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    input_buffer: Vec<Complex<f32>>,
    output_buffer: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
}

impl SpectrumProcessor {
    pub fn new(fft_size: usize) -> Self {
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(fft_size);
        
        Self {
            fft,
            window: hann_window(fft_size),
            input_buffer: vec![Complex::new(0.0, 0.0); fft_size],
            output_buffer: vec![Complex::new(0.0, 0.0); fft_size],
            scratch: vec![Complex::new(0.0, 0.0); fft_size],
        }
    }
    
    pub fn process(&mut self, samples: &[Complex<f32>]) -> Vec<f32> {
        // 应用窗函数
        for (i, sample) in samples.iter().enumerate() {
            self.input_buffer[i] = sample * self.window[i];
        }
        
        // FFT
        self.output_buffer.copy_from_slice(&self.input_buffer);
        self.fft.process_with_scratch(
            &mut self.output_buffer,
            &mut self.scratch
        );
        
        // 计算功率谱
        self.output_buffer
            .iter()
            .map(|c| 20.0 * (c.norm() + 1e-10).log10())
            .collect()
    }
}
```

### Python：高性能实时绘图

```python
import pyqtgraph as pg
import zmq
import numpy as np

class SpectrumViewer:
    def __init__(self):
        self.app = pg.mkQApp()
        self.win = pg.GraphicsLayoutWidget()
        self.plot = self.win.addPlot()
        self.curve = self.plot.plot()
        
        # ZMQ 接收器
        context = zmq.Context()
        self.socket = context.socket(zmq.SUB)
        self.socket.connect("tcp://localhost:5555")
        self.socket.setsockopt_string(zmq.SUBSCRIBE, "")
        
        # 定时器
        self.timer = pg.QtCore.QTimer()
        self.timer.timeout.connect(self.update)
        self.timer.start(33)  # 30 FPS
    
    def update(self):
        try:
            data = self.socket.recv(zmq.NOBLOCK)
            spectrum = np.frombuffer(data, dtype=np.float32)
            self.curve.setData(spectrum)
        except zmq.Again:
            pass
    
    def run(self):
        self.win.show()
        self.app.exec_()
```

## 🎯 最终建议

对于你的项目，我建议：

1. **开始用 TCP + JSON**（原型阶段）
   - 快速验证功能
   - 易于调试

2. **升级到 ZMQ + 二进制**（优化阶段）
   - 提升性能
   - 降低延迟

3. **PyQtGraph 而不是 matplotlib**
   - 实时性能好 10x
   - 专为实时数据设计

4. **保持 Rust 做重活**
   - FFT、滤波、检测
   - Python 只负责显示
