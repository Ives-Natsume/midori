# 动态频率控制使用指南

## 🎯 功能概述

新增的交互式频谱服务器支持**运行时动态调整频率和增益**，非常适合：
- 🛰️ 卫星跟踪（多普勒补偿）
- 📡 频谱扫描
- 🔍 信号搜索
- 📻 动态监听

## 🚀 快速开始

### 方法 1: 终端直接控制（最简单）

启动交互式服务器：

```bash
cargo run --example spectrum_interactive
```

服务器启动后，直接在终端输入命令：

```
交互命令：
  f <MHz>  - 设置中心频率，例如：f 145.8
  g <dB>   - 设置增益，例如：g 60
  s        - 显示当前状态
  h        - 显示帮助
  q        - 退出程序
```

**示例操作**：
```
f 144.0       # 设置到 144 MHz
✓ 频率设置为 144.000 MHz

f 145.8       # 切换到 145.8 MHz
✓ 频率设置为 145.800 MHz

g 65          # 增加增益
✓ 增益设置为 65.0 dB

s             # 查看状态
状态:
  中心频率: 145.800 MHz
  采样率: 2.084 MSPS
  增益: 65.0 dB
```

### 方法 2: Python GUI 控制（推荐）

**1. 启动 Rust 后端：**
```bash
cargo run --example spectrum_interactive
```

**2. 启动 Python 可视化（带控制面板）：**
```bash
python app_interactive.py --mode spectrum
```

**控制方式**：
- 📝 **输入框**：直接输入频率/增益，点击按钮应用
- ⌨️ **键盘快捷键**：
  - `↑` / `↓` - 微调频率 (±10 kHz)
  - `PageUp` / `PageDown` - 大幅调整 (±100 kHz)
  - `Enter` - 应用当前输入的频率

**适合场景**：实时可视化 + 快速调整

### 方法 3: 命令行工具（脚本自动化）

使用独立的控制工具：

```bash
# 设置频率
python control.py -f 145.8

# 设置增益
python control.py -g 60

# 查询状态
python control.py -s

# 远程控制（如果服务器在其他机器上）
python control.py --host 192.168.1.100 -f 433.0
```

**适合场景**：
- 自动化脚本
- 批量调整
- 远程控制
- 卫星跟踪软件集成

## 🛰️ 实际应用案例

### 案例 1: 国际空间站 (ISS) 信号接收

ISS APRS 频率：145.825 MHz，但因多普勒频移可能在 145.820-145.830 MHz 范围内变化。

**方法 A - 手动跟踪**：

启动服务器和可视化：
```bash
cargo run --example spectrum_interactive
python app_interactive.py
```

卫星过境时，观察频谱峰值位置，使用键盘快捷键快速调整：
- 看到信号偏左 → 按 `↓` 降低频率
- 看到信号偏右 → 按 `↑` 提高频率

**方法 B - 自动化脚本**：

```python
# doppler_track.py
import time
from control import send_command

# 假设有卫星轨道预测数据
frequencies = [145.820, 145.822, 145.825, 145.828, 145.830]

for freq in frequencies:
    send_command('localhost', 5556, f'f {freq}')
    time.sleep(10)  # 每 10 秒调整一次
```

### 案例 2: 频谱扫描

扫描 144-146 MHz 业余频段：

```bash
# scan_band.sh
for freq in $(seq 144 0.1 146); do
    python control.py -f $freq
    sleep 0.5
done
```

### 案例 3: 动态监听多个频率

监听多个遥控器频率：

```python
# multi_channel_monitor.py
channels = [433.05, 433.92, 434.42]

while True:
    for freq in channels:
        send_command('localhost', 5556, f'f {freq}')
        time.sleep(2)  # 每个频率停留 2 秒
```

## 🔧 技术细节

### 架构

```
┌──────────────────────────────────────┐
│   Rust 后端 (spectrum_interactive)    │
├──────────────────────────────────────┤
│  端口 5555: 频谱数据流 (只读)        │
│  端口 5556: 控制命令 (双向)          │
└──────────────────────────────────────┘
           │                    │
           │ 频谱数据            │ 控制命令
           ↓                    ↑
┌─────────────────┐    ┌─────────────────┐
│  Python GUI     │    │  控制工具       │
│  (可视化)       │    │  (命令行)       │
└─────────────────┘    └─────────────────┘
```

### 命令协议

通过 TCP 端口 5556 发送文本命令：

