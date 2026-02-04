/// 频谱分析模块
/// 
/// 提供 FFT 计算和频谱数据处理功能

use num_complex::Complex;
use std::f32::consts::PI;

/// 频谱分析器配置
#[derive(Debug, Clone)]
pub struct SpectrumConfig {
    pub fft_size: usize,
    pub window_type: WindowType,
    pub averaging: usize,
}

/// 窗函数类型
#[derive(Debug, Clone, Copy)]
pub enum WindowType {
    Rectangular,
    Hann,
    Hamming,
    Blackman,
}

impl Default for SpectrumConfig {
    fn default() -> Self {
        Self {
            fft_size: 1024,
            window_type: WindowType::Hann,
            averaging: 4,
        }
    }
}

/// 频谱分析器
pub struct SpectrumAnalyzer {
    config: SpectrumConfig,
    window: Vec<f32>,
    fft_buffer: Vec<Complex<f32>>,
    averaging_buffer: Vec<Vec<f32>>,
    avg_index: usize,
}

impl SpectrumAnalyzer {
    /// 创建新的频谱分析器
    pub fn new(config: SpectrumConfig) -> Self {
        let window = generate_window(config.fft_size, config.window_type);
        
        Self {
            fft_buffer: vec![Complex::new(0.0, 0.0); config.fft_size],
            averaging_buffer: vec![vec![0.0; config.fft_size]; config.averaging],
            avg_index: 0,
            window,
            config,
        }
    }
    
    /// 处理 IQ 样本，返回功率谱（dB）
    pub fn process(&mut self, samples: &[Complex<f32>]) -> Vec<f32> {
        // 确保有足够的样本
        if samples.len() < self.config.fft_size {
            return vec![0.0; self.config.fft_size];
        }
        
        // 应用窗函数并复制到 FFT 缓冲区
        for i in 0..self.config.fft_size {
            self.fft_buffer[i] = samples[i] * self.window[i];
        }
        
        // 执行 FFT（简化版 - 实际应使用 rustfft）
        let spectrum = self.simple_fft(&self.fft_buffer);
        
        // 计算功率谱（dB）
        let power_spectrum: Vec<f32> = spectrum
            .iter()
            .map(|c| {
                let magnitude = c.norm();
                20.0 * (magnitude + 1e-10).log10()  // 避免 log(0)
            })
            .collect();
        
        // 移动平均
        if self.config.averaging > 1 {
            self.averaging_buffer[self.avg_index] = power_spectrum.clone();
            self.avg_index = (self.avg_index + 1) % self.config.averaging;
            
            self.compute_average()
        } else {
            power_spectrum
        }
    }
    
    /// 计算移动平均
    fn compute_average(&self) -> Vec<f32> {
        let mut result = vec![0.0; self.config.fft_size];
        
        for buffer in &self.averaging_buffer {
            for (i, val) in buffer.iter().enumerate() {
                result[i] += val;
            }
        }
        
        for val in &mut result {
            *val /= self.config.averaging as f32;
        }
        
        result
    }
    
    /// 简化的 FFT 实现（仅用于演示）
    /// 实际项目应使用 rustfft 库
    fn simple_fft(&self, input: &[Complex<f32>]) -> Vec<Complex<f32>> {
        // 这里使用简化的 DFT 实现
        // 警告：性能很差，仅用于演示！
        let n = input.len();
        let mut output = vec![Complex::new(0.0, 0.0); n];
        
        for k in 0..n {
            for (j, &sample) in input.iter().enumerate() {
                let angle = -2.0 * PI * (k * j) as f32 / n as f32;
                let twiddle = Complex::new(angle.cos(), angle.sin());
                output[k] += sample * twiddle;
            }
        }
        
        output
    }
    
