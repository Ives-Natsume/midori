/// 纯 libiio-sys 接收器实现
/// 
/// 这个接收器完全使用 libiio-sys，避免与 industrial-io 的冲突

use num_complex::Complex;
use anyhow::Result;
use std::ptr;
use std::ffi::CString;

/// 接收器配置
#[derive(Debug, Clone)]
pub struct LibiioRxConfig {
    /// 中心频率 (Hz)
    pub center_freq_hz: u64,
    /// 采样率 (Hz)
    pub sample_rate_hz: u64,
    /// RF 带宽 (Hz)
    pub bandwidth_hz: u64,
    /// 接收增益 (dB)
    pub rx_gain_db: f64,
    /// 缓冲区大小（样本数）
    pub buffer_size: usize,
    /// 增益控制模式
    pub gain_mode: String,
}

impl Default for LibiioRxConfig {
    fn default() -> Self {
        Self {
            center_freq_hz: 915_000_000,
            sample_rate_hz: 2_084_000,
            bandwidth_hz: 2_000_000,
            rx_gain_db: 50.0,
            buffer_size: 8192,
            gain_mode: "manual".to_string(),
        }
    }
}

/// 纯 libiio-sys 接收器
pub struct LibiioReceiver {
    config: LibiioRxConfig,
    ctx: *mut libiio_sys::iio_context,
    phy_dev: *mut libiio_sys::iio_device,
    rx_dev: *mut libiio_sys::iio_device,
    buffer: *mut libiio_sys::iio_buffer,
}

unsafe impl Send for LibiioReceiver {}

impl LibiioReceiver {
    /// 创建新的接收器实例
    pub fn new(config: LibiioRxConfig) -> Self {
        Self {
            config,
            ctx: ptr::null_mut(),
            phy_dev: ptr::null_mut(),
            rx_dev: ptr::null_mut(),
            buffer: ptr::null_mut(),
        }
    }
    
    /// 使用默认配置创建
    pub fn with_defaults() -> Self {
        Self::new(LibiioRxConfig::default())
    }
    
    /// 连接并配置设备
    pub fn connect(&mut self) -> Result<()> {
        unsafe {
            // 1. 创建 context
            println!("正在连接到 Pluto SDR...");
            let uri = CString::new("usb:")?;
            self.ctx = libiio_sys::iio_create_context_from_uri(uri.as_ptr());
            
            if self.ctx.is_null() {
                return Err(anyhow::anyhow!("无法连接到 Pluto SDR (USB)"));
            }
            println!("✓ 已连接到 Pluto SDR\n");
            
            // 2. 获取设备
            let phy_name = CString::new("ad9361-phy")?;
            let rx_name = CString::new("cf-ad9361-lpc")?;
            
            self.phy_dev = libiio_sys::iio_context_find_device(self.ctx, phy_name.as_ptr());
            self.rx_dev = libiio_sys::iio_context_find_device(self.ctx, rx_name.as_ptr());
            
            if self.phy_dev.is_null() || self.rx_dev.is_null() {
                return Err(anyhow::anyhow!("无法找到 PHY 或接收设备"));
            }
            
            println!("正在配置接收参数...\n");
            
            // 3. 配置 PHY
            self.configure_phy()?;
            
            // 4. 启用通道并创建缓冲区
            self.create_buffer()?;
            
            println!("\n✓ 接收器配置完成！\n");
        }
        
        Ok(())
    }
    
