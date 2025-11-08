use fs_extra::dir;
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

    for monitor in monitors {
        // 第一次捕获
        let capture_start = Instant::now();
        let image = monitor.capture_image().await.unwrap();
        println!("capture_image (第1次) 耗时: {:?}", capture_start.elapsed());

        image
            .save(format!(
                "target/monitors/monitor-{}.png",
                normalized(monitor.name().unwrap())
            ))
            .unwrap();

        // 第二次捕获（应该复用流）
        let capture_start = Instant::now();
        let _image2 = monitor.capture_image().await.unwrap();
        println!("capture_image (第2次) 耗时: {:?}", capture_start.elapsed());

        // 第三次捕获（应该复用流）
        let capture_start = Instant::now();
        let _image3 = monitor.capture_image().await.unwrap();
        println!("capture_image (第3次) 耗时: {:?}", capture_start.elapsed());
    }

    println!("运行耗时: {:?}", start.elapsed());
}
