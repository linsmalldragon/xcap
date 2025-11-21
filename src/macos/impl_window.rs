use std::{
    ffi::c_void,
    ptr::NonNull,
    sync::{Arc, OnceLock, RwLock, mpsc},
};

use block2::RcBlock;
use dispatch2::DispatchQueue;
use image::RgbaImage;
use objc2::{MainThreadMarker, rc::Retained, runtime::ProtocolObject};
use objc2_app_kit::{NSWorkspace, NSWorkspaceDidActivateApplicationNotification};
use objc2_core_foundation::{
    CFBoolean, CFDictionary, CFNumber, CFNumberType, CFRetained, CFString, CGPoint, CGRect,
};
use objc2_core_graphics::{
    CGDisplayBounds, CGMainDisplayID, CGRectContainsPoint, CGRectIntersectsRect,
    CGRectMakeWithDictionaryRepresentation, CGWindowListCopyWindowInfo, CGWindowListOption,
};

use objc2_foundation::{NSNotification, NSObjectProtocol};

use crate::{XCapError, error::XCapResult};

use super::{capture::capture, impl_monitor::ImplMonitor};

static ACTIVE_APP_TRACKER: OnceLock<Arc<ActiveAppTracker>> = OnceLock::new();

#[derive(Debug, Clone)]
pub(crate) struct ImplWindow {
    pub window_id: u32,
}

unsafe impl Send for ImplWindow {}

/// 活动应用信息
///
/// 存储当前处于前台的活动应用的名称、进程 ID 和所在显示器的序列号
#[derive(Clone)]
struct ActiveAppInfo {
    /// 应用名称
    name: String,
    /// 进程 ID
    pid: i32,
    /// 所在显示器的序列号
    display_serial: String,
}

/// 活动应用跟踪器
///
/// 用于跟踪当前处于前台的活动应用信息（名称、pid、显示序列号）。通过监听 macOS 的 NSWorkspaceDidActivateApplicationNotification
/// 通知，当用户切换应用时自动更新当前应用信息。
struct ActiveAppTracker {
    /// 当前活动应用的信息，使用 Arc<RwLock<>> 包装以支持多线程读取
    /// - Arc: 允许多个线程共享同一个信息的引用
    /// - RwLock: 允许多个线程同时读取，但写入时需要独占锁
    current_info: Arc<RwLock<ActiveAppInfo>>,
    /// 通知观察者的令牌，用于在对象销毁时自动移除观察者
    /// 使用下划线前缀表示这是一个仅用于保持生命周期的字段
    _observer_token: Retained<ProtocolObject<dyn NSObjectProtocol>>,
}

/// 为 ActiveAppTracker 实现 Send 和 Sync trait
///
/// 虽然 ActiveAppTracker 内部包含的 observer_token 不满足 Send/Sync，
/// 但该 tracker 只在主线程上创建和使用，observer_token 也只在主线程上访问。
/// 通过 Arc 在线程间共享时，其他线程只能读取 current_info，不会访问 observer_token，
/// 因此可以安全地标记为 Send + Sync。
unsafe impl Send for ActiveAppTracker {}
unsafe impl Sync for ActiveAppTracker {}

