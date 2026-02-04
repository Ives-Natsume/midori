#!/usr/bin/env python3
"""
Pluto SDR 频谱可视化工具
接收来自 Rust 后端的 FFT 数据并实时显示
"""

import socket
import json
import numpy as np
import matplotlib.pyplot as plt
from matplotlib.animation import FuncAnimation
from collections import deque
import argparse

class SpectrumViewer:
    """实时频谱可视化器"""
    
    def __init__(self, host='localhost', port=5555, mode='spectrum'):
        self.host = host
        self.port = port
        self.mode = mode  # 'spectrum' 或 'waterfall'
        
        # 设置图形
        self.fig, self.ax = plt.subplots(figsize=(12, 6))
        
        if mode == 'spectrum':
            self._setup_spectrum_plot()
        elif mode == 'waterfall':
            self._setup_waterfall_plot()
        
        # 数据缓冲
        self.data_buffer = deque(maxlen=100)
        
        # Socket 连接
        self.sock = None
        
    def _setup_spectrum_plot(self):
        """设置功率谱显示"""
        self.line, = self.ax.plot([], [], 'b-', linewidth=1)
        self.ax.set_xlabel('Frequency (MHz)')
        self.ax.set_ylabel('Power (dB)')
        self.ax.set_title('Real-time Power Spectrum')
        self.ax.grid(True, alpha=0.3)
        self.ax.set_ylim(-100, 0)
        
    def _setup_waterfall_plot(self):
        """设置瀑布图显示"""
        self.waterfall_data = np.zeros((100, 1024))
        self.waterfall_img = self.ax.imshow(
            self.waterfall_data,
            aspect='auto',
            cmap='viridis',
            interpolation='bilinear',
            vmin=-100,
            vmax=0
        )
        self.ax.set_xlabel('Frequency Bin')
        self.ax.set_ylabel('Time')
        self.ax.set_title('Spectrum Waterfall')
        self.fig.colorbar(self.waterfall_img, ax=self.ax, label='Power (dB)')
    
    def connect(self):
        """连接到 Rust 后端"""
        try:
            self.sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            self.sock.connect((self.host, self.port))
            self.sock.settimeout(0.1)  # 100ms 超时
            print(f"Connected to {self.host}:{self.port}")
            return True
        except Exception as e:
            print(f"Connection failed: {e}")
            return False
    
    def receive_data(self):
        """接收频谱数据"""
        try:
            # 接收数据长度（4字节）
            length_bytes = self.sock.recv(4)
            if not length_bytes:
                return None
            
            data_length = int.from_bytes(length_bytes, byteorder='little')
            
            # 接收实际数据
            data = b''
            while len(data) < data_length:
                chunk = self.sock.recv(min(4096, data_length - len(data)))
                if not chunk:
                    return None
                data += chunk
            
            # 解析 JSON
            spectrum_data = json.loads(data.decode('utf-8'))
            return spectrum_data
            
        except socket.timeout:
            return None
        except Exception as e:
            print(f"Receive error: {e}")
            return None
    
    def update_spectrum(self, frame):
        """更新功率谱显示"""
        data = self.receive_data()
        
        if data is None:
            return self.line,
        
        spectrum = np.array(data['spectrum'])
        freqs = np.array(data.get('frequencies', range(len(spectrum))))
        
        # 更新数据
        self.line.set_data(freqs, spectrum)
        
        # 自动调整 x 轴
        if len(freqs) > 0:
            self.ax.set_xlim(freqs[0], freqs[-1])
        
        # 更新标题显示当前频率
        center_freq = data.get('center_freq_hz', 0) / 1e6
        sample_rate = data.get('sample_rate_hz', 0) / 1e6
        self.ax.set_title(
            f'Power Spectrum - Center: {center_freq:.2f} MHz, SR: {sample_rate:.2f} MSPS'
        )
        
        return self.line,
    
    def update_waterfall(self, frame):
        """更新瀑布图显示"""
        data = self.receive_data()
        
        if data is None:
            return self.waterfall_img,
        
        spectrum = np.array(data['spectrum'])
        
        # 滚动瀑布图
        self.waterfall_data = np.roll(self.waterfall_data, 1, axis=0)
        
        # 添加新数据
        if len(spectrum) == self.waterfall_data.shape[1]:
            self.waterfall_data[0] = spectrum
        else:
            # 调整大小以匹配
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
            print("Failed to connect. Make sure Rust backend is running.")
            return
        
        try:
            if self.mode == 'spectrum':
                ani = FuncAnimation(
                    self.fig,
                    self.update_spectrum,
                    interval=33,  # ~30 FPS
                    blit=True,
                    cache_frame_data=False
                )
            elif self.mode == 'waterfall':
                ani = FuncAnimation(
                    self.fig,
                    self.update_waterfall,
                    interval=100,  # ~10 FPS
                    blit=True,
                    cache_frame_data=False
                )
            
            plt.show()
            
        except KeyboardInterrupt:
            print("\nStopping...")
        finally:
            if self.sock:
                self.sock.close()
                print("Disconnected")


def main():
    parser = argparse.ArgumentParser(description='Pluto SDR Spectrum Viewer')
    parser.add_argument('--host', default='localhost', help='Backend host')
    parser.add_argument('--port', type=int, default=5555, help='Backend port')
    parser.add_argument('--mode', choices=['spectrum', 'waterfall'], 
                        default='spectrum', help='Visualization mode')
    
    args = parser.parse_args()
    
    viewer = SpectrumViewer(
        host=args.host,
        port=args.port,
        mode=args.mode
    )
    
    print(f"Starting {args.mode} viewer...")
    print(f"Connecting to {args.host}:{args.port}")
    print("\nControls:")
    print("  - Close window to exit")
    print("  - Ctrl+C to stop")
    
    viewer.run()


if __name__ == '__main__':
    main()
