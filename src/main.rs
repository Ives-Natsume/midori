use std::error::Error;
use pluto_rs::io::libiio_receiver::{LibiioReceiver, LibiioRxConfig, signal};

fn main() -> Result<(), Box<dyn Error>> {
    println!("=== Pluto SDR 接收器示例 ===\n");
    
    // 1. 配置接收参数
    let config = LibiioRxConfig {
        center_freq_hz: 915_000_000,    // 915 MHz (ISM 频段)
        sample_rate_hz: 2_084_000,      // 2.084 MSPS
        bandwidth_hz: 2_000_000,        // 2 MHz
        rx_gain_db: 40.0,               // 40 dB
        buffer_size: 16384,             // 16K 样本
        gain_mode: "manual".to_string(), // 手动增益控制
    };
    
    // 2. 创建并连接到 Pluto SDR
    println!("正在连接到 Pluto SDR...");
    let mut receiver = LibiioReceiver::new(config);
    receiver.connect()?;
    println!("\n接收器配置完成！\n");
    
    // 3. 接收信号数据
    println!("开始接收信号...");
    println!("{:<10} {:<15} {:<15} {:<15}", "缓冲区", "样本数", "功率 (dB)", "平均幅度");
    println!("{}", "-".repeat(60));
    
    for i in 0..20 {
        // 接收一个缓冲区的样本
        let samples = receiver.receive_samples()?;
        
        // 计算信号统计信息
        let power_db = signal::calculate_power_db(&samples);
        let magnitudes = signal::calculate_magnitude(&samples);
        let avg_magnitude: f32 = magnitudes.iter().sum::<f32>() / magnitudes.len() as f32;
        
        // 显示信息
        println!("{:<10} {:<15} {:<15.2} {:<15.4}", 
            i + 1, 
            samples.len(), 
            power_db, 
            avg_magnitude
        );
        
        // 每 5 个缓冲区调整一次频率作为示例
        if i == 5 {
            println!("\n>>> 切换到 433 MHz...");
            receiver.set_frequency(433_000_000)?;
            println!();
        }
        
        // 每 10 个缓冲区调整一次增益作为示例
        if i == 10 {
            println!("\n>>> 调整增益到 60 dB...");
            receiver.set_gain(60.0)?;
            println!();
        }
    }
    
    println!("\n接收完成！");
    
    Ok(())
}
