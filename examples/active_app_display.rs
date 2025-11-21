use std::thread;
use xcap::Window;

fn main() {
    println!("等待 3 秒，请切换到你想要检测的应用窗口...");
    thread::sleep(std::time::Duration::from_secs(3));

    // 使用高性能 API 获取当前活动应用名称和显示器序列号
    let start = std::time::Instant::now();
    match Window::get_active_info() {
        Ok((app_name, pid, display_serial)) => {
            let elapsed = start.elapsed();
            println!("\n当前活动应用信息:");
            println!("  应用名称: {}", app_name);
            println!("  进程 ID: {}", pid);
            println!("  显示器序列号: {}", display_serial);
            println!("  获取耗时: {:?}", elapsed);
        }
        Err(e) => {
            eprintln!("获取活动应用信息失败: {:?}", e);
        }
    }

    // 多次调用以展示性能稳定性
    println!("\n连续调用 10 次以测试性能:");
    let mut total_time = std::time::Duration::ZERO;

    for i in 1..=10 {
        let start = std::time::Instant::now();
        if let Ok((app_name, pid, display_serial)) = Window::get_active_info() {
            let elapsed = start.elapsed();
            total_time += elapsed;
            println!(
                "  第 {} 次: app={}, display_serial={}, 耗时={:?}",
                i, app_name, display_serial, elapsed
            );
        }
    }

    println!("\n平均耗时: {:?}", total_time / 10);
}
