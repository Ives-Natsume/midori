#!/usr/bin/env python3
"""
Pluto SDR 频谱查看器（带控制功能）
支持实时调整频率和增益
"""

import socket
import json
import numpy as np
import matplotlib.pyplot as plt
from matplotlib.animation import FuncAnimation
from matplotlib.widgets import TextBox, Button
import argparse

class InteractiveSpectrumViewer:
    """交互式频谱可视化器（支持频率控制）"""
    
    def __init__(self, host='localhost', spectrum_port=5555, control_port=5556, mode='spectrum'):
        self.host = host
        self.spectrum_port = spectrum_port
        self.control_port = control_port
        self.mode = mode
        
        # Socket 连接
        self.spectrum_sock = None
        self.control_sock = None
        
        # 当前状态
        self.current_freq_mhz = 144.0
        self.current_gain_db = 50.0
        
        # 设置图形界面
        self.fig = plt.figure(figsize=(14, 8))
        
        # 主绘图区域
        self.ax_spectrum = plt.subplot2grid((4, 4), (0, 0), colspan=4, rowspan=3)
        
        # 控制面板区域
        self.ax_freq_box = plt.subplot2grid((4, 4), (3, 0))
        self.ax_freq_btn = plt.subplot2grid((4, 4), (3, 1))
        self.ax_gain_box = plt.subplot2grid((4, 4), (3, 2))
        self.ax_gain_btn = plt.subplot2grid((4, 4), (3, 3))
        
        if mode == 'spectrum':
            self._setup_spectrum_plot()
        elif mode == 'waterfall':
            self._setup_waterfall_plot()
        
        # 添加控制组件
        self._setup_controls()
        
    def _setup_spectrum_plot(self):
        """设置功率谱显示"""
        self.line, = self.ax_spectrum.plot([], [], 'b-', linewidth=1)
        self.ax_spectrum.set_xlabel('Frequency (MHz)', fontsize=11)
        self.ax_spectrum.set_ylabel('Power (dB)', fontsize=11)
        self.ax_spectrum.set_title('Real-time Power Spectrum (Interactive)', fontsize=12, fontweight='bold')
        self.ax_spectrum.grid(True, alpha=0.3)
        self.ax_spectrum.set_ylim(-100, 0)
        
    def _setup_waterfall_plot(self):
        """设置瀑布图显示"""
        self.waterfall_data = np.zeros((100, 1024))
        self.waterfall_img = self.ax_spectrum.imshow(
            self.waterfall_data,
            aspect='auto',
            cmap='viridis',
            interpolation='bilinear',
            vmin=-100,
            vmax=0
        )
        self.ax_spectrum.set_xlabel('Frequency Bin', fontsize=11)
        self.ax_spectrum.set_ylabel('Time', fontsize=11)
        self.ax_spectrum.set_title('Spectrum Waterfall (Interactive)', fontsize=12, fontweight='bold')
        self.fig.colorbar(self.waterfall_img, ax=self.ax_spectrum, label='Power (dB)')
    
    def _setup_controls(self):
        """设置控制组件"""
        # 频率输入框
        self.freq_box = TextBox(self.ax_freq_box, 'Freq (MHz)', initial=str(self.current_freq_mhz))
        
        # 频率设置按钮
        self.freq_btn = Button(self.ax_freq_btn, 'Set Freq')
        self.freq_btn.on_clicked(self._on_freq_button)
        
        # 增益输入框
        self.gain_box = TextBox(self.ax_gain_box, 'Gain (dB)', initial=str(self.current_gain_db))
        
        # 增益设置按钮
        self.gain_btn = Button(self.ax_gain_btn, 'Set Gain')
        self.gain_btn.on_clicked(self._on_gain_button)
        
        # 键盘快捷键
        self.fig.canvas.mpl_connect('key_press_event', self._on_key_press)
    
    def _on_freq_button(self, event):
        """频率按钮点击处理"""
        try:
            freq_mhz = float(self.freq_box.text)
            self.set_frequency(freq_mhz)
        except ValueError:
            print(f"错误: 无效的频率值 '{self.freq_box.text}'")
    
    def _on_gain_button(self, event):
        """增益按钮点击处理"""
        try:
            gain_db = float(self.gain_box.text)
            self.set_gain(gain_db)
        except ValueError:
            print(f"错误: 无效的增益值 '{self.gain_box.text}'")
    
    def _on_key_press(self, event):
        """键盘快捷键处理"""
        if event.key == 'enter':
            self._on_freq_button(None)
        elif event.key == 'up':
            # 向上微调频率 (+10 kHz)
            freq = self.current_freq_mhz + 0.01
            self.freq_box.set_val(f"{freq:.3f}")
            self.set_frequency(freq)
        elif event.key == 'down':
            # 向下微调频率 (-10 kHz)
            freq = self.current_freq_mhz - 0.01
            self.freq_box.set_val(f"{freq:.3f}")
            self.set_frequency(freq)
        elif event.key == 'pageup':
            # 向上大幅调整 (+100 kHz)
            freq = self.current_freq_mhz + 0.1
            self.freq_box.set_val(f"{freq:.3f}")
            self.set_frequency(freq)
        elif event.key == 'pagedown':
            # 向下大幅调整 (-100 kHz)
            freq = self.current_freq_mhz - 0.1
            self.freq_box.set_val(f"{freq:.3f}")
            self.set_frequency(freq)
    
    def connect(self):
        """连接到 Rust 后端"""
        try:
            # 连接频谱数据端口
            self.spectrum_sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            self.spectrum_sock.connect((self.host, self.spectrum_port))
            self.spectrum_sock.settimeout(0.1)
            print(f"✓ 连接到频谱服务器 {self.host}:{self.spectrum_port}")
            
            # 连接控制端口
            self.control_sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            self.control_sock.connect((self.host, self.control_port))
            print(f"✓ 连接到控制服务器 {self.host}:{self.control_port}")
            
            return True
        except Exception as e:
            print(f"连接失败: {e}")
            return False
    
    def set_frequency(self, freq_mhz):
        """发送频率控制命令"""
        if not self.control_sock:
            print("错误: 未连接到控制服务器")
            return
        
        try:
            command = f"f {freq_mhz}\n"
            self.control_sock.send(command.encode())
            response = self.control_sock.recv(1024).decode().strip()
            print(f"→ {response}")
            self.current_freq_mhz = freq_mhz
        except Exception as e:
            print(f"发送频率命令失败: {e}")
    
    def set_gain(self, gain_db):
        """发送增益控制命令"""
        if not self.control_sock:
            print("错误: 未连接到控制服务器")
            return
        
        try:
            command = f"g {gain_db}\n"
            self.control_sock.send(command.encode())
            response = self.control_sock.recv(1024).decode().strip()
            print(f"→ {response}")
            self.current_gain_db = gain_db
        except Exception as e:
            print(f"发送增益命令失败: {e}")
    
    def receive_data(self):
        """接收频谱数据"""
        try:
            length_bytes = self.spectrum_sock.recv(4)
            if not length_bytes:
                return None
            
            data_length = int.from_bytes(length_bytes, byteorder='little')
            
            data = b''
            while len(data) < data_length:
                chunk = self.spectrum_sock.recv(min(4096, data_length - len(data)))
                if not chunk:
                    return None
                data += chunk
            
            spectrum_data = json.loads(data.decode('utf-8'))
            return spectrum_data
            
        except socket.timeout:
            return None
        except Exception as e:
            print(f"接收错误: {e}")
            return None
    
    def update_spectrum(self, frame):
        """更新功率谱显示"""
        data = self.receive_data()
        
        if data is None:
            return self.line,
        
        spectrum = np.array(data['spectrum'])
        freqs = np.array(data.get('frequencies', range(len(spectrum))))
        
        self.line.set_data(freqs, spectrum)
        
        if len(freqs) > 0:
            self.ax_spectrum.set_xlim(freqs[0], freqs[-1])
        
        center_freq = data.get('center_freq_hz', 0) / 1e6
        sample_rate = data.get('sample_rate_hz', 0) / 1e6
        self.ax_spectrum.set_title(
            f'Power Spectrum - Center: {center_freq:.3f} MHz, SR: {sample_rate:.2f} MSPS',
            fontsize=12,
            fontweight='bold'
        )
        
        return self.line,
    
    def update_waterfall(self, frame):
        """更新瀑布图显示"""
        data = self.receive_data()
        
        if data is None:
            return self.waterfall_img,
        
        spectrum = np.array(data['spectrum'])
        
        self.waterfall_data = np.roll(self.waterfall_data, 1, axis=0)
        
        if len(spectrum) == self.waterfall_data.shape[1]:
            self.waterfall_data[0] = spectrum
        else:
            spectrum_resized = np.interp(
                np.linspace(0, len(spectrum), self.waterfall_data.shape[1]),
                np.arange(len(spectrum)),
                spectrum
            )
            self.waterfall_data[0] = spectrum_resized
        
        self.waterfall_img.set_data(self.waterfall_data)
        
        return self.waterfall_img,
    
    def run(self):
        """运行可视化"""
        if not self.connect():
            print("连接失败。请确保 Rust 后端正在运行:")
            print("  cargo run --example spectrum_interactive")
            return
        
        print("\n控制方式:")
        print("  • 在输入框中输入频率/增益，点击按钮设置")
        print("  • 或使用键盘快捷键:")
        print("    ↑/↓     - 微调频率 (±10 kHz)")
        print("    PgUp/PgDn - 大幅调整 (±100 kHz)")
        print("    Enter   - 应用当前频率")
        print()
        
        try:
            if self.mode == 'spectrum':
                ani = FuncAnimation(
                    self.fig,
                    self.update_spectrum,
                    interval=33,
                    blit=True,
                    cache_frame_data=False
                )
            elif self.mode == 'waterfall':
                ani = FuncAnimation(
                    self.fig,
                    self.update_waterfall,
                    interval=100,
                    blit=True,
                    cache_frame_data=False
                )
            
            plt.show()
            
        except KeyboardInterrupt:
            print("\n正在停止...")
        finally:
            if self.spectrum_sock:
                self.spectrum_sock.close()
            if self.control_sock:
                self.control_sock.close()
            print("已断开连接")


def main():
    parser = argparse.ArgumentParser(description='Pluto SDR Interactive Spectrum Viewer')
    parser.add_argument('--host', default='localhost', help='Backend host')
    parser.add_argument('--spectrum-port', type=int, default=5555, help='Spectrum data port')
    parser.add_argument('--control-port', type=int, default=5556, help='Control command port')
    parser.add_argument('--mode', choices=['spectrum', 'waterfall'], 
                        default='waterfall', help='Visualization mode')
    
    args = parser.parse_args()
    
    viewer = InteractiveSpectrumViewer(
        host=args.host,
        spectrum_port=args.spectrum_port,
        control_port=args.control_port,
        mode=args.mode
    )
    
    print(f"启动 {args.mode} 交互式查看器...")
    print(f"连接到 {args.host}:{args.spectrum_port} (数据)")
    print(f"连接到 {args.host}:{args.control_port} (控制)")
    
    viewer.run()


if __name__ == '__main__':
    main()