| 命令 | 格式 | 示例 | 响应 |
|------|------|------|------|
| 设置频率 | `f <MHz>` | `f 145.8` | `✓ 频率设置为 145.800 MHz` |
| 设置增益 | `g <dB>` | `g 60` | `✓ 增益设置为 60.0 dB` |
| 查询状态 | `s` | `s` | 多行状态信息 |
| 帮助 | `h` | `h` | 命令列表 |

### 频率更新流程

1. 客户端发送命令：`f 145.8\n`
2. 服务器解析并更新共享配置
3. 主循环检测到 `freq_changed` 标志
4. 调用 `receiver.set_frequency()`
5. 硬件频率立即更新
6. 后续频谱数据使用新频率

**延迟**：通常 <100ms

## 📊 性能考虑

### 频率切换开销

- **切换时间**：~50-100ms
- **数据丢失**：切换期间 1-3 个缓冲区
- **建议**：避免 >10 Hz 的频繁切换

### 多客户端支持

- ✅ 多个可视化客户端可以同时连接（端口 5555）
- ✅ 多个控制客户端可以同时连接（端口 5556）
- ⚠️ 控制命令会影响所有客户端

### 线程安全

使用 `Arc<Mutex<SharedConfig>>` 保证线程安全：
- 主数据处理线程
- 控制命令处理线程
- 标准输入处理线程

## 🎛️ 高级用法

### 1. 与卫星跟踪软件集成

**Gpredict** 或 **Orbitron** 可以输出频率预测数据：

```python
# 从 Gpredict UDP 接收频率
import socket

sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
sock.bind(('0.0.0.0', 4532))

while True:
    data, addr = sock.recvfrom(1024)
    freq_mhz = parse_gpredict_data(data)
    send_command('localhost', 5556, f'f {freq_mhz}')
```

### 2. Web 控制界面

可以创建简单的 Web 界面：

```python
from flask import Flask, request
app = Flask(__name__)

@app.route('/set_freq')
def set_freq():
    freq = request.args.get('freq')
    send_command('localhost', 5556, f'f {freq}')
    return f'OK: {freq} MHz'

app.run(host='0.0.0.0', port=8080)
```

访问：`http://localhost:8080/set_freq?freq=145.8`

### 3. 语音控制

结合语音识别：

```python
import speech_recognition as sr

r = sr.Recognizer()
with sr.Microphone() as source:
    audio = r.listen(source)
    text = r.recognize_google(audio)
    
    if "frequency" in text:
        # 解析语音中的频率
        freq = extract_frequency(text)
        send_command('localhost', 5556, f'f {freq}')
```

## 🐛 故障排查

### 问题 1: 控制命令无响应

**检查**：
```bash
# 测试控制端口是否开放
telnet localhost 5556

# 手动发送命令
echo "s" | nc localhost 5556
```

### 问题 2: 频率设置失败

**可能原因**：
- 频率超出范围 (70-6000 MHz)
- 硬件不支持该频率

**解决**：查看服务器端的错误消息

### 问题 3: 频率切换时画面卡顿

**正常现象**：切换频率时会短暂中断数据流

**优化**：减少切换频率，或使用更大的可视化缓冲区

## 📝 与原版对比

| 特性 | spectrum_server | spectrum_interactive |
|------|----------------|---------------------|
| 频率调整 | ❌ 硬编码 | ✅ 运行时调整 |
| 增益调整 | ❌ 硬编码 | ✅ 运行时调整 |
| 控制接口 | ❌ 无 | ✅ 双通道 |
| 多客户端 | ✅ 支持 | ✅ 支持 |
| 复杂度 | 简单 | 中等 |
| 适用场景 | 固定监测 | 动态跟踪 |

## 🎯 推荐使用方式

### 初学者
使用**终端直接控制**，简单直观

### 日常使用
使用 **Python GUI** ([app_interactive.py](app_interactive.py))，可视化 + 快速调整

### 自动化/集成
使用**控制工具** ([control.py](control.py))，适合脚本调用

### 卫星跟踪
编写专用脚本，结合轨道预测数据自动调整频率

## 💡 未来扩展

可以进一步添加：
- 🔄 频率扫描模式
- 💾 频率预设存储
- 📊 频谱录制与回放
- 🌐 Web 控制面板
- 🤖 自动信号检测与跟踪
- 📡 与 Gpredict/Orbitron 直接集成

---

现在你可以实时调整频率来跟踪卫星信号了！🛰️
