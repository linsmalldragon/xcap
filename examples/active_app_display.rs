use std::time::Duration;

use objc2_foundation::{NSDate, NSRunLoop};
use tokio::time;
use xcap::Window;

fn run_main_run_loop_for(duration: Duration) {
    let run_loop = NSRunLoop::currentRunLoop();
    let target_date = NSDate::dateWithTimeIntervalSinceNow(duration.as_secs_f64());
    run_loop.runUntilDate(&target_date);
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    println!("等待主线程 RunLoop 运行，请切换到你想要检测的应用窗口...");
    run_main_run_loop_for(Duration::from_secs(3));

    // 在主线程中连续调用以测试性能
    println!("\n主线程连续调用 10 次以测试性能:");
    tokio::spawn(async move {
        let mut total_elapsed = std::time::Duration::ZERO;
        let mut success = 0;

        for attempt in 1..=10 {
            let start = std::time::Instant::now();
            match Window::get_active_info().await {
                Ok((app_name, pid, display_serial)) => {
                    let elapsed = start.elapsed();
                    total_elapsed += elapsed;
                    success += 1;
                    println!(
                        "  tokio 线程第 {} 次: app={}, display_serial={}, pid={}, 耗时={:?}",
                        attempt, app_name, display_serial, pid, elapsed
                    );
                }
                Err(e) => {
                    eprintln!("  tokio 线程第 {} 次获取活动应用信息失败: {:?}", attempt, e);
                }
            }
            time::sleep(Duration::from_secs(1)).await;
        }
        if success > 0 {
            println!("\n平均耗时: {:?}", total_elapsed / success as u32);
        } else {
            println!("\n全部失败，无法计算平均耗时");
        }
    });

    // 保持主线程运行，让 tokio 任务能够执行
    let wait_interval = Duration::from_millis(100);
    loop {
        run_main_run_loop_for(wait_interval);
        // 给 tokio 运行时一些时间处理任务
        tokio::task::yield_now().await;
    }
}