    /// 配置 PHY 参数
    fn configure_phy(&self) -> Result<()> {
        unsafe {
            // 设置频率
            let freq_chan_name = CString::new("altvoltage0")?;
            let freq_chan = libiio_sys::iio_device_find_channel(self.phy_dev, freq_chan_name.as_ptr(), false);
            if !freq_chan.is_null() {
                let freq_attr = CString::new("frequency")?;
                let freq_val = CString::new(self.config.center_freq_hz.to_string())?;
                libiio_sys::iio_channel_attr_write(freq_chan, freq_attr.as_ptr(), freq_val.as_ptr());
                println!("✓ 中心频率: {:.3} MHz", self.config.center_freq_hz as f64 / 1e6);
            }
            
            // 设置采样率
            let rx_chan_name = CString::new("voltage0")?;
            let rx_chan = libiio_sys::iio_device_find_channel(self.phy_dev, rx_chan_name.as_ptr(), true);
            if !rx_chan.is_null() {
                let sr_attr = CString::new("sampling_frequency")?;
                let sr_val = CString::new(self.config.sample_rate_hz.to_string())?;
                libiio_sys::iio_channel_attr_write(rx_chan, sr_attr.as_ptr(), sr_val.as_ptr());
                println!("✓ 采样率: {:.3} MSPS", self.config.sample_rate_hz as f64 / 1e6);
                
                // 设置带宽
                let bw_attr = CString::new("rf_bandwidth")?;
                let bw_val = CString::new(self.config.bandwidth_hz.to_string())?;
                libiio_sys::iio_channel_attr_write(rx_chan, bw_attr.as_ptr(), bw_val.as_ptr());
                println!("✓ RF 带宽: {:.3} MHz", self.config.bandwidth_hz as f64 / 1e6);
                
                // 设置增益模式
                let gain_mode_attr = CString::new("gain_control_mode")?;
                let gain_mode_val = CString::new(self.config.gain_mode.as_str())?;
                libiio_sys::iio_channel_attr_write(rx_chan, gain_mode_attr.as_ptr(), gain_mode_val.as_ptr());
                println!("✓ 增益模式: {}", self.config.gain_mode);
                
                // 如果是手动模式，设置增益
                if self.config.gain_mode == "manual" {
                    let gain_attr = CString::new("hardwaregain")?;
                    let gain_val = CString::new(format!("{}", self.config.rx_gain_db))?;
                    libiio_sys::iio_channel_attr_write(rx_chan, gain_attr.as_ptr(), gain_val.as_ptr());
                    println!("✓ RX 增益: {} dB", self.config.rx_gain_db);
                }
            }
        }
        
        Ok(())
    }
    
    /// 创建缓冲区
    fn create_buffer(&mut self) -> Result<()> {
        unsafe {
            // 启用 I/Q 通道
            let i_chan_name = CString::new("voltage0")?;
            let q_chan_name = CString::new("voltage1")?;
            
            let i_chan = libiio_sys::iio_device_find_channel(self.rx_dev, i_chan_name.as_ptr(), false);
            let q_chan = libiio_sys::iio_device_find_channel(self.rx_dev, q_chan_name.as_ptr(), false);
            
            if i_chan.is_null() || q_chan.is_null() {
                return Err(anyhow::anyhow!("无法找到 I/Q 通道"));
            }
            
            libiio_sys::iio_channel_enable(i_chan);
            libiio_sys::iio_channel_enable(q_chan);
            println!("✓ 已启用 I/Q 通道");
            
            // 创建缓冲区
            self.buffer = libiio_sys::iio_device_create_buffer(self.rx_dev, self.config.buffer_size, false);
            
            if self.buffer.is_null() {
                return Err(anyhow::anyhow!("无法创建缓冲区"));
            }
            
            println!("✓ 创建缓冲区: {} 样本", self.config.buffer_size);
        }
        
        Ok(())
    }
    
    /// 接收 IQ 样本
    pub fn receive_samples(&self) -> Result<Vec<Complex<f32>>> {
        if self.buffer.is_null() {
            return Err(anyhow::anyhow!("缓冲区未初始化"));
        }
        
        unsafe {
            // 填充缓冲区
            let bytes_read = libiio_sys::iio_buffer_refill(self.buffer);
            
            if bytes_read < 0 {
                return Err(anyhow::anyhow!("缓冲区填充失败: {}", bytes_read));
            }
            
            // 获取数据
            let start_ptr = libiio_sys::iio_buffer_start(self.buffer) as *const u8;
            let end_ptr = libiio_sys::iio_buffer_end(self.buffer) as *const u8;
            
            if start_ptr.is_null() || end_ptr.is_null() {
                return Err(anyhow::anyhow!("无法获取缓冲区数据"));
            }
            
            let data_len = end_ptr as usize - start_ptr as usize;
            let data_slice = std::slice::from_raw_parts(start_ptr, data_len);
            
            // 转换为 IQ 样本
            let samples = convert_to_complex(data_slice);
            
            Ok(samples)
        }
    }
    
