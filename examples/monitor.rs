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

        // 使用序列号反查 from_unique_key
        if let Ok(serial) = monitor.serial_number() {
            if !serial.is_empty() {
                println!("\n=== 使用 from_unique_key 反查序列号: {} ===", serial);
                let lookup_start = Instant::now();
                match Monitor::from_unique_key(serial.clone()) {
                    Ok(found_monitor) => {
                        let lookup_duration = lookup_start.elapsed();
                        println!("✓ 成功找到显示器 (耗时: {:?}):", lookup_duration);
                        println!("  id: {}", found_monitor.id().unwrap());
                        println!("  name: {}", found_monitor.name().unwrap());
                        println!("  position: {:?}", (found_monitor.x().unwrap(), found_monitor.y().unwrap()));
                        println!("  size: {:?}", (found_monitor.width().unwrap(), found_monitor.height().unwrap()));
                    }
                    Err(e) => {
                        let lookup_duration = lookup_start.elapsed();
                        println!("✗ 查找失败 (耗时: {:?}): {}", lookup_duration, e);
                    }
                }
                println!();
            }
        }
    }

    let monitor = Monitor::from_point(100, 100).unwrap();

    println!("Monitor::from_point(): {:?}", monitor.name().unwrap());
    println!(
        "Monitor::from_point(100, 100) 运行耗时: {:?}",
        start.elapsed()
    );

    println!("运行耗时: {:?}", start.elapsed());
}
