import sys
import numpy as np
import pyaudio
import pyqtgraph as pg
import dsp_bridge
from PyQt6 import QtCore, QtWidgets

class WaterfallSpectrogram(QtWidgets.QMainWindow):
    def __init__(self):
        super().__init__()

        self.FS = 96000        # 采样率
        self.CHUNK = 1024      # 每个数据块的大小
        self.ROWS = 200        # 瀑布图显示的历史行数
        self.COLS = self.CHUNK // 2  # 频率轴点数 (Nyquist)
        
        self.p = pyaudio.PyAudio()
        self.stream = self.p.open(
            format=pyaudio.paInt16,
            channels=1,
            rate=self.FS,
            input=True,
            frames_per_buffer=self.CHUNK,
            input_device_index=2
        )
        
        self.handler = dsp_bridge.FftHandler(self.CHUNK)

        self.setWindowTitle("Real-Time Waterfall Spectrogram")
        self.main_widget = QtWidgets.QWidget()
        self.setCentralWidget(self.main_widget)
        self.vlayout = QtWidgets.QVBoxLayout(self.main_widget)

        self.win = pg.GraphicsLayoutWidget()
        self.vlayout.addWidget(self.win)
        
        self.p1 = self.win.addPlot()  # type: ignore
        self.img = pg.ImageItem()
        self.p1.addItem(self.img)

        colormap = pg.colormap.get('viridis')
        bar = pg.ColorBarItem(values=(0, 100), colorMap=colormap)
        bar.setImageItem(self.img)

        # initialize data matrix, rows: time steps, cols: frequency bins
        self.data_matrix = np.zeros((self.ROWS, self.COLS))

        self.p1.setLabel('bottom', 'Frequency Bin', units='index')
        self.p1.setLabel('left', 'Time Steps')

        # timer: control refresh rate
        self.timer = QtCore.QTimer()
        self.timer.timeout.connect(self.update)
        self.timer.start(60)

    def update(self):
        raw_bytes = self.stream.read(self.CHUNK, exception_on_overflow=False)
        # convert bytes to int16 array to pass to Rust
        y = np.frombuffer(raw_bytes, dtype=np.int16)
        db_data = self.handler.compute_db_spectrum(y)
        
        # update data matrix for waterfall display
        self.data_matrix = np.roll(self.data_matrix, -1, axis=0)
        self.data_matrix[-1, :] = db_data
        self.img.setImage(self.data_matrix.T, autoLevels=False)

    def closeEvent(self, event):
        self.stream.stop_stream()
        self.stream.close()
        self.p.terminate()
        event.accept()

if __name__ == "__main__":
    app = QtWidgets.QApplication(sys.argv)
    window = WaterfallSpectrogram()
    window.show()
    sys.exit(app.exec())