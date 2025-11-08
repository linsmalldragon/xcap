use fs_extra::dir;
use std::sync::Arc;
use std::time::{Duration, Instant};
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

#[tokio::main]
async fn main() {
    setup();
    let start = Instant::now();
    let monitors = Monitor::all().unwrap();

    dir::create_all("target/monitors", true).unwrap();

    let handles: Vec<_> = monitors
        .into_iter()
        .map(|monitor| {
            let monitor = Arc::new(monitor);
            let handle = tokio::spawn(async move {
                let monitor_name = normalized(monitor.name().unwrap());
                // 第一次捕获
                let capture_start = Instant::now();
                let monitor_clone = Arc::clone(&monitor);
                let image =
                    tokio::task::spawn_blocking(move || monitor_clone.capture_image().unwrap())
                        .await
                        .unwrap();
                println!(
                    "capture_image {monitor_name} (第1次) 耗时: {:?}",
                    capture_start.elapsed()
                );
                tokio::time::sleep(Duration::from_secs(2)).await;

                // 第二次捕获（应该复用流）
                let capture_start = Instant::now();
                let monitor_clone = Arc::clone(&monitor);
                let _image2 =
                    tokio::task::spawn_blocking(move || monitor_clone.capture_image().unwrap())
                        .await
                        .unwrap();
                println!(
                    "capture_image {monitor_name} (第2次) 耗时: {:?}",
                    capture_start.elapsed()
                );

                tokio::time::sleep(Duration::from_secs(2)).await;

                // 第三次捕获（应该复用流）
                let capture_start = Instant::now();
                let monitor_clone = Arc::clone(&monitor);
                let _image3 =
                    tokio::task::spawn_blocking(move || monitor_clone.capture_image().unwrap())
                        .await
                        .unwrap();
                println!(
                    "capture_image {monitor_name} (第3次) 耗时: {:?}",
                    capture_start.elapsed()
                );

                let image_clone = image.clone();

                image_clone
                    .save(format!("target/monitors/monitor-{}.png", monitor_name))
                    .unwrap();
            });
            handle
        })
        .collect();

    // 等待所有任务完成
    for handle in handles {
        handle.await.unwrap();
    }

    println!("运行耗时: {:?}", start.elapsed());
}
