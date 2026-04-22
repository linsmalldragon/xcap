use std::{
    env::temp_dir, fs,
    sync::{Mutex, atomic::{AtomicBool, Ordering}},
};

use image::RgbaImage;
use scopeguard::defer;
use zbus::blocking::{Connection, Proxy};

use crate::error::XCapResult;

use super::screencast_capture::screencast_capture;
use super::utils::{get_zbus_connection, png_to_rgba_image};

static GNOME_SHELL_AVAILABLE: AtomicBool = AtomicBool::new(true);

fn org_gnome_shell_screenshot(
    conn: &Connection,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> XCapResult<RgbaImage> {
    let proxy = Proxy::new(
        conn,
        "org.gnome.Shell.Screenshot",
        "/org/gnome/Shell/Screenshot",
        "org.gnome.Shell.Screenshot",
    )?;

    let filename = rand::random::<u32>();

    let dirname = temp_dir().join("screenshot");
    fs::create_dir_all(&dirname)?;

    let mut path = dirname.join(filename.to_string());
    path.set_extension("png");
    defer!({
        let _ = fs::remove_file(&path);
    });

    let filename = path.to_string_lossy().to_string();

    // https://github.com/vinzenz/gnome-shell/blob/master/data/org.gnome.Shell.Screenshot.xml
    proxy.call_method("ScreenshotArea", &(x, y, width, height, false, &filename))?;

    let rgba_image = png_to_rgba_image(&filename, 0, 0, width, height)?;

    Ok(rgba_image)
}

static DBUS_LOCK: Mutex<()> = Mutex::new(());

fn wlroots_screenshot(
    x_coordinate: i32,
    y_coordinate: i32,
    width: i32,
    height: i32,
) -> XCapResult<RgbaImage> {
    let wayshot_connection = libwayshot_xcap::WayshotConnection::new()?;
    let capture_region = libwayshot_xcap::region::LogicalRegion {
        inner: libwayshot_xcap::region::Region {
            position: libwayshot_xcap::region::Position {
                x: x_coordinate,
                y: y_coordinate,
            },
            size: libwayshot_xcap::region::Size {
                width: width as u32,
                height: height as u32,
            },
        },
    };
    let rgba_image = wayshot_connection.screenshot(capture_region, false)?;

    // libwayshot returns image 0.24 RgbaImage
    // we need image 0.25 RgbaImage
    let image = image::RgbaImage::from_raw(
        rgba_image.width(),
        rgba_image.height(),
        rgba_image.to_rgba8().into_vec(),
    )
    .expect("Conversion of PNG -> Raw -> PNG does not fail");

    Ok(image)
}

pub fn wayland_capture(x: i32, y: i32, width: i32, height: i32) -> XCapResult<RgbaImage> {
    // Try GNOME Shell Screenshot first (only if not already known to be unavailable)
    if GNOME_SHELL_AVAILABLE.load(Ordering::Relaxed) {
        let lock = DBUS_LOCK.lock();
        let conn = get_zbus_connection()?;
        match org_gnome_shell_screenshot(conn, x, y, width, height) {
            Ok(img) => {
                drop(lock);
                return Ok(img);
            }
            Err(e) => {
                GNOME_SHELL_AVAILABLE.store(false, Ordering::Relaxed);
                log::info!("org.gnome.Shell.Screenshot unavailable ({e}), will use ScreenCast portal");
                drop(lock);
            }
        }
    }

    // Try ScreenCast portal (persistent session, only prompts once)
    screencast_capture(x, y, width, height)
        .or_else(|e| {
            log::debug!("ScreenCast capture failed: {e}, trying wlroots");
            wlroots_screenshot(x, y, width, height)
        })
}
#[test]
fn screnshot_multithreaded() {
    fn make_screenshots() {
        let monitors = crate::monitor::Monitor::all().unwrap();
        for monitor in monitors {
            monitor.capture_image().unwrap();
        }
    }
    // Try making screenshots in paralel. If this times out, then this means that there is a threading issue.
    const PARALELISM: usize = 10;
    let handles: Vec<_> = (0..PARALELISM)
        .map(|_| {
            std::thread::spawn(|| {
                make_screenshots();
            })
        })
        .collect();
    make_screenshots();
    handles
        .into_iter()
        .for_each(|handle| handle.join().unwrap());
}
