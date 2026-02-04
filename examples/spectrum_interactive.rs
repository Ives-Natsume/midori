/// 可交互的频谱服务器
/// 
/// 支持运行时动态调整频率和增益，适合卫星跟踪等应用
/// 
/// 使用方法:
/// ```
/// cargo run --example spectrum_interactive
/// ```
/// 
/// 交互命令：
/// - f <MHz>  : 设置中心频率（例如：f 144.0）
/// - g <dB>   : 设置增益（例如：g 60）
/// - s        : 显示当前状态
/// - q        : 退出

use std::error::Error;
use std::net::{TcpListener, TcpStream};
use std::io::{self, Write, BufRead, BufReader};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use pluto_rs::io::libiio_receiver::{LibiioReceiver, LibiioRxConfig, signal};
use pluto_rs::io::spectrum::{SpectrumAnalyzer, SpectrumConfig, WindowType, generate_frequency_axis};
use serde_json::json;

// 共享的配置状态
struct SharedConfig {
    center_freq_hz: u64,
    sample_rate_hz: u64,
    rx_gain_db: f64,
    freq_changed: bool,  // 标记频率是否改变
}

fn main() -> Result<(), Box<dyn Error>> {
    println!("=== Pluto SDR 交互式频谱服务器 ===\n");
    
    // 1. 初始配置
    let rx_config = LibiioRxConfig {
        center_freq_hz: 144_000_000,    // 144 MHz
        sample_rate_hz: 2_084_000,      // 2.084 MSPS
        bandwidth_hz: 2_000_000,        // 2 MHz
        rx_gain_db: 50.0,               // 50 dB
        buffer_size: 2048,
        gain_mode: "manual".to_string(),
    };
    
    // 共享配置（用于动态调整）
    let shared_config = Arc::new(Mutex::new(SharedConfig {
        center_freq_hz: rx_config.center_freq_hz,
        sample_rate_hz: rx_config.sample_rate_hz,
        rx_gain_db: rx_config.rx_gain_db,
        freq_changed: false,
    }));
    
    let spectrum_config = SpectrumConfig {
        fft_size: 1024,
        window_type: WindowType::Hann,
        averaging: 4,
    };
    
    // 2. 启动 TCP 服务器
    println!("启动频谱数据服务器 localhost:5555...");
    let listener = TcpListener::bind("127.0.0.1:5555")?;
    listener.set_nonblocking(true)?;
    
    println!("启动控制命令服务器 localhost:5556...");
    let control_listener = TcpListener::bind("127.0.0.1:5556")?;
    control_listener.set_nonblocking(true)?;
    
    println!("\n可视化: python app.py --mode spectrum");
    println!("或直接在此终端输入命令\n");
    
    // 3. 连接到 Pluto SDR
    println!("连接到 Pluto SDR...");
    let mut receiver = LibiioReceiver::new(rx_config);
    receiver.connect()?;
    println!("✓ Pluto SDR 已连接\n");
    
    // 4. 打印帮助信息
    print_help();
    
    // 5. 创建频谱分析器
    let mut analyzer = SpectrumAnalyzer::new(spectrum_config);
    
    // 客户端连接列表
    let clients: Arc<Mutex<Vec<TcpStream>>> = Arc::new(Mutex::new(Vec::new()));
    let clients_clone = clients.clone();
    
    // 控制客户端列表
    let control_clients: Arc<Mutex<Vec<TcpStream>>> = Arc::new(Mutex::new(Vec::new()));
    let control_clients_clone = control_clients.clone();
    
    // 6. 接受频谱数据连接的线程
    thread::spawn(move || {
        loop {
            match listener.accept() {
                Ok((stream, addr)) => {
                    println!("✓ 新的频谱客户端: {}", addr);
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
    
    // 7. 接受控制连接的线程
    let config_for_control = shared_config.clone();
    thread::spawn(move || {
        loop {
            match control_listener.accept() {
                Ok((stream, addr)) => {
                    println!("✓ 新的控制客户端: {}", addr);
                    stream.set_nonblocking(false).ok();
                    control_clients_clone.lock().unwrap().push(stream);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(100));
                }
                Err(e) => eprintln!("控制连接错误: {}", e),
            }
            
            // 处理来自控制客户端的命令
            let mut control_clients_lock = control_clients_clone.lock().unwrap();
            control_clients_lock.retain_mut(|client| {
                let mut reader = BufReader::new(client.try_clone().unwrap());
                let mut line = String::new();
                
                match reader.read_line(&mut line) {
                    Ok(0) => return false,  // 连接关闭
                    Ok(_) => {
                        let response = process_command(&line.trim(), &config_for_control);
                        let _ = writeln!(client, "{}", response);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => return false,
                }
                true
            });
            
            thread::sleep(Duration::from_millis(100));
        }
    });
    
    // 8. 标准输入命令处理线程
    let config_for_stdin = shared_config.clone();
    thread::spawn(move || {
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            if let Ok(cmd) = line {
                let response = process_command(&cmd.trim(), &config_for_stdin);
                println!("{}", response);
                
                if cmd.trim() == "q" {
                    std::process::exit(0);
                }
            }
        }
    });
    
    // 9. 主循环：接收数据并发送频谱
    println!("开始处理数据...\n");
    let mut buffer_count = 0;
    
    loop {
        // 检查是否需要更新频率
        {
            let mut config = shared_config.lock().unwrap();
            if config.freq_changed {
                println!("\n⚡ 正在更新频率到 {:.3} MHz...", 
                    config.center_freq_hz as f64 / 1e6);
                
                if let Err(e) = receiver.set_frequency(config.center_freq_hz) {
                    eprintln!("频率更新失败: {}", e);
                } else {
                    println!("✓ 频率已更新\n");
                }
                
                config.freq_changed = false;
            }
        }
        
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
        spectrum = analyzer.fftshift(spectrum);
        
        // 获取当前配置
        let (center_freq, sample_rate) = {
            let config = shared_config.lock().unwrap();
            (config.center_freq_hz, config.sample_rate_hz)
        };
        
        // 生成频率轴
        let frequencies = generate_frequency_axis(center_freq, sample_rate, 1024);
        let freq_mhz: Vec<f64> = frequencies.iter().map(|f| f / 1e6).collect();
        
        // 构建 JSON 数据
        let data = json!({
            "spectrum": spectrum,
            "frequencies": freq_mhz,
            "center_freq_hz": center_freq,
            "sample_rate_hz": sample_rate,
            "buffer_count": buffer_count,
        });
        
        let json_str = data.to_string();
        let json_bytes = json_str.as_bytes();
        
        // 发送给所有频谱客户端
        let mut clients_lock = clients.lock().unwrap();
        clients_lock.retain_mut(|client| {
            let length = json_bytes.len() as u32;
            if client.write_all(&length.to_le_bytes()).is_err() {
                return false;
            }
            if client.write_all(json_bytes).is_err() {
                return false;
            }
            true
        });
        
        buffer_count += 1;
        
        // 每 30 个缓冲区打印一次状态
        if buffer_count % 30 == 0 {
            let power_db = signal::calculate_power_db(&samples);
            let config = shared_config.lock().unwrap();
            println!(
                "[{}] {:.3} MHz | 功率 {:.1} dB | 客户端 {}",
                buffer_count,
                config.center_freq_hz as f64 / 1e6,
                power_db,
                clients_lock.len()
            );
        }
        
        thread::sleep(Duration::from_millis(33));
    }
}

fn print_help() {
    println!("交互命令：");
    println!("  f <MHz>  - 设置中心频率，例如：f 145.8");
    println!("  g <dB>   - 设置增益，例如：g 60");
    println!("  s        - 显示当前状态");
    println!("  h        - 显示帮助");
    println!("  q        - 退出程序");
    println!();
}

fn process_command(cmd: &str, config: &Arc<Mutex<SharedConfig>>) -> String {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    
    if parts.is_empty() {
        return String::new();
    }
    
    match parts[0] {
        "f" | "freq" => {
            if parts.len() < 2 {
                return "用法: f <MHz>".to_string();
            }
            
            match parts[1].parse::<f64>() {
                Ok(freq_mhz) => {
                    let freq_hz = (freq_mhz * 1e6) as u64;
                    
                    // 检查频率范围 (70 MHz - 6 GHz)
                    if freq_hz < 70_000_000 || freq_hz > 6_000_000_000 {
                        return format!("错误: 频率必须在 70-6000 MHz 范围内");
                    }
                    
                    let mut cfg = config.lock().unwrap();
                    cfg.center_freq_hz = freq_hz;
                    cfg.freq_changed = true;
                    
                    format!("✓ 频率设置为 {:.3} MHz", freq_mhz)
                }
                Err(_) => format!("错误: 无效的频率值 '{}'", parts[1]),
            }
        }
        
        "g" | "gain" => {
            if parts.len() < 2 {
                return "用法: g <dB>".to_string();
            }
            
            match parts[1].parse::<f64>() {
                Ok(gain_db) => {
                    if gain_db < -3.0 || gain_db > 71.0 {
                        return format!("错误: 增益必须在 -3 到 71 dB 范围内");
                    }
                    
                    let mut cfg = config.lock().unwrap();
                    cfg.rx_gain_db = gain_db;
                    
                    format!("✓ 增益设置为 {:.1} dB (下次生效)", gain_db)
                }
                Err(_) => format!("错误: 无效的增益值 '{}'", parts[1]),
            }
        }
        
        "s" | "status" => {
            let cfg = config.lock().unwrap();
            format!(
                "状态:\n  中心频率: {:.3} MHz\n  采样率: {:.3} MSPS\n  增益: {:.1} dB",
                cfg.center_freq_hz as f64 / 1e6,
                cfg.sample_rate_hz as f64 / 1e6,
                cfg.rx_gain_db
            )
        }
        
        "h" | "help" => {
            let help = "命令列表:\n\
                f <MHz>  - 设置中心频率\n\
                g <dB>   - 设置增益\n\
                s        - 显示状态\n\
                h        - 帮助\n\
                q        - 退出";
            help.to_string()
        }
        
        "q" | "quit" | "exit" => {
            "正在退出...".to_string()
        }
        
        _ => format!("未知命令: '{}' (输入 h 查看帮助)", parts[0]),
    }
}