/// 确保活动应用跟踪器已被初始化（单例模式）
///
/// 这个函数实现了线程安全的单例模式：
/// 1. 首先检查全局静态变量 ACTIVE_APP_TRACKER 是否已经初始化
/// 2. 如果已初始化，直接返回现有的 tracker
/// 3. 如果未初始化，调用 init_active_app_tracker() 进行初始化
/// 4. 尝试将新创建的 tracker 设置到全局变量中
/// 5. 如果设置失败（说明其他线程已经先设置了），返回已存在的 tracker
///
/// 这样可以确保整个程序运行期间只有一个 ActiveAppTracker 实例，
/// 避免重复注册通知观察者。
fn ensure_active_app_tracker() -> XCapResult<Arc<ActiveAppTracker>> {
    // 尝试获取已存在的 tracker（如果已经初始化过）
    if let Some(tracker) = ACTIVE_APP_TRACKER.get() {
        return Ok(tracker.clone());
    }

    // 如果不存在，则初始化一个新的 tracker
    let tracker = init_active_app_tracker()?;

    // 尝试将新创建的 tracker 设置到全局变量中
    // 如果设置失败（is_err），说明其他线程已经先设置了
    if ACTIVE_APP_TRACKER.set(tracker.clone()).is_err() {
        // 返回其他线程已经设置好的 tracker
        if let Some(existing) = ACTIVE_APP_TRACKER.get() {
            return Ok(existing.clone());
        }
    }

    // 设置成功，返回新创建的 tracker
    Ok(tracker)
}

/// 初始化活动应用跟踪器
///
/// 这个函数负责创建 ActiveAppTracker 实例。由于 macOS 的通知观察者必须在主线程上注册，
/// 所以需要根据当前线程情况选择不同的初始化方式：
///
/// 1. 如果当前已经在主线程上：直接在主线程上创建 tracker
/// 2. 如果当前不在主线程上：通过 DispatchQueue 切换到主线程创建 tracker
///
/// 使用 mpsc::channel 来同步等待主线程上的初始化完成。
fn init_active_app_tracker() -> XCapResult<Arc<ActiveAppTracker>> {
    // 检查当前是否在主线程上
    if MainThreadMarker::new().is_some() {
        // 已经在主线程上，直接创建 tracker
        return ActiveAppTracker::new_observer_on_main_thread().map(Arc::new);
    }

    // 不在主线程上，需要切换到主线程
    // 创建一个通道用于接收主线程上的初始化结果
    let (tx, rx) = mpsc::channel();

    // 在主线程上异步执行初始化任务
    DispatchQueue::main().exec_async(move || {
        // 在主线程上创建 tracker
        let tracker = ActiveAppTracker::new_observer_on_main_thread().map(Arc::new);
        // 将结果发送回原线程（忽略发送失败的情况）
        let _ = tx.send(tracker);
    });

    // 阻塞等待主线程完成初始化并返回结果
    // 使用 ?? 是因为 rx.recv() 返回 Result<Result<Arc<ActiveAppTracker>, XCapError>, RecvError>
    // 第一个 ? 处理通道接收错误，第二个 ? 处理 tracker 创建错误
    let tracker = rx
        .recv()
        .map_err(|_| XCapError::new("Failed to initialize active app tracker"))??;

    Ok(tracker)
}

