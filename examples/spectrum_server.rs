/// 频谱服务器示例
/// 
/// 接收 Pluto SDR 数据，计算 FFT，通过 TCP 发送给 Python 前端
/// 
/// 使用方法:
/// ```
/// cargo run --example spectrum_server
/// ```
/// 
/// 然后在另一个终端运行:
/// ```
/// python app.py --mode spectrum
/// ```

use std::error::Error;
use std::net::{TcpListener, TcpStream};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use pluto_rs::io::libiio_receiver::{LibiioReceiver, LibiioRxConfig, signal};
use pluto_rs::io::spectrum::{SpectrumAnalyzer, SpectrumConfig, WindowType, generate_frequency_axis};
use serde_json::json;

fn main() -> Result<(), Box<dyn Error>> {
    println!("=== Pluto SDR 频谱服务器 ===\n");
    
    // 1. 配置接收器
    let rx_config = LibiioRxConfig {
        center_freq_hz: 146_500_000,    // 144 MHz
        sample_rate_hz: 2_084_000,      // 2.084 MSPS
        bandwidth_hz: 2_000_000,        // 2 MHz
        rx_gain_db: 50.0,               // 50 dB
        buffer_size: 2048,              // 2048 样本用于 FFT
        gain_mode: "manual".to_string(),
    };
    
    // 2. 配置频谱分析器
    let spectrum_config = SpectrumConfig {
        fft_size: 1024,
        window_type: WindowType::Hann,
        averaging: 4,  // 4 次平均
    };
    
    // 3. 启动 TCP 服务器
    println!("启动 TCP 服务器 localhost:5555...");
    let listener = TcpListener::bind("127.0.0.1:5555")?;
    listener.set_nonblocking(true)?;
    
    println!("等待 Python 客户端连接...");
    println!("运行: python app.py --mode spectrum\n");
    
    // 4. 连接到 Pluto SDR
    println!("连接到 Pluto SDR...");
    let mut receiver = LibiioReceiver::new(rx_config.clone());
    receiver.connect()?;
    println!("✓ Pluto SDR 已连接\n");
    
    // 5. 创建频谱分析器
    let mut analyzer = SpectrumAnalyzer::new(spectrum_config);
    
    // 生成频率轴
    let frequencies = generate_frequency_axis(
        rx_config.center_freq_hz,
        rx_config.sample_rate_hz,
        1024,
    );
    
    // 频率转换为 MHz（用于显示）
    let freq_mhz: Vec<f64> = frequencies.iter().map(|f| f / 1e6).collect();
    
    // 客户端连接列表
    let clients: Arc<Mutex<Vec<TcpStream>>> = Arc::new(Mutex::new(Vec::new()));
    let clients_clone = clients.clone();
    
    // 6. 接受连接的线程
    thread::spawn(move || {
        loop {
            match listener.accept() {
                Ok((stream, addr)) => {
                    println!("✓ 新客户端连接: {}", addr);
                    stream.set_nonblocking(false).ok();
                    clients_clone.lock().unwrap().push(stream);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(100));
                }
                Err(e) => eprintln!("连接错误: {}", e),
            }
        }
    });
    
    // 7. 主循环：接收数据并发送频谱
    println!("开始处理数据...");
    let mut buffer_count = 0;
    
    loop {
        // 接收样本
        let samples = match receiver.receive_samples() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("接收错误: {}", e);
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        
        // 计算频谱
        let mut spectrum = analyzer.process(&samples);
        
        // FFT 移位（DC 移到中心）
        spectrum = analyzer.fftshift(spectrum);
        
        // 构建 JSON 数据
        let data = json!({
            "spectrum": spectrum,
            "frequencies": freq_mhz,
            "center_freq_hz": rx_config.center_freq_hz,
            "sample_rate_hz": rx_config.sample_rate_hz,
            "buffer_count": buffer_count,
        });
        
        let json_str = data.to_string();
        let json_bytes = json_str.as_bytes();
        
        // 发送给所有连接的客户端
        let mut clients_lock = clients.lock().unwrap();
        clients_lock.retain_mut(|client| {
            // 发送数据长度（4字节小端序）
            let length = json_bytes.len() as u32;
            if client.write_all(&length.to_le_bytes()).is_err() {
                return false;  // 移除断开的客户端
            }
            
            // 发送实际数据
            if client.write_all(json_bytes).is_err() {
                return false;
            }
            
            true  // 保留连接的客户端
        });
        
        buffer_count += 1;
        
        // 每 10 个缓冲区打印一次状态
        if buffer_count % 10 == 0 {
            let power_db = signal::calculate_power_db(&samples);
            println!(
                "缓冲区 {}: 功率 {:.1} dB, 客户端数: {}",
                buffer_count,
                power_db,
                clients_lock.len()
            );
        }
        
        // 控制更新率（约 30 FPS）
        thread::sleep(Duration::from_millis(33));
    }
}
