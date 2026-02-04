/// 使用纯 libiio-sys 接收器的示例
/// 
/// 这个版本避免了 USB 接口冲突问题

use std::error::Error;
use pluto_rs::io::libiio_receiver::{LibiioReceiver, LibiioRxConfig, signal};

fn main() -> Result<(), Box<dyn Error>> {
    println!("=== Pluto SDR 接收器（无冲突版本）===\n");
    
    // 配置
    let config = LibiioRxConfig {
        center_freq_hz: 915_000_000,
        sample_rate_hz: 2_084_000,
        bandwidth_hz: 2_000_000,
        rx_gain_db: 50.0,
        buffer_size: 8192,
        gain_mode: "manual".to_string(),
    };
    
    // 创建接收器
    let mut receiver = LibiioReceiver::new(config);
    
    // 连接并配置
    receiver.connect()?;
    
    // 开始接收
    println!("开始接收信号...\n");
    println!("{:<10} {:<15} {:<15} {:<15}", "缓冲区", "样本数", "功率 (dB)", "平均幅度");
    println!("{}", "-".repeat(60));
    
    for i in 0..20 {
        let samples = receiver.receive_samples()?;
        
        // 计算信号统计
        let power_db = signal::calculate_power_db(&samples);
        let magnitudes = signal::calculate_magnitude(&samples);
        let avg_magnitude: f32 = magnitudes.iter().sum::<f32>() / magnitudes.len() as f32;
        
        println!("{:<10} {:<15} {:<15.2} {:<15.6}", 
            i + 1, 
            samples.len(), 
            power_db, 
            avg_magnitude
        );
        
        // 显示样本示例
        if i == 0 {
            println!("\n  前 5 个 IQ 样本:");
            for (idx, sample) in samples.iter().take(5).enumerate() {
                println!("    [{}] I={:8.4}, Q={:8.4}, 幅度={:8.4}", 
                    idx, sample.re, sample.im, sample.norm());
            }
            println!();
        }
        
        // 测试频率切换
        if i == 7 {
            println!("\n>>> 切换到 433 MHz...");
            receiver.set_frequency(433_000_000)?;
            println!();
        }
        
        // 测试增益调整
        if i == 14 {
            println!("\n>>> 调整增益到 60 dB...");
            receiver.set_gain(60.0)?;
            println!();
        }
        
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    
    println!("\n接收完成！");
    println!("\n✅ 所有功能正常工作：");
    println!("  - 真实 IQ 数据接收");
    println!("  - 动态频率调整");
    println!("  - 动态增益调整");
    println!("  - 无 USB 接口冲突");
    
    Ok(())
}