impl ActiveAppTracker {
    /// 在主线程上创建活动应用跟踪器
    ///
    /// 这个函数必须在主线程上调用，因为：
    /// 1. macOS 的 NSWorkspace API 必须在主线程上使用
    /// 2. 通知观察者的注册必须在主线程上进行
    ///
    /// 工作流程：
    /// 1. 验证当前在主线程上
    /// 2. 获取当前前台应用的信息（名称、pid、显示序列号）作为初始值
    /// 3. 创建一个闭包作为通知回调，当应用切换时更新应用信息
    /// 4. 注册 NSWorkspaceDidActivateApplicationNotification 通知观察者
    /// 5. 返回初始化好的 tracker
    fn new_observer_on_main_thread() -> XCapResult<Self> {
        // 双重检查：确保当前确实在主线程上
        if MainThreadMarker::new().is_none() {
            return Err(XCapError::new(
                "Active app tracker must be initialized on the main thread",
            ));
        }

        // 获取当前前台应用的信息作为初始值
        let (initial_name, initial_pid) = ImplWindow::get_app_name_pid()?;
        let initial_display_serial = ImplWindow::get_display_serial_by_pid(initial_pid)
            .unwrap_or_else(|_| "Unknown".to_string());

        let initial_info = ActiveAppInfo {
            name: initial_name,
            pid: initial_pid,
            display_serial: initial_display_serial,
        };

        // 将初始信息包装在 Arc<RwLock<>> 中，以便多线程共享和修改
        let current_info = Arc::new(RwLock::new(initial_info));
        // 克隆一份引用，用于在闭包中使用（闭包会 move 这个值）
        let shared_state = current_info.clone();

        // 创建通知回调闭包
        // 当用户切换应用时，macOS 会调用这个闭包
        let observer_block = RcBlock::new(move |_notification: NonNull<NSNotification>| {
            // 获取新的前台应用信息
            if let Ok((name, pid)) = ImplWindow::get_app_name_pid() {
                // 获取显示序列号，如果获取失败则使用默认值
                let display_serial = ImplWindow::get_display_serial_by_pid(pid)
                    .unwrap_or_else(|_| "Unknown".to_string());

                // 尝试获取写锁并更新应用信息
                // 如果获取锁失败（比如其他线程正在读取），则忽略本次更新
                if let Ok(mut guard) = shared_state.write() {
                    guard.name = name;
                    guard.pid = pid;
                    guard.display_serial = display_serial;
                }
            }
        });

        unsafe {
            // 获取 NSWorkspace 单例（macOS 系统级的工作空间管理器）
            let workspace = NSWorkspace::sharedWorkspace();
            // 获取通知中心（用于注册和接收系统通知）
            let notification_center = workspace.notificationCenter();

            // 注册应用激活通知的观察者
            // NSWorkspaceDidActivateApplicationNotification: 当用户切换到另一个应用时触发
            // 参数说明：
            //   - 第一个参数：要监听的通知名称
            //   - 第二个参数：要监听的对象（None 表示监听所有对象）
            //   - 第三个参数：接收通知的队列（None 表示在主线程上接收）
            //   - 第四个参数：通知触发时调用的闭包
            let observer_token = notification_center.addObserverForName_object_queue_usingBlock(
                Some(NSWorkspaceDidActivateApplicationNotification),
                None,
                None,
                &observer_block,
            );

            // 重要：使用 std::mem::forget 防止 observer_block 被释放
            //
            // 原因：通知中心会持有 observer_block 的引用，但 Rust 的借用检查器
            // 不知道这一点。如果不使用 forget，observer_block 会在函数结束时被释放，
            // 导致通知中心持有悬垂指针，程序崩溃。
            //
            // 使用 forget 后，observer_block 会一直存在直到程序结束或观察者被移除。
            // observer_token 会在 ActiveAppTracker 被销毁时自动移除观察者。
            std::mem::forget(observer_block);

            // 返回初始化好的 tracker
            Ok(Self {
                current_info,
                _observer_token: observer_token,
            })
        }
    }

    /// 读取当前活动应用的信息
    ///
    /// 这个函数是线程安全的，可以在任何线程上调用。
    /// 它从共享的 RwLock 中读取应用信息（名称、pid、显示序列号），如果读取锁被污染（poisoned），
    /// 说明之前有线程在持有写锁时发生了 panic，此时返回错误。
    ///
    /// 返回：(应用名称, 进程 ID, 显示序列号)
    fn read_active_info(&self) -> XCapResult<(String, i32, String)> {
        // 尝试获取读锁
        self.current_info
            .read()
            // 如果成功，克隆信息内容（因为 guard 会在离开作用域时释放锁）
            .map(|guard| (guard.name.clone(), guard.pid, guard.display_serial.clone()))
            // 如果获取锁失败（锁被污染），返回错误
            .map_err(|_| XCapError::new("Active app tracker state poisoned"))
    }
}

fn get_cf_dictionary_get_value(
    cf_dictionary: &CFDictionary,
    key: &str,
) -> XCapResult<*const c_void> {
    unsafe {
        let cf_dictionary_key = CFString::from_str(key);
        let cf_dictionary_key_ref = cf_dictionary_key.as_ref() as *const CFString;

        let value = cf_dictionary.value(cf_dictionary_key_ref.cast());

        if value.is_null() {
            return Err(XCapError::new(format!(
                "Get CFDictionary {} value failed",
                key
            )));
        }

        Ok(value)
    }
}