    /// 设置频率
    pub fn set_frequency(&self, freq_hz: u64) -> Result<()> {
        unsafe {
            let freq_chan_name = CString::new("altvoltage0")?;
            let freq_chan = libiio_sys::iio_device_find_channel(self.phy_dev, freq_chan_name.as_ptr(), false);
            if !freq_chan.is_null() {
                let freq_attr = CString::new("frequency")?;
                let freq_val = CString::new(freq_hz.to_string())?;
                libiio_sys::iio_channel_attr_write(freq_chan, freq_attr.as_ptr(), freq_val.as_ptr());
                println!("频率已更新至: {:.3} MHz", freq_hz as f64 / 1e6);
            }
        }
        Ok(())
    }
    
    /// 设置增益
    pub fn set_gain(&self, gain_db: f64) -> Result<()> {
        unsafe {
            let rx_chan_name = CString::new("voltage0")?;
            let rx_chan = libiio_sys::iio_device_find_channel(self.phy_dev, rx_chan_name.as_ptr(), true);
            if !rx_chan.is_null() {
                let gain_attr = CString::new("hardwaregain")?;
                let gain_val = CString::new(format!("{}", gain_db))?;
                libiio_sys::iio_channel_attr_write(rx_chan, gain_attr.as_ptr(), gain_val.as_ptr());
                println!("增益已更新至: {} dB", gain_db);
            }
        }
        Ok(())
    }
    
    /// 获取配置
    pub fn config(&self) -> &LibiioRxConfig {
        &self.config
    }
}

impl Drop for LibiioReceiver {
    fn drop(&mut self) {
        unsafe {
            if !self.buffer.is_null() {
                libiio_sys::iio_buffer_destroy(self.buffer);
                self.buffer = ptr::null_mut();
            }
            if !self.ctx.is_null() {
                libiio_sys::iio_context_destroy(self.ctx);
                self.ctx = ptr::null_mut();
            }
        }
    }
}

/// 转换为复数样本
fn convert_to_complex(data: &[u8]) -> Vec<Complex<f32>> {
    let mut samples = Vec::new();
    
    for chunk in data.chunks(4) {
        if chunk.len() == 4 {
            let i_raw = i16::from_le_bytes([chunk[0], chunk[1]]);
            let q_raw = i16::from_le_bytes([chunk[2], chunk[3]]);
            
            let i = i_raw as f32 / 2048.0;
            let q = q_raw as f32 / 2048.0;
            
            samples.push(Complex::new(i, q));
        }
    }
    
    samples
}

/// 信号处理工具
pub mod signal {
    use num_complex::Complex;
    
    /// 计算信号功率（线性）
    pub fn calculate_power(samples: &[Complex<f32>]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        
        let sum: f32 = samples.iter().map(|s| s.norm_sqr()).sum();
        sum / samples.len() as f32
    }
    
    /// 计算信号功率（dB）
    pub fn calculate_power_db(samples: &[Complex<f32>]) -> f32 {
        let power = calculate_power(samples);
        if power > 0.0 {
            10.0 * power.log10()
        } else {
            -std::f32::INFINITY
        }
    }
    
    /// 计算信号幅度
    pub fn calculate_magnitude(samples: &[Complex<f32>]) -> Vec<f32> {
        samples.iter().map(|s| s.norm()).collect()
    }
    
    /// 计算信号相位
    pub fn calculate_phase(samples: &[Complex<f32>]) -> Vec<f32> {
        samples.iter().map(|s| s.arg()).collect()
    }
}
