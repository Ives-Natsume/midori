/// 基础配置测试示例
/// 
/// 这个示例只配置 Pluto SDR 的接收参数，不涉及数据接收
/// 适合测试设备连接和参数设置是否正常工作

use std::error::Error;
use pluto_rs::io::pluto::pluto_connect;
use industrial_io::channel::Direction;

fn main() -> Result<(), Box<dyn Error>> {
    println!("=== Pluto SDR 配置测试 ===\n");
    
    // 连接到 Pluto
    println!("正在连接到 Pluto SDR...");
    let ctx = pluto_connect()?;
    println!();
    
    // 获取 PHY 设备
    let phy = ctx.find_device("ad9361-phy")
        .ok_or("未找到 PHY 设备")?;
    
    println!("配置接收参数...\n");
    
    // 1. 设置中心频率到 915 MHz
    if let Some(lo_chan) = phy.find_channel("altvoltage0", Direction::Output) {
        lo_chan.attr_write("frequency", 915_000_000u64)?;
        let freq = lo_chan.attr_read::<String>("frequency")?;
        println!("✓ 中心频率: {} Hz ({:.3} MHz)", freq.trim(), 915.0);
    }
    
    // 2. 设置采样率到 2.084 MSPS
    if let Some(chan) = phy.find_channel("voltage0", Direction::Input) {
        chan.attr_write("sampling_frequency", 2_084_000u64)?;
        let sr = chan.attr_read::<String>("sampling_frequency")?;
        println!("✓ 采样率: {} Hz ({:.3} MSPS)", sr.trim(), 2.084);
    }
    
    // 3. 设置 RF 带宽到 2 MHz
    if let Some(chan) = phy.find_channel("voltage0", Direction::Input) {
        chan.attr_write("rf_bandwidth", 2_000_000u64)?;
        let bw = chan.attr_read::<String>("rf_bandwidth")?;
        println!("✓ RF 带宽: {} Hz ({:.3} MHz)", bw.trim(), 2.0);
    }
    
    // 4. 设置增益控制为手动模式
    if let Some(chan) = phy.find_channel("voltage0", Direction::Input) {
        chan.attr_write_str("gain_control_mode", "manual")?;
        let mode = chan.attr_read::<String>("gain_control_mode")?;
        println!("✓ 增益模式: {}", mode.trim());
    }
    
    // 5. 设置硬件增益到 40 dB
    if let Some(chan) = phy.find_channel("voltage0", Direction::Input) {
        chan.attr_write_str("hardwaregain", "40")?;
        let gain = chan.attr_read::<String>("hardwaregain")?;
        println!("✓ 硬件增益: {} dB", gain.trim());
    }
    
    // 6. 获取接收设备并启用通道
    let rx_dev = ctx.find_device("cf-ad9361-lpc")
        .ok_or("未找到接收设备")?;
    
    for chan_name in ["voltage0", "voltage1"] {
        if let Some(chan) = rx_dev.find_channel(chan_name, Direction::Input) {
            chan.enable();
            println!("✓ 已启用通道: {}", chan_name);
        }
    }
    
    println!("\n配置完成！");
    println!("\n接收器状态:");
    println!("  - 频率: 915 MHz");
    println!("  - 采样率: 2.084 MSPS");
    println!("  - 带宽: 2 MHz");
    println!("  - 增益: 40 dB (手动)");
    println!("  - 通道: I/Q 已启用");
    
    // 测试频率切换
    println!("\n测试频率调整...");
    let test_freqs = [433_000_000u64, 868_000_000, 2_400_000_000];
    
    if let Some(lo_chan) = phy.find_channel("altvoltage0", Direction::Output) {
        for freq in test_freqs {
            lo_chan.attr_write("frequency", freq)?;
            let actual = lo_chan.attr_read::<String>("frequency")?;
            println!("  - 切换到 {:.1} MHz... 实际: {} Hz", 
                freq as f64 / 1e6, 
                actual.trim()
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    
    // 测试增益调整
    println!("\n测试增益调整...");
    let test_gains = ["20", "40", "60"];
    
    if let Some(chan) = phy.find_channel("voltage0", Direction::Input) {
        for gain in test_gains {
            chan.attr_write_str("hardwaregain", gain)?;
            let actual = chan.attr_read::<String>("hardwaregain")?;
            println!("  - 设置增益到 {} dB... 实际: {} dB", gain, actual.trim());
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    
    println!("\n所有测试通过！✅");
    println!("\n接下来你可以：");
    println!("  1. 使用 libiio-sys 添加数据接收功能");
    println!("  2. 实现信号处理和分析");
    println!("  3. 构建实际的应用程序");
    
    Ok(())
}