fn get_cf_number_i32_value(cf_dictionary: &CFDictionary, key: &str) -> XCapResult<i32> {
    unsafe {
        let cf_number = get_cf_dictionary_get_value(cf_dictionary, key)? as *const CFNumber;

        let mut value: i32 = 0;
        let is_success =
            (*cf_number).value(CFNumberType::IntType, &mut value as *mut _ as *mut c_void);

        if !is_success {
            return Err(XCapError::new(format!(
                "Get {} CFNumberGetValue failed",
                key
            )));
        }

        Ok(value)
    }
}

fn get_cf_string_value(cf_dictionary: &CFDictionary, key: &str) -> XCapResult<String> {
    let value_ref = get_cf_dictionary_get_value(cf_dictionary, key)? as *const CFString;
    let value = unsafe { (*value_ref).to_string() };
    Ok(value)
}

fn get_cf_bool_value(cf_dictionary: &CFDictionary, key: &str) -> XCapResult<bool> {
    let value_ref = get_cf_dictionary_get_value(cf_dictionary, key)? as *const CFBoolean;

    Ok(unsafe { (*value_ref).value() })
}

fn get_window_cg_rect(window_cf_dictionary: &CFDictionary) -> XCapResult<CGRect> {
    unsafe {
        let window_bounds = get_cf_dictionary_get_value(window_cf_dictionary, "kCGWindowBounds")?
            as *const CFDictionary;

        let mut cg_rect = CGRect::default();

        let is_success =
            CGRectMakeWithDictionaryRepresentation(Some(&*window_bounds), &mut cg_rect);

        if !is_success {
            return Err(XCapError::new(
                "CGRectMakeWithDictionaryRepresentation failed",
            ));
        }

        Ok(cg_rect)
    }
}

fn get_window_id(window_cf_dictionary: &CFDictionary) -> XCapResult<u32> {
    let window_name = get_cf_string_value(window_cf_dictionary, "kCGWindowName")?;

    let window_owner_name = get_cf_string_value(window_cf_dictionary, "kCGWindowOwnerName")?;

    if window_name.eq("StatusIndicator") && window_owner_name.eq("Window Server") {
        return Err(XCapError::new("Window is StatusIndicator"));
    }

    let window_sharing_state =
        get_cf_number_i32_value(window_cf_dictionary, "kCGWindowSharingState")?;

    if window_sharing_state == 0 {
        return Err(XCapError::new("Window sharing state is 0"));
    }

    let window_id = get_cf_number_i32_value(window_cf_dictionary, "kCGWindowNumber")?;

    Ok(window_id as u32)
}

pub fn get_window_cf_dictionary(window_id: u32) -> XCapResult<CFRetained<CFDictionary>> {
    unsafe {
        // CGWindowListCopyWindowInfo 返回窗口顺序为从顶层到最底层
        // 即在前面的窗口在数组前面
        let cf_array = match CGWindowListCopyWindowInfo(
            CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements,
            0,
        ) {
            Some(cf_array) => cf_array,
            None => return Err(XCapError::new("Get window info failed")),
        };

        let windows_count = cf_array.count();

        for i in 0..windows_count {
            let window_cf_dictionary_ref = cf_array.value_at_index(i) as *const CFDictionary;

            if window_cf_dictionary_ref.is_null() {
                continue;
            }
            let window_cf_dictionary = &*window_cf_dictionary_ref;

            let current_window_id = match get_window_id(window_cf_dictionary) {
                Ok(val) => val,
                Err(_) => continue,
            };

            if current_window_id == window_id {
                let s = CFDictionary::new_copy(None, Some(window_cf_dictionary)).unwrap();
                return Ok(s);
            }
        }

        Err(XCapError::new("Window not found"))
    }
}

