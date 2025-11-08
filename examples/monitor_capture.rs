use fs_extra::dir;
use std::time::Instant;
use xcap::Monitor;

fn normalized(filename: String) -> String {
    filename.replace(['|', '\\', ':', '/'], "")
}
fn setup() {
    // Initialize the logger with an info level filter
    if tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::default()
                .add_directive("debug".parse().unwrap())
                .add_directive("tokenizers=error".parse().unwrap()),
        )
        .try_init()
        .is_ok()
    {};
}

fn main() {
    setup();
    let start = Instant::now();
    let monitors = Monitor::all().unwrap();

    dir::create_all("target/monitors", true).unwrap();

    // 在同一线程中依次处理每个显示器
    for monitor in monitors {
        let monitor_name = normalized(monitor.name().unwrap());
        println!("开始处理显示器: {monitor_name}");

        // 第一次捕获（创建新流并缓存）
        let capture_start = Instant::now();
        let image = monitor.capture_image().unwrap();
        println!(
            "capture_image {monitor_name} (第1次) 耗时: {:?}",
            capture_start.elapsed()
        );

        // 第二次捕获（应该复用流）
        let capture_start = Instant::now();
        let _image2 = monitor.capture_image().unwrap();
        println!(
            "capture_image {monitor_name} (第2次) 耗时: {:?}",
            capture_start.elapsed()
        );

        // 第三次捕获（应该复用流）
        let capture_start = Instant::now();
        let _image3 = monitor.capture_image().unwrap();
        println!(
            "capture_image {monitor_name} (第3次) 耗时: {:?}",
            capture_start.elapsed()
        );

        // 保存第一张图片
        image
            .save(format!("target/monitors/monitor-{}.png", monitor_name))
            .unwrap();

        println!("完成处理显示器: {monitor_name}\n");
    }

    println!("运行耗时: {:?}", start.elapsed());
}
