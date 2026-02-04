# 动态频率控制 - 快速参考

## 🚀 三种控制方式

### 1️⃣ 终端直接控制（最简单）

```bash
cargo run --example spectrum_interactive
```

在终端直接输入：
```
f 145.8    # 设置频率到 145.8 MHz
g 60       # 设置增益到 60 dB
s          # 查看状态
```

### 2️⃣ Python GUI（推荐）

```bash
# 终端 1
cargo run --example spectrum_interactive

# 终端 2
python app_interactive.py
```

**快捷键**：
- `↑` / `↓` - 微调 ±10 kHz
- `PageUp` / `PageDown` - 大调 ±100 kHz
- `Enter` - 应用频率

### 3️⃣ 命令行工具（自动化）

```bash
python control.py -f 145.8    # 设置频率
python control.py -g 60       # 设置增益
python control.py -s          # 查询状态
```

## 🛰️ 卫星跟踪示例

### ISS (国际空间站)

```bash
# 手动跟踪
cargo run --example spectrum_interactive
python app_interactive.py

# 在 GUI 中使用 ↑↓ 键实时调整
# ISS 频率：145.820 - 145.830 MHz
```

### 自动化脚本

```python
# doppler_compensate.py
import time
from control import send_command

# 预测的频率序列
freqs = [145.820, 145.822, 145.825, 145.828, 145.830]

for f in freqs:
    send_command('localhost', 5556, f'f {f}')
    time.sleep(10)
```

```bash
python doppler_compensate.py
```

## 📡 频段扫描

```bash
# 扫描 144-146 MHz
for f in $(seq 144 0.1 146); do
    python control.py -f $f
    sleep 0.5
done
```

## 🎛️ 命令参考

| 命令 | 格式 | 示例 |
|------|------|------|
| 频率 | `f <MHz>` | `f 145.8` |
| 增益 | `g <dB>` | `g 60` |
| 状态 | `s` | `s` |
| 帮助 | `h` | `h` |
| 退出 | `q` | `q` |

## 🔌 端口

- **5555** - 频谱数据（可视化）
- **5556** - 控制命令

## 📝 完整文档

详见 [DYNAMIC_CONTROL.md](DYNAMIC_CONTROL.md)