impl ImplWindow {
    pub fn new(window_id: u32) -> ImplWindow {
        ImplWindow { window_id }
    }

    pub fn all() -> XCapResult<Vec<ImplWindow>> {
        unsafe {
            let mut impl_window = Vec::new();

            // CGWindowListCopyWindowInfo 返回窗口顺序为从顶层到最底层
            // 即在前面的窗口在数组前面
            let cf_array = match CGWindowListCopyWindowInfo(
                CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements,
                0,
            ) {
                Some(cf_array) => cf_array,
                None => return Ok(impl_window),
            };

            let windows_count = cf_array.count();

            for i in 0..windows_count {
                let window_cf_dictionary_ref = cf_array.value_at_index(i) as *const CFDictionary;

                if window_cf_dictionary_ref.is_null() {
                    continue;
                }

                let window_cf_dictionary = &*window_cf_dictionary_ref;

                let window_id = match get_window_id(window_cf_dictionary) {
                    Ok(window_id) => window_id,
                    Err(_) => continue,
                };

                impl_window.push(ImplWindow::new(window_id));
            }

            Ok(impl_window)
        }
    }
    /// 获取当前活动应用的信息
    ///
    /// 返回：(应用名称, 进程 ID, 显示序列号)
    pub fn get_active_info() -> XCapResult<(String, i32, String)> {
        let tracker = ensure_active_app_tracker()?;
        tracker.read_active_info()
    }

    /// 根据进程 ID 获取该进程窗口所在显示器的序列号
    ///
    /// 参数：
    /// - `pid`: 进程 ID
    ///
    /// 返回：显示器序列号（如果获取失败则返回显示器 ID 作为后备）
    ///
    /// 性能优化：
    /// - 不遍历所有窗口，只查找第一个匹配的窗口即停止
    /// - 窗口列表按 z-order 排序，第一个匹配的就是最前面的窗口
    fn get_display_serial_by_pid(pid: i32) -> XCapResult<String> {
        unsafe {
            // 获取窗口列表（按 z-order 排序，最前面的窗口在数组最前）
            let cf_array = CGWindowListCopyWindowInfo(
                CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements,
                0,
            )
            .ok_or_else(|| XCapError::new("Failed to get window list"))?;

            let windows_count = cf_array.count();

            // 查找第一个属于该进程的窗口
            for i in 0..windows_count {
                let window_cf_dictionary_ref = cf_array.value_at_index(i) as *const CFDictionary;

                if window_cf_dictionary_ref.is_null() {
                    continue;
                }

                let window_cf_dictionary = &*window_cf_dictionary_ref;

                // 检查窗口的所有者 PID
                let window_pid =
                    match get_cf_number_i32_value(window_cf_dictionary, "kCGWindowOwnerPID") {
                        Ok(pid) => pid,
                        Err(_) => continue,
                    };

                // 找到第一个属于该进程的窗口
                if window_pid == pid {
                    // 获取窗口位置
                    let cg_rect = match get_window_cg_rect(window_cf_dictionary) {
                        Ok(rect) => rect,
                        Err(_) => continue,
                    };

                    // 计算窗口中心点
                    let window_center_x = cg_rect.origin.x + cg_rect.size.width / 2.0;
                    let window_center_y = cg_rect.origin.y + cg_rect.size.height / 2.0;
                    let cg_point = CGPoint {
                        x: window_center_x,
                        y: window_center_y,
                    };

                    // 获取所有显示器并找到包含窗口中心点的显示器
                    let impl_monitors = ImplMonitor::all()?;
                    let primary_monitor = ImplMonitor::new(CGMainDisplayID());

                    let monitor = impl_monitors
                        .iter()
                        .find(|impl_monitor| {
                            let display_bounds = CGDisplayBounds(impl_monitor.cg_direct_display_id);
                            CGRectContainsPoint(display_bounds, cg_point)
                                || CGRectIntersectsRect(display_bounds, cg_rect)
                        })
                        .unwrap_or(&primary_monitor);

                    // 获取显示器序列号，如果获取失败则使用 display_id 作为后备
                    let display_serial = monitor
                        .serial_number()
                        .unwrap_or_else(|_| monitor.cg_direct_display_id.to_string());

                    return Ok(display_serial);
                }
            }

            // 如果没有找到窗口，返回主显示器序列号
            let primary_monitor = ImplMonitor::new(CGMainDisplayID());
            let display_serial = primary_monitor
                .serial_number()
                .unwrap_or_else(|_| CGMainDisplayID().to_string());
            Ok(display_serial)
        }
    }

