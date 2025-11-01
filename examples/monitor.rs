use std::time::Instant;
use xcap::Monitor;

fn main() {
    let start = Instant::now();
    let monitors = Monitor::all().unwrap();
    println!("Monitor::all() 运行耗时: {:?}", start.elapsed());

    for monitor in monitors {
        // 获取 UUID 和序列号（如果可用）
        let uuid_str = monitor.uuid()
            .map(|u| format!("UUID: {}", u))
            .unwrap_or_else(|_| "UUID: N/A".to_string());

        let serial_str = monitor.serial_number()
            .map(|s| format!("Serial: {}", s))
            .unwrap_or_else(|_| "Serial: N/A".to_string());

        println!(
            "Monitor:\n id: {}\n name: {}\n {}\n {}\n position: {:?}\n size: {:?}\n state:{:?}\n",
            monitor.id().unwrap(),
            monitor.name().unwrap(),
            uuid_str,
            serial_str,
            (monitor.x().unwrap(), monitor.y().unwrap()),
            (monitor.width().unwrap(), monitor.height().unwrap()),
            (
                monitor.rotation().unwrap(),
                monitor.scale_factor().unwrap(),
                monitor.frequency().unwrap(),
                monitor.is_primary().unwrap(),
                monitor.is_builtin().unwrap()
            )
        );
    }

    let monitor = Monitor::from_point(100, 100).unwrap();

    println!("Monitor::from_point(): {:?}", monitor.name().unwrap());
    println!(
        "Monitor::from_point(100, 100) 运行耗时: {:?}",
        start.elapsed()
    );

    println!("运行耗时: {:?}", start.elapsed());
}