    /// FFT 移位（将 DC 移到中心）
    pub fn fftshift(&self, spectrum: Vec<f32>) -> Vec<f32> {
        let n = spectrum.len();
        let half = n / 2;
        
        let mut shifted = vec![0.0; n];
        shifted[..half].copy_from_slice(&spectrum[half..]);
        shifted[half..].copy_from_slice(&spectrum[..half]);
        
        shifted
    }
}

/// 生成窗函数
fn generate_window(size: usize, window_type: WindowType) -> Vec<f32> {
    match window_type {
        WindowType::Rectangular => vec![1.0; size],
        
        WindowType::Hann => {
            (0..size)
                .map(|i| {
                    0.5 - 0.5 * (2.0 * PI * i as f32 / (size - 1) as f32).cos()
                })
                .collect()
        }
        
        WindowType::Hamming => {
            (0..size)
                .map(|i| {
                    0.54 - 0.46 * (2.0 * PI * i as f32 / (size - 1) as f32).cos()
                })
                .collect()
        }
        
        WindowType::Blackman => {
            (0..size)
                .map(|i| {
                    let x = i as f32 / (size - 1) as f32;
                    0.42 - 0.5 * (2.0 * PI * x).cos() + 0.08 * (4.0 * PI * x).cos()
                })
                .collect()
        }
    }
}

/// 频率轴生成
pub fn generate_frequency_axis(
    center_freq_hz: u64,
    sample_rate_hz: u64,
    num_points: usize,
) -> Vec<f64> {
    let start_freq = center_freq_hz as f64 - (sample_rate_hz as f64 / 2.0);
    let freq_step = sample_rate_hz as f64 / num_points as f64;
    
    (0..num_points)
        .map(|i| start_freq + i as f64 * freq_step)
        .collect()
}

/// 峰值检测
pub struct Peak {
    pub index: usize,
    pub frequency_hz: f64,
    pub power_db: f32,
}

pub fn detect_peaks(
    spectrum: &[f32],
    frequencies: &[f64],
    threshold_db: f32,
    min_distance: usize,
) -> Vec<Peak> {
    let mut peaks = Vec::new();
    
    for i in min_distance..spectrum.len() - min_distance {
        let current = spectrum[i];
        
        // 检查是否高于阈值
        if current < threshold_db {
            continue;
        }
        
        // 检查是否是局部最大值
        let mut is_peak = true;
        for j in i.saturating_sub(min_distance)..=(i + min_distance).min(spectrum.len() - 1) {
            if j != i && spectrum[j] >= current {
                is_peak = false;
                break;
            }
        }
        
        if is_peak {
            peaks.push(Peak {
                index: i,
                frequency_hz: frequencies[i],
                power_db: current,
            });
        }
    }
    
    peaks
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_window_generation() {
        let window = generate_window(10, WindowType::Hann);
        assert_eq!(window.len(), 10);
        
        // Hann 窗的首尾应接近 0
        assert!(window[0] < 0.01);
        assert!(window[9] < 0.01);
        
        // 中间应接近 1
        assert!(window[5] > 0.9);
    }
    
    #[test]
    fn test_frequency_axis() {
        let freqs = generate_frequency_axis(915_000_000, 2_000_000, 100);
        assert_eq!(freqs.len(), 100);
        
        // 检查频率范围
        assert!((freqs[0] - 914_000_000.0).abs() < 1.0);
        assert!((freqs[99] - 915_980_000.0).abs() < 1000.0);
    }
    
    #[test]
    fn test_spectrum_analyzer() {
        let config = SpectrumConfig::default();
        let mut analyzer = SpectrumAnalyzer::new(config);
        
        // 生成测试信号
        let samples: Vec<Complex<f32>> = (0..1024)
            .map(|i| {
                let t = i as f32 / 1024.0;
                Complex::new((2.0 * PI * 10.0 * t).cos(), 0.0)
            })
            .collect();
        
        let spectrum = analyzer.process(&samples);
        assert_eq!(spectrum.len(), 1024);
    }
}