    /// 获取前台应用的名称和所在显示器的序列号
    ///
    /// 返回：(应用名称, 显示器序列号)
    ///
    /// 性能优化：
    /// - 不遍历所有窗口，只查找第一个匹配的窗口即停止
    /// - 窗口列表按 z-order 排序，第一个匹配的就是最前面的激活窗口
    pub fn get_app_name_pid() -> XCapResult<(String, i32)> {
        unsafe {
            let workspace = NSWorkspace::sharedWorkspace();
            let frontmost_app = workspace
                .frontmostApplication()
                .ok_or_else(|| XCapError::new("Failed to get frontmost application"))?;

            let app_name = frontmost_app
                .localizedName()
                .map(|name| name.to_string())
                .unwrap_or_else(|| "Unknown".to_string());

            // 获取前台应用的 PID
            let frontmost_pid = frontmost_app.processIdentifier() as i32;

            Ok((app_name, frontmost_pid))
        }
    }
}

impl ImplWindow {
    pub fn id(&self) -> XCapResult<u32> {
        Ok(self.window_id)
    }

    pub fn pid(&self) -> XCapResult<u32> {
        let window_cf_dictionary = get_window_cf_dictionary(self.window_id)?;

        let pid = get_cf_number_i32_value(window_cf_dictionary.as_ref(), "kCGWindowOwnerPID")?;

        Ok(pid as u32)
    }

    pub fn app_name(&self) -> XCapResult<String> {
        let window_cf_dictionary = get_window_cf_dictionary(self.window_id)?;

        get_cf_string_value(window_cf_dictionary.as_ref(), "kCGWindowOwnerName")
    }

    pub fn title(&self) -> XCapResult<String> {
        let window_cf_dictionary = get_window_cf_dictionary(self.window_id)?;

        get_cf_string_value(window_cf_dictionary.as_ref(), "kCGWindowName")
    }

    pub fn current_monitor(&self) -> XCapResult<ImplMonitor> {
        let window_cf_dictionary = get_window_cf_dictionary(self.window_id)?;
        let cg_rect = get_window_cg_rect(window_cf_dictionary.as_ref())?;

        // 获取窗口中心点的坐标
        let window_center_x = cg_rect.origin.x + cg_rect.size.width / 2.0;
        let window_center_y = cg_rect.origin.y + cg_rect.size.height / 2.0;
        let cg_point = CGPoint {
            x: window_center_x,
            y: window_center_y,
        };

        let impl_monitors = ImplMonitor::all()?;
        let primary_monitor = ImplMonitor::new(unsafe { CGMainDisplayID() });

        let impl_monitor = impl_monitors
            .iter()
            .find(|impl_monitor| unsafe {
                let display_bounds = CGDisplayBounds(impl_monitor.cg_direct_display_id);
                CGRectContainsPoint(display_bounds, cg_point)
                    || CGRectIntersectsRect(display_bounds, cg_rect)
            })
            .unwrap_or(&primary_monitor);

        Ok(impl_monitor.to_owned())
    }

    pub fn x(&self) -> XCapResult<i32> {
        let window_cf_dictionary = get_window_cf_dictionary(self.window_id)?;

        let cg_rect = get_window_cg_rect(window_cf_dictionary.as_ref())?;

        Ok(cg_rect.origin.x as i32)
    }

