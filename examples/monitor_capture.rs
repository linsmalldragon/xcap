use fs_extra::dir;
use std::sync::Arc;
use std::time::Instant;
use xcap::Monitor;

fn normalized(filename: String) -> String {
    filename.replace(['|', '\\', ':', '/'], "")
}

#[tokio::main]
async fn main() {
    let start = Instant::now();
    let monitors = Monitor::all().unwrap();

    dir::create_all("target/monitors", true).unwrap();

    let handles: Vec<_> = monitors
        .into_iter()
        .map(|monitor| {
            let monitor = Arc::new(monitor);
            tokio::spawn(async move {
                // 第一次捕获
                let capture_start = Instant::now();
                let monitor_clone = Arc::clone(&monitor);
                let image =
                    tokio::task::spawn_blocking(move || monitor_clone.capture_image().unwrap())
                        .await
                        .unwrap();
                println!("capture_image (第1次) 耗时: {:?}", capture_start.elapsed());

                let image_clone = image.clone();
                let monitor_name = normalized(monitor.name().unwrap());
                tokio::task::spawn_blocking(move || {
                    image_clone
                        .save(format!("target/monitors/monitor-{}.png", monitor_name))
                        .unwrap();
                })
                .await
                .unwrap();

                // 第二次捕获（应该复用流）
                let capture_start = Instant::now();
                let monitor_clone = Arc::clone(&monitor);
                let _image2 =
                    tokio::task::spawn_blocking(move || monitor_clone.capture_image().unwrap())
                        .await
                        .unwrap();
                println!("capture_image (第2次) 耗时: {:?}", capture_start.elapsed());

                // 第三次捕获（应该复用流）
                let capture_start = Instant::now();
                let monitor_clone = Arc::clone(&monitor);
                let _image3 =
                    tokio::task::spawn_blocking(move || monitor_clone.capture_image().unwrap())
                        .await
                        .unwrap();
                println!("capture_image (第3次) 耗时: {:?}", capture_start.elapsed());
            })
        })
        .collect();

    // 等待所有任务完成
    for handle in handles {
        handle.await.unwrap();
    }

    println!("运行耗时: {:?}", start.elapsed());
}
