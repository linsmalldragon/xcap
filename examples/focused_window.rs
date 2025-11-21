use std::thread;
use xcap::Window;

#[tokio::main]
async fn main() {
    thread::sleep(std::time::Duration::from_secs(3));

    //打印当前活动窗口的 app 名称
    let start = std::time::Instant::now();
    let app_name = Window::get_active_info().await.unwrap().0;
    let elapsed = start.elapsed();
    println!(
        "当前活动窗口的 app 名称: {:?}, 耗时: {:?}",
        app_name, elapsed
    );

    // 打印当前活动窗口的 app 名称和显示器序列号（高性能版本）
    let start = std::time::Instant::now();
    let (app_name, pid, display_serial) = Window::get_active_info().await.unwrap();
    let elapsed = start.elapsed();
    println!(
        "当前活动窗口的 app 名称: {:?}, pid: {:?}, 显示器序列号: {}, 耗时: {:?}",
        app_name, pid, display_serial, elapsed
    );

    let windows = Window::all().unwrap();
    windows.iter().filter(|w| w.is_focused().unwrap()).for_each(|focused| {
            println!(
                "Focused Window:\n id: {}\n title: {}\n app_name: {}\n monitor: {:?}\n position: {:?}\n size {:?}\n state {:?}\n",
                focused.id().unwrap(),
                focused.title().unwrap(),
                focused.app_name().unwrap(),
                focused.current_monitor().unwrap().name().unwrap(),
                (focused.x().unwrap(), focused.y().unwrap(), focused.z().unwrap()),
                (focused.width().unwrap(), focused.height().unwrap()),
                (focused.is_minimized().unwrap(), focused.is_maximized().unwrap(), focused.is_focused().unwrap())
            );
        });
}