    pub fn y(&self) -> XCapResult<i32> {
        let window_cf_dictionary = get_window_cf_dictionary(self.window_id)?;

        let cg_rect = get_window_cg_rect(window_cf_dictionary.as_ref())?;

        Ok(cg_rect.origin.y as i32)
    }

    pub fn z(&self) -> XCapResult<i32> {
        unsafe {
            // CGWindowListCopyWindowInfo 返回窗口顺序为从顶层到最底层
            // 即在前面的窗口在数组前面
            let cf_array = match CGWindowListCopyWindowInfo(
                CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements,
                0,
            ) {
                Some(cf_array) => cf_array,
                None => return Err(XCapError::new("Get window list failed")),
            };

            let windows_count = cf_array.count();
            let mut z = windows_count as i32;

            for i in 0..windows_count {
                z -= 1;
                let window_cf_dictionary_ref = cf_array.value_at_index(i) as *const CFDictionary;

                if window_cf_dictionary_ref.is_null() {
                    continue;
                }

                let window_cf_dictionary = &*window_cf_dictionary_ref;

                let window_id = match get_window_id(window_cf_dictionary) {
                    Ok(window_id) => window_id,
                    Err(_) => continue,
                };

                if window_id == self.window_id {
                    break;
                }
            }

            Ok(z)
        }
    }

    pub fn width(&self) -> XCapResult<u32> {
        let window_cf_dictionary = get_window_cf_dictionary(self.window_id)?;

        let cg_rect = get_window_cg_rect(window_cf_dictionary.as_ref())?;

        Ok(cg_rect.size.width as u32)
    }

    pub fn height(&self) -> XCapResult<u32> {
        let window_cf_dictionary = get_window_cf_dictionary(self.window_id)?;

        let cg_rect = get_window_cg_rect(window_cf_dictionary.as_ref())?;

        Ok(cg_rect.size.height as u32)
    }

    pub fn is_minimized(&self) -> XCapResult<bool> {
        let window_cf_dictionary = get_window_cf_dictionary(self.window_id)?;
        let is_on_screen = get_cf_bool_value(window_cf_dictionary.as_ref(), "kCGWindowIsOnscreen")?;
        let is_maximized = self.is_maximized()?;

        Ok(!is_on_screen && !is_maximized)
    }

    pub fn is_maximized(&self) -> XCapResult<bool> {
        let window_cf_dictionary = get_window_cf_dictionary(self.window_id)?;

        let cg_rect = get_window_cg_rect(window_cf_dictionary.as_ref())?;
        let impl_monitor = self.current_monitor()?;
        let impl_monitor_width = impl_monitor.width()?;
        let impl_monitor_height = impl_monitor.height()?;

        let is_maximized = {
            cg_rect.size.width as u32 >= impl_monitor_width
                && cg_rect.size.height as u32 >= impl_monitor_height
        };

        Ok(is_maximized)
    }

    pub fn is_focused(&self) -> XCapResult<bool> {
        unsafe {
            let workspace = NSWorkspace::sharedWorkspace();

            // Use frontmostApplication instead of deprecated activeApplication
            let frontmost_app = match workspace.frontmostApplication() {
                Some(app) => app,
                None => return Ok(false),
            };

            let active_app_pid = frontmost_app.processIdentifier() as u32;

            if active_app_pid == self.pid().ok().unwrap_or(0) {
                return Ok(true);
            }

            Ok(false)
        }
    }

    pub fn capture_image(&self) -> XCapResult<RgbaImage> {
        let window_cf_dictionary = get_window_cf_dictionary(self.window_id)?;

        let cg_rect = get_window_cg_rect(window_cf_dictionary.as_ref())?;

        capture(
            cg_rect,
            CGWindowListOption::OptionIncludingWindow,
            self.window_id,
            None, // 窗口捕获不需要 display_id
        )
    }
}
