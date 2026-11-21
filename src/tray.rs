//! Tray application: battery icon, event-driven per-game onboard profiles,
//! and low-battery notifications. Pure Win32, zero global input hooks.
//! Started by `freenapse.exe` when it is launched without arguments.

#![allow(non_snake_case, unsafe_op_in_unsafe_fn)]

use std::collections::{BTreeMap, VecDeque};
use std::ffi::c_void;
use std::mem;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::{CreateMutexW, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
use windows_sys::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows_sys::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::config::Config;
use crate::device::{Battery, ButtonFunction, DeviceSpec, DpiStages, Mouse};
use crate::hid::HidError;
use crate::{apply, config_path, ui, user_active};

const WM_TRAY: u32 = WM_APP + 1;
/// Posted by a second instance (or `freenapse ui`) to open the settings window.
const WM_OPEN_SETTINGS: u32 = WM_APP + 5;
const CLASS_NAME: &str = "freenapse-tray";
const TIMER_POLL: usize = 1;
const TIMER_GAME: usize = 2;
const TIMER_RETRY: usize = 3;
/// One shot after a HID device arrived or the PC resumed: reopen the mouse.
const TIMER_REPLUG: usize = 4;
/// One shot after the config file changed on disk (editors write in several steps).
const TIMER_CONFIG: usize = 5;
/// The daily update check.
const TIMER_UPDATE: usize = 6;
/// Retries adding the tray icon until the taskbar takes it (at login it is often not ready yet).
const TIMER_ICON: usize = 7;
/// Posted by the config watcher thread when something in the config folder changed.
const WM_CONFIG_FILE: u32 = WM_APP + 6;
/// Posted by the update thread: lparam is a `Box<String>` with the newer tag, or 0.
const WM_UPDATE_RESULT: u32 = WM_APP + 7;
/// Posted by the tray device worker thread when one or more [`WorkerReport`]s are queued.
const WM_WORKER_REPORT: u32 = WM_APP + 8;

const ID_QUIT: usize = 1;
const ID_SETTINGS: usize = 2;
const ID_AUTOSTART: usize = 3;
const ID_UPDATE: usize = 4;

const RETRY_DEFER_MS: u32 = 200;
const RETRY_BASE_MS: u32 = 1200;
const RETRY_MAX_MS: u32 = 4800;

/// Poll step and quiet/timeout bounds (ms) of the shortcut/button release fence.
const FENCE_STEP_MS: u64 = 15;
const FENCE_QUIET_HELD_MS: u64 = 120;
const FENCE_QUIET_WIN_MS: u64 = 60;
const FENCE_MAX_WAIT_MS: u64 = 2400;

/// Pure per-game profile state machine, separated from Win32/HID for deterministic testing.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
struct GameSync {
    /// Game profile confirmed written in full to the hardware (`None` = base layout confirmed).
    applied: Option<String>,
    /// True while hardware may hold a partial/unconfirmed game layout, or right after base `apply()` ran.
    dirty: bool,
    /// True once a game DPI override started and before desktop DPI (`cfg.dpi.active`) is confirmed restored.
    dpi_overridden: bool,
}

impl GameSync {
    fn on_base_applied(&mut self, has_rules: bool) {
        self.applied = None;
        self.dirty = has_rules;
        self.dpi_overridden = false;
    }

    fn needs_sync(&self, wanted: &Option<String>) -> bool {
        self.dirty || *wanted != self.applied
    }

    fn begin_sync(&mut self) {
        self.dirty = true;
    }

    /// Decide which 1-based DPI stage to write (`None` = no game rule touches DPI).
    /// Uses `desktop_active` (`cfg.dpi.active`) deterministically whenever leaving a game or switching
    /// to a game without its own `dpi_stage`, so mid-game device reopens or restarts never poison desktop DPI.
    fn plan_dpi_stage(&mut self, desktop_active: u8, game_stage: Option<u8>, has_dpi_rules: bool) -> Option<u8> {
        match game_stage {
            Some(s) => {
                self.dpi_overridden = true;
                Some(s)
            }
            None if has_dpi_rules || self.dpi_overridden => Some(desktop_active),
            None => None,
        }
    }

    fn commit_sync(&mut self, wanted: Option<String>, game_stage: Option<u8>) {
        if game_stage.is_none() {
            self.dpi_overridden = false;
        }
        self.applied = wanted;
        self.dirty = false;
    }
}

/// Confirmed hardware state cached by the device worker, field by field.
/// Unknown (`None`) fields are read once from hardware; known matching fields skip both GET and SET.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
struct HwCache {
    polling: Option<u16>,
    stages: Option<DpiStages>,
    dpi: Option<(u16, u16)>,
    /// Per physical button code: `Some(f)` = confirmed function, `None` = button absent on this unit.
    buttons: BTreeMap<u8, Option<ButtonFunction>>,
}

impl HwCache {
    fn clear(&mut self) {
        self.polling = None;
        self.stages = None;
        self.dpi = None;
        self.buttons.clear();
    }

    /// Populate known base-layout values right after a full `apply::apply` succeeded on a verified mouse.
    fn seed_from_base(&mut self, spec: &DeviceSpec, cfg: &Config, polling: u16, stages: Option<DpiStages>, dpi: Option<(u16, u16)>) {
        self.polling = Some(polling);
        self.stages = stages;
        self.dpi = dpi;
        if spec.verified {
            for b in spec.buttons {
                let (func, _) = apply::normal_button_target(cfg, None, b);
                self.buttons.insert(b.code, Some(func));
            }
        } else {
            self.buttons.clear();
        }
    }
}

/// Coalesced desired state shared between the non-blocking Win32 tray thread and the single HID worker thread.
struct WorkerGoal {
    /// Incremented whenever `wanted_game`, `config`, or device state changes; cancels stale in-flight switches.
    epoch: u64,
    quit: bool,
    reopen: bool,
    unplugged: Option<String>,
    cache_rev: u64,
    config: Config,
    config_ok: bool,
    need_import: bool,
    need_base_apply: bool,
    notify_disk_reload: bool,
    wanted_game: Option<String>,
    has_game_rules: bool,
    wake_probe: bool,
    want_refresh: bool,
}

enum WorkerReport {
    Device { connected: bool, model_name: Option<String>, config_file: std::path::PathBuf, unplugged_error: bool },
    Imported(Box<Result<Config, String>>),
    Refresh { base_applied: Option<usize>, from_disk_reload: bool, result: Result<(Battery, Option<DpiStages>, Option<u16>), HidError> },
    Game { epoch: u64, applied: Option<String>, dirty: bool, deferred: bool, polling: Option<u16>, stages: Option<DpiStages>, error: Option<String> },
}

struct WorkerShared {
    goal: Mutex<WorkerGoal>,
    cvar: Condvar,
    reports: Mutex<VecDeque<WorkerReport>>,
}

impl WorkerShared {
    fn push_report(&self, hwnd: HWND, r: WorkerReport) {
        self.reports.lock().unwrap_or_else(|e| e.into_inner()).push_back(r);
        if hwnd != 0 {
            unsafe { PostMessageW(hwnd, WM_WORKER_REPORT, 0, 0) };
        }
    }
}

struct App {
    hwnd: HWND,
    nid: NOTIFYICONDATAW,
    hicon: HICON,
    config: Config,
    config_error: Option<String>,
    mouse_connected: bool,
    mouse_name: Option<String>,
    battery: Option<Battery>,
    stages: Option<DpiStages>,
    polling: Option<u16>,
    config_applied: bool,
    /// No config file yet: build one from the mouse's current settings once it answers.
    need_import: bool,
    /// The file `config` was loaded from: another mouse has another file.
    config_file: std::path::PathBuf,
    wanted_game: Option<String>,
    game_applied: Option<String>,
    game_dirty: bool,
    game_deferred: bool,
    fg_empty_retries: u8,
    notified_low: bool,
    last_error: Option<String>,
    last_refresh: Option<Instant>,
    taskbar_created: u32,
    /// The taskbar has the icon. False until Shell_NotifyIcon succeeds.
    icon_shown: bool,
    /// Modification time of the config file as last loaded, to tell an edit from our own save.
    config_mtime: Option<std::time::SystemTime>,
    /// A newer release on GitHub (version without the `v`), shown in the menu and tooltip.
    update_tag: Option<String>,
    notified_update: bool,
    /// Out-of-context WinEvent hooks (`EVENT_SYSTEM_FOREGROUND`, `EVENT_SYSTEM_MINIMIZEEND`), installed only when game rules exist.
    fg_hooks: [HWINEVENTHOOK; 2],
    /// Current `TIMER_RETRY` period (`0` = disarmed) and tick counter for bounded idle probing.
    retry_ms: u32,
    retry_ticks: u32,
    /// Current `TIMER_POLL` period in seconds.
    poll_secs: u32,
    shared: Arc<WorkerShared>,
    worker: Option<std::thread::JoinHandle<()>>,
}

static TRAY_HWND: AtomicIsize = AtomicIsize::new(0);
static GAME_MODE_UNSETTLED: AtomicBool = AtomicBool::new(false);

/// True while the tray is running and either inside a game profile or mid-transition
/// (debouncing, waiting on the shortcut fence, or restoring desktop settings).
pub(crate) fn is_game_mode_unsettled() -> bool {
    TRAY_HWND.load(Ordering::Relaxed) != 0 && GAME_MODE_UNSETTLED.load(Ordering::Relaxed)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn copy_wide(dst: &mut [u16], s: &str) {
    let w: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
    dst[..w.len()].copy_from_slice(&w);
    dst[w.len()] = 0;
}

/// Run the tray application on this thread until it quits. A second instance
/// just asks the running one to open the settings window.
pub fn run(open_settings: bool) {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let class_name = wide(CLASS_NAME);
        let mutex = CreateMutexW(ptr::null(), 1, wide("Local\\freenapse-tray").as_ptr());
        if mutex == 0 || GetLastError() == ERROR_ALREADY_EXISTS {
            // Already running: launching again opens the settings window, like Synapse.
            let existing = FindWindowW(class_name.as_ptr(), ptr::null());
            if existing != 0 {
                PostMessageW(existing, WM_OPEN_SETTINGS, 0, 0);
            }
            return;
        }

        let hinstance = GetModuleHandleW(ptr::null());
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: 0,
            hCursor: 0,
            hbrBackground: 0,
            lpszMenuName: ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };
        RegisterClassW(&wc);

        let need_import = !config_path().exists();
        let (config, config_error) = if need_import {
            (Config::default(), None)
        } else {
            match Config::load(&config_path()) {
                Ok(c) => (c, None),
                Err(e) => (Config::default(), Some(e)),
            }
        };

        let poll_secs = config.tray.poll_seconds.max(5);
        let has_game_rules = !config.games.is_empty() || !config.polling.games.is_empty();
        let config_ok = config_error.is_none();
        let shared = Arc::new(WorkerShared {
            goal: Mutex::new(WorkerGoal {
                epoch: 1,
                quit: false,
                reopen: false,
                unplugged: None,
                cache_rev: 1,
                config: config.clone(),
                config_ok,
                need_import,
                need_base_apply: !need_import && config_ok,
                notify_disk_reload: false,
                wanted_game: None,
                has_game_rules,
                wake_probe: true,
                want_refresh: true,
            }),
            cvar: Condvar::new(),
            reports: Mutex::new(VecDeque::new()),
        });
        let app = Box::new(App {
            hwnd: 0,
            nid: mem::zeroed(),
            hicon: 0,
            config,
            config_error,
            mouse_connected: false,
            mouse_name: None,
            battery: None,
            stages: None,
            polling: None,
            config_applied: false,
            need_import,
            config_file: config_path(),
            wanted_game: None,
            game_applied: None,
            game_dirty: has_game_rules,
            game_deferred: false,
            fg_empty_retries: 0,
            notified_low: false,
            last_error: None,
            last_refresh: None,
            taskbar_created: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
            icon_shown: false,
            config_mtime: None,
            update_tag: None,
            notified_update: false,
            fg_hooks: [0, 0],
            retry_ms: 0,
            retry_ticks: 0,
            poll_secs,
            shared,
            worker: None,
        });
        let app_ptr = Box::into_raw(app);

        // A hidden top-level window, not a message-only one: only top-level windows
        // receive the TaskbarCreated broadcast that says the taskbar is ready for
        // the icon (at login, and after Explorer restarts).
        let hwnd =
            CreateWindowExW(WS_EX_TOOLWINDOW, class_name.as_ptr(), wide("freenapse").as_ptr(), WS_POPUP, 0, 0, 0, 0, 0, 0, hinstance, app_ptr as *const c_void);
        if hwnd == 0 {
            return;
        }
        if open_settings {
            PostMessageW(hwnd, WM_OPEN_SETTINGS, 0, 0);
        }

        let mut msg: MSG = mem::zeroed();
        while GetMessageW(&mut msg, 0, 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe fn app_from(hwnd: HWND) -> *mut App {
    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            let cs = lparam as *const CREATESTRUCTW;
            let app = (*cs).lpCreateParams as *mut App;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, app as isize);
            (*app).hwnd = hwnd;
            TRAY_HWND.store(hwnd, Ordering::Relaxed);
            // Let TaskbarCreated through even if the process runs elevated.
            ChangeWindowMessageFilterEx(hwnd, (*app).taskbar_created, MSGFLT_ALLOW, ptr::null_mut());
            (*app).add_icon();
            (*app).repair_autostart();
            (*app).sync_fg_hooks();
            (*app).check_game();
            (*app).start_worker();
            (*app).sync_retry_timer();
            let poll_ms = (*app).poll_secs * 1000;
            SetTimer(hwnd, TIMER_POLL, poll_ms, None);
            // Reconnect the moment the dongle is plugged in or the PC wakes, instead of at the next poll.
            register_device_notifications(hwnd);
            register_resume_notifications(hwnd);
            // Re-apply the config file when it is edited by hand.
            (*app).note_config_mtime();
            watch_config(hwnd);
            if (*app).config.tray.check_updates {
                check_updates(hwnd);
                SetTimer(hwnd, TIMER_UPDATE, 24 * 60 * 60 * 1000, None);
            }
            0
        }
        WM_TIMER => {
            let app = app_from(hwnd);
            if app.is_null() {
                return 0;
            }
            match wparam {
                TIMER_POLL => {
                    (*app).check_game();
                    (*app).request_refresh();
                    (*app).sync_retry_timer();
                }
                TIMER_GAME => {
                    KillTimer(hwnd, TIMER_GAME);
                    (*app).check_game();
                    (*app).sync_retry_timer();
                }
                TIMER_RETRY => {
                    (*app).retry_ticks = (*app).retry_ticks.wrapping_add(1);
                    // RETRY_BASE_MS (1200 ms) < user_active() (1500 ms) so mouse wakes are never missed;
                    // every 4th tick also probes once in case the mouse was already awake without cursor motion;
                    // a deferred key-fence probes promptly on RETRY_DEFER_MS.
                    if (*app).game_deferred || user_active() || (*app).retry_ticks.is_multiple_of(4) {
                        let need_base = (*app).need_import || (!(*app).config_applied && (*app).config_error.is_none()) || (*app).battery.is_none();
                        if (*app).game_dirty {
                            (*app).check_game();
                        }
                        (*app).wake_worker(need_base);
                    }
                    (*app).sync_retry_timer();
                }
                TIMER_REPLUG => {
                    KillTimer(hwnd, TIMER_REPLUG);
                    // The handle may be stale after a resume; tell the worker to reopen the device.
                    (*app).mouse_connected = false;
                    (*app).battery = None;
                    (*app).request_reopen();
                    (*app).check_game();
                    (*app).sync_retry_timer();
                }
                TIMER_CONFIG => {
                    KillTimer(hwnd, TIMER_CONFIG);
                    (*app).reload_config_from_disk();
                }
                TIMER_UPDATE => {
                    if (*app).config.tray.check_updates {
                        check_updates(hwnd);
                    }
                }
                TIMER_ICON => (*app).show_icon(),
                _ => {}
            }
            0
        }
        WM_TRAY => {
            let app = app_from(hwnd);
            let event = (lparam & 0xffff) as u32;
            if event == WM_CONTEXTMENU || event == WM_RBUTTONUP {
                (*app).show_menu();
            } else if event == NIN_SELECT || event == WM_LBUTTONDBLCLK {
                // Left click on the icon opens the settings window, like Synapse.
                ui::open(hwnd);
            }
            0
        }
        WM_QUERYENDSESSION => {
            // Signing out or shutting down: make sure the next sign-in starts the tray.
            let app = app_from(hwnd);
            if !app.is_null() {
                (*app).repair_autostart();
            }
            1
        }
        WM_DESTROY => {
            TRAY_HWND.store(0, Ordering::Relaxed);
            GAME_MODE_UNSETTLED.store(false, Ordering::Relaxed);
            let app = app_from(hwnd);
            if !app.is_null() {
                (*app).unhook_fg();
                (*app).stop_worker();
                Shell_NotifyIconW(NIM_DELETE, &(*app).nid);
                if (*app).hicon != 0 {
                    DestroyIcon((*app).hicon);
                }
                drop(Box::from_raw(app));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            PostQuitMessage(0);
            0
        }
        _ => {
            let app = app_from(hwnd);
            if app.is_null() {
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }
            match msg {
                m if m == (*app).taskbar_created => {
                    // The taskbar is (again) ready: at login, or after Explorer restarted.
                    (*app).icon_shown = false;
                    (*app).show_icon();
                    0
                }
                WM_OPEN_SETTINGS => {
                    ui::open(hwnd);
                    0
                }
                WM_WORKER_REPORT => {
                    (*app).drain_worker_reports();
                    0
                }
                ui::WM_UI_CONFIG_CHANGED => {
                    // The settings window saved the config file (and wrote the mouse when wparam == 1).
                    match Config::load(&config_path()) {
                        Ok(c) => {
                            (*app).config = c;
                            (*app).config_error = None;
                        }
                        Err(e) => (*app).config_error = Some(e),
                    }
                    (*app).sync_poll_timer();
                    (*app).sync_fg_hooks();
                    (*app).note_config_mtime();
                    (*app).config_applied = wparam == 1;
                    (*app).game_applied = None;
                    (*app).game_dirty = (*app).has_game_rules();
                    (*app).push_config_to_worker(wparam != 1, false);
                    (*app).check_game();
                    (*app).sync_retry_timer();
                    0
                }
                ui::WM_UI_CLOSED => 0,
                WM_DEVICECHANGE => {
                    let event = wparam as u32;
                    if event == DBT_DEVICEARRIVAL {
                        if broadcast_interface_name(lparam).is_some_and(|n| n.to_ascii_lowercase().contains("vid_1532")) {
                            // A Razer HID interface arrived (dongle plugged in or USB-C cable attached): look again shortly.
                            SetTimer(hwnd, TIMER_REPLUG, 1500, None);
                        }
                    } else if event == DBT_DEVICEREMOVECOMPLETE {
                        if let Some(name) = broadcast_interface_name(lparam) {
                            (*app).notify_unplugged(name);
                        }
                    }
                    1
                }
                WM_POWERBROADCAST => {
                    let event = wparam as u32;
                    if event == PBT_APMRESUMEAUTOMATIC || event == PBT_APMRESUMESUSPEND {
                        // The dongle re-enumerates after a resume; give it a moment.
                        SetTimer(hwnd, TIMER_REPLUG, 3000, None);
                    }
                    1
                }
                WM_CONFIG_FILE => {
                    SetTimer(hwnd, TIMER_CONFIG, 400, None);
                    0
                }
                WM_UPDATE_RESULT => {
                    if lparam != 0 {
                        let tag = *Box::from_raw(lparam as *mut String);
                        (*app).update_found(&tag);
                    }
                    0
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
    }
}

impl App {
    unsafe fn add_icon(&mut self) {
        self.nid.cbSize = mem::size_of::<NOTIFYICONDATAW>() as u32;
        self.nid.hWnd = self.hwnd;
        self.nid.uID = 1;
        self.nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
        self.nid.uCallbackMessage = WM_TRAY;
        self.nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        self.set_icon();
        copy_wide(&mut self.nid.szTip, "Freenapse-DeathAdder V4 Pro：正在连接鼠标...");
        self.show_icon();
    }

    /// Put the icon in the notification area. Right after login the taskbar
    /// often refuses it, so a failure retries every two seconds until it works
    /// (TaskbarCreated also triggers a new try).
    unsafe fn show_icon(&mut self) {
        if !self.icon_shown {
            self.nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
            // NIM_MODIFY succeeds when the icon is already there (a NIM_ADD that timed out may still have landed).
            self.icon_shown = Shell_NotifyIconW(NIM_ADD, &self.nid) != 0 || Shell_NotifyIconW(NIM_MODIFY, &self.nid) != 0;
            if self.icon_shown {
                Shell_NotifyIconW(NIM_SETVERSION, &self.nid);
            }
        }
        if self.icon_shown {
            KillTimer(self.hwnd, TIMER_ICON);
        } else {
            SetTimer(self.hwnd, TIMER_ICON, 2000, None);
        }
    }

    unsafe fn set_icon(&mut self) {
        let new_icon = make_battery_icon(self.battery, self.mouse_connected);
        if new_icon != 0 {
            if self.hicon != 0 {
                DestroyIcon(self.hicon);
            }
            self.hicon = new_icon;
            self.nid.hIcon = self.hicon;
        }
    }

    fn update_game_mode_flag(&self) {
        let unsettled = self.wanted_game.is_some() || self.game_applied.is_some() || self.game_dirty || (self.has_game_rules() && !self.config_applied);
        GAME_MODE_UNSETTLED.store(unsettled, Ordering::Relaxed);
    }

    unsafe fn update_tray(&mut self) {
        self.update_game_mode_flag();
        self.set_icon();
        let mut tip = format!("Freenapse-DeathAdder V4 Pro ({})", crate::VERSION);
        match (self.battery, self.mouse_connected) {
            (Some(b), _) => {
                if b.wired {
                    tip.push_str("\n有线直连（无电池）");
                } else {
                    tip.push_str(&format!("\n电量 {}%{}", b.percent, if b.charging { "（充电中）" } else { "" }));
                }
            }
            (None, true) => tip.push_str("\n鼠标休眠中：移动鼠标唤醒"),
            (None, false) => tip.push_str("\n未检测到鼠标"),
        }
        if let Some(st) = &self.stages {
            if let Some(d) = st.stages.get(st.active.saturating_sub(1) as usize) {
                tip.push_str(&format!("\nDPI {}", d.0));
            }
        }
        if let Some(p) = self.polling {
            tip.push_str(&format!("\n{p} Hz"));
        }
        if let Some(g) = self.game_applied.as_deref().and_then(|k| self.config.game_by_key(k)) {
            tip.push_str(&format!("\n游戏配置：{}", g.name));
        }
        if let Some(e) = &self.config_error {
            tip.push_str(&format!("\n配置文件异常：{e}"));
        }
        if let Some(v) = &self.update_tag {
            tip.push_str(&format!("\n发现新版本 {v}"));
        }
        copy_wide(&mut self.nid.szTip, &tip);
        self.nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
        Shell_NotifyIconW(NIM_MODIFY, &self.nid);
    }

    /// Point the "start at login" entry at this executable again when it broke, and say so.
    unsafe fn repair_autostart(&mut self) {
        if crate::autostart::repair() {
            self.notify("Freenapse-DeathAdder V4 Pro", "检测到开机启动项路径变更，已自动指向当前 freenapse.exe。", false);
        }
    }

    unsafe fn notify(&mut self, title: &str, text: &str, warning: bool) {
        copy_wide(&mut self.nid.szInfoTitle, title);
        copy_wide(&mut self.nid.szInfo, text);
        self.nid.dwInfoFlags = if warning { NIIF_WARNING } else { NIIF_INFO };
        self.nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP | NIF_INFO;
        Shell_NotifyIconW(NIM_MODIFY, &self.nid);
        self.nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
    }

    fn has_game_rules(&self) -> bool {
        !self.config.games.is_empty() || !self.config.polling.games.is_empty()
    }

    fn start_worker(&mut self) {
        if self.worker.is_some() {
            return;
        }
        let hwnd = self.hwnd;
        let shared = self.shared.clone();
        if let Ok(h) = std::thread::Builder::new().name("freenapse-tray-io".into()).spawn(move || worker_loop(hwnd, shared)) {
            self.worker = Some(h);
        }
    }

    fn stop_worker(&mut self) {
        {
            let mut g = self.shared.goal.lock().unwrap_or_else(|e| e.into_inner());
            g.quit = true;
            g.epoch = g.epoch.wrapping_add(1);
        }
        self.shared.cvar.notify_all();
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }

    fn request_refresh(&mut self) {
        {
            let mut g = self.shared.goal.lock().unwrap_or_else(|e| e.into_inner());
            g.want_refresh = true;
            g.wake_probe = true;
        }
        self.shared.cvar.notify_one();
    }

    fn wake_worker(&mut self, want_refresh: bool) {
        {
            let mut g = self.shared.goal.lock().unwrap_or_else(|e| e.into_inner());
            if want_refresh {
                g.want_refresh = true;
            }
            g.wake_probe = true;
        }
        self.shared.cvar.notify_one();
    }

    fn request_reopen(&mut self) {
        {
            let mut g = self.shared.goal.lock().unwrap_or_else(|e| e.into_inner());
            g.reopen = true;
            g.want_refresh = true;
            g.wake_probe = true;
            g.epoch = g.epoch.wrapping_add(1);
        }
        self.shared.cvar.notify_one();
    }

    fn notify_unplugged(&mut self, name: String) {
        {
            let mut g = self.shared.goal.lock().unwrap_or_else(|e| e.into_inner());
            g.unplugged = Some(name);
            g.epoch = g.epoch.wrapping_add(1);
        }
        self.shared.cvar.notify_one();
    }

    fn push_config_to_worker(&mut self, need_base_apply: bool, notify_disk_reload: bool) {
        {
            let mut g = self.shared.goal.lock().unwrap_or_else(|e| e.into_inner());
            g.config = self.config.clone();
            g.config_ok = self.config_error.is_none();
            g.need_import = self.need_import;
            g.need_base_apply = need_base_apply && g.config_ok;
            g.notify_disk_reload = notify_disk_reload;
            g.has_game_rules = self.has_game_rules();
            g.cache_rev = g.cache_rev.wrapping_add(1);
            g.want_refresh = true;
            g.wake_probe = true;
            g.epoch = g.epoch.wrapping_add(1);
        }
        self.shared.cvar.notify_one();
    }

    /// Apply reports produced by the background device worker on the Win32 thread.
    unsafe fn drain_worker_reports(&mut self) {
        loop {
            let Some(rep) = self.shared.reports.lock().unwrap_or_else(|e| e.into_inner()).pop_front() else {
                break;
            };
            match rep {
                WorkerReport::Device { connected, model_name, config_file, unplugged_error } => {
                    self.mouse_connected = connected;
                    self.mouse_name = model_name;
                    if connected && config_file != self.config_file {
                        self.switch_config();
                    }
                    if !connected && unplugged_error {
                        self.battery = None;
                        self.last_error = Some("mouse unplugged".into());
                    }
                }
                WorkerReport::Imported(res) => match *res {
                    Ok(cfg) => {
                        if let Err(e) = Config::write_new(&config_path(), &cfg) {
                            self.config_error = Some(e);
                        }
                        self.config = cfg;
                        self.need_import = false;
                        self.note_config_mtime();
                        self.config_applied = true;
                        self.game_applied = None;
                        self.game_dirty = self.has_game_rules();
                        self.sync_poll_timer();
                        self.sync_fg_hooks();
                        self.push_config_to_worker(false, false);
                        self.check_game();
                        self.notify("Freenapse-DeathAdder V4 Pro", "已从鼠标板载内存导入初始配置并保存", false);
                    }
                    Err(e) => {
                        self.last_error = Some(e);
                    }
                },
                WorkerReport::Refresh { base_applied, from_disk_reload, result } => match result {
                    Ok((battery, stages, polling)) => {
                        if let Some(n) = base_applied {
                            self.config_applied = true;
                            self.game_applied = None;
                            if self.has_game_rules() {
                                self.game_dirty = true;
                            }
                            if from_disk_reload {
                                self.notify("Freenapse-DeathAdder V4 Pro", "检测到配置文件修改：已自动写入鼠标", false);
                            } else if n > 0 {
                                self.notify("Freenapse-DeathAdder V4 Pro", &format!("已将 {n} 项配置同步至鼠标板载内存"), false);
                            }
                        }
                        self.battery = Some(battery);
                        if stages.is_some() {
                            self.stages = stages;
                        }
                        if polling.is_some() {
                            self.polling = polling;
                        }
                        self.last_refresh = Some(Instant::now());
                        self.last_error = None;
                        let low = self.config.tray.notify_percent;
                        if !battery.wired && !battery.charging && battery.percent <= low && !self.notified_low {
                            self.notified_low = true;
                            let name = self.mouse_name.as_deref().map(ui::display_model).unwrap_or_else(|| "鼠标".into());
                            self.notify("鼠标电量过低", &format!("{name} 剩余电量 {}%，请及时充电。", battery.percent), true);
                        }
                        if battery.charging || battery.percent > low + 5 {
                            self.notified_low = false;
                        }
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        match e {
                            HidError::Invalid(_) | HidError::Unsupported(_) | HidError::Unverified(_) => {
                                self.config_error = Some(msg.clone());
                            }
                            HidError::Win32(..) | HidError::NotFound => {
                                self.mouse_connected = false;
                                self.battery = None;
                            }
                            _ => {}
                        }
                        self.last_error = Some(msg);
                    }
                },
                WorkerReport::Game { epoch, applied, dirty, deferred, polling, stages, error } => {
                    let latest_epoch = self.shared.goal.lock().unwrap_or_else(|e| e.into_inner()).epoch;
                    self.game_applied = applied;
                    self.game_dirty = dirty || epoch != latest_epoch;
                    self.game_deferred = deferred && epoch == latest_epoch;
                    if polling.is_some() {
                        self.polling = polling;
                    }
                    if stages.is_some() {
                        self.stages = stages;
                    }
                    self.last_error = error;
                }
            }
        }
        self.update_tray();
        self.sync_retry_timer();
    }

    /// Per-game profiles: identify the foreground executable on the UI thread (non-blocking)
    /// and hand the target generation to the device worker.
    unsafe fn check_game(&mut self) {
        if !self.has_game_rules() && !self.game_dirty && self.game_applied.is_none() {
            self.wanted_game = None;
            self.update_game_mode_flag();
            return;
        }
        let path = match query_foreground() {
            FgQuery::TransientEmpty if self.fg_empty_retries < 3 => {
                self.fg_empty_retries += 1;
                SetTimer(self.hwnd, TIMER_GAME, 150, None);
                return;
            }
            FgQuery::TransientEmpty => {
                self.fg_empty_retries = 0;
                String::new()
            }
            FgQuery::Window(opt) => {
                self.fg_empty_retries = 0;
                opt.unwrap_or_default()
            }
        };
        let wanted = self.config.game_for(&path).map(|g| g.key);
        self.wanted_game = wanted.clone();

        let need_notify = {
            let mut g = self.shared.goal.lock().unwrap_or_else(|e| e.into_inner());
            if g.wanted_game != wanted {
                g.wanted_game = wanted.clone();
                g.wake_probe = true;
                g.epoch = g.epoch.wrapping_add(1);
                self.game_dirty = true;
                self.game_deferred = false;
                true
            } else if self.game_dirty || wanted != self.game_applied {
                self.game_dirty = true;
                g.wake_probe = true;
                true
            } else {
                false
            }
        };
        self.update_game_mode_flag();
        if need_notify {
            self.shared.cvar.notify_one();
        }
    }

    /// Subscribe to out-of-context foreground window events (`WINEVENT_OUTOFCONTEXT`, including our own
    /// settings window so clicking directly from a game into freenapse exits game mode cleanly).
    unsafe fn sync_fg_hooks(&mut self) {
        if self.has_game_rules() {
            if self.fg_hooks[0] == 0 {
                self.fg_hooks[0] = SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, 0, Some(win_event_proc), 0, 0, WINEVENT_OUTOFCONTEXT);
            }
            if self.fg_hooks[1] == 0 {
                self.fg_hooks[1] = SetWinEventHook(EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZEEND, 0, Some(win_event_proc), 0, 0, WINEVENT_OUTOFCONTEXT);
            }
        } else {
            self.unhook_fg();
        }
    }

    unsafe fn unhook_fg(&mut self) {
        for h in &mut self.fg_hooks {
            if *h != 0 {
                UnhookWinEvent(*h);
                *h = 0;
            }
        }
        if self.hwnd != 0 {
            KillTimer(self.hwnd, TIMER_GAME);
        }
    }

    unsafe fn sync_poll_timer(&mut self) {
        let want = self.config.tray.poll_seconds.max(5);
        if want != self.poll_secs && self.hwnd != 0 {
            if SetTimer(self.hwnd, TIMER_POLL, want * 1000, None) != 0 {
                self.poll_secs = want;
            }
        }
    }

    /// Arm the wake-up retry timer with bounded backoff (`1200..=4800 ms`, or `200 ms` while a held shortcut defers writes).
    unsafe fn sync_retry_timer(&mut self) {
        let need_base = self.need_import || (!self.config_applied && self.config_error.is_none()) || self.battery.is_none();
        let need = (need_base || self.game_dirty) && self.config_error.is_none();
        if need {
            let next_ms = if self.game_deferred {
                RETRY_DEFER_MS
            } else if self.retry_ms == 0 || self.retry_ms == RETRY_DEFER_MS {
                RETRY_BASE_MS
            } else if self.last_error.is_some() {
                (self.retry_ms * 2).min(RETRY_MAX_MS)
            } else {
                RETRY_BASE_MS
            };
            if next_ms != self.retry_ms && SetTimer(self.hwnd, TIMER_RETRY, next_ms, None) != 0 {
                self.retry_ms = next_ms;
            }
        } else if self.retry_ms != 0 {
            KillTimer(self.hwnd, TIMER_RETRY);
            self.retry_ms = 0;
            self.retry_ticks = 0;
        }
    }

    /// A different mouse is open: drive it with its own config file.
    unsafe fn switch_config(&mut self) {
        self.config_file = config_path();
        self.need_import = !self.config_file.exists();
        let (config, err) = if self.need_import {
            (Config::default(), None)
        } else {
            match Config::load(&self.config_file) {
                Ok(c) => (c, None),
                Err(e) => (Config::default(), Some(e)),
            }
        };
        self.config = config;
        self.config_error = err;
        self.sync_poll_timer();
        self.sync_fg_hooks();
        self.config_applied = false;
        self.game_applied = None;
        self.game_dirty = self.has_game_rules();
        self.game_deferred = false;
        self.note_config_mtime();
        self.push_config_to_worker(!self.need_import, false);
        self.check_game();
    }

    fn note_config_mtime(&mut self) {
        self.config_mtime = std::fs::metadata(config_path()).and_then(|m| m.modified()).ok();
    }

    /// The config file changed on disk (watcher thread): reload it and apply it,
    /// unless it was our own save.
    unsafe fn reload_config_from_disk(&mut self) {
        let mtime = std::fs::metadata(config_path()).and_then(|m| m.modified()).ok();
        if mtime.is_none() || mtime == self.config_mtime {
            return;
        }
        self.config_mtime = mtime;
        match Config::load(&config_path()) {
            Ok(c) => {
                if c == self.config {
                    return;
                }
                self.config = c;
                self.config_error = None;
            }
            Err(e) => {
                // Half-written or wrong: say so in the tooltip, keep driving the mouse with the old one.
                self.config_error = Some(e);
                self.update_tray();
                return;
            }
        }
        self.sync_poll_timer();
        self.sync_fg_hooks();
        self.config_applied = false;
        self.game_applied = None;
        self.game_dirty = self.has_game_rules();
        self.push_config_to_worker(true, true);
        self.check_game();
        self.sync_retry_timer();
    }

    /// The update thread found a release newer than this build.
    unsafe fn update_found(&mut self, tag: &str) {
        let v = tag.trim().trim_start_matches(['v', 'V']).to_string();
        if !crate::update::is_newer(&v, crate::VERSION) {
            return;
        }
        if !self.notified_update {
            self.notified_update = true;
            self.notify("发现上游新版本", &format!("上游版本 {v} 已发布，可在托盘右键菜单查看。"), false);
        }
        self.update_tag = Some(v);
        self.update_tray();
    }

    unsafe fn show_menu(&mut self) {
        let menu = CreatePopupMenu();
        let mut parts = Vec::new();
        match (self.battery, self.mouse_connected) {
            (Some(b), _) if b.wired => parts.push("有线直连".to_string()),
            (Some(b), _) => parts.push(format!("电量 {}%{}", b.percent, if b.charging { "（充电中）" } else { "" })),
            (None, true) => parts.push("鼠标已休眠（移动唤醒）".to_string()),
            (None, false) => parts.push("未检测到鼠标".to_string()),
        }
        if self.battery.is_some() {
            if let Some(d) = self.stages.as_ref().and_then(|st| st.stages.get(st.active.saturating_sub(1) as usize)) {
                parts.push(format!("{} DPI", d.0));
            }
            if let Some(p) = self.polling {
                parts.push(format!("{p} Hz"));
            }
            if let Some(g) = self.game_applied.as_deref().and_then(|k| self.config.game_by_key(k)) {
                parts.push(g.name);
            }
        }
        let status = parts.join(" · ");
        AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, wide(&status).as_ptr());
        if let Some(v) = &self.update_tag {
            AppendMenuW(menu, MF_STRING, ID_UPDATE, wide(&format!("发现新版本 {v}...")).as_ptr());
        }
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(menu, MF_STRING, ID_SETTINGS, wide("打开控制中心").as_ptr());
        SetMenuDefaultItem(menu, ID_SETTINGS as u32, 0);
        let auto_chk = if crate::autostart::is_enabled() { MF_CHECKED } else { 0 };
        AppendMenuW(menu, MF_STRING | auto_chk, ID_AUTOSTART, wide("开机自启").as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(menu, MF_STRING, ID_QUIT, wide("退出").as_ptr());

        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        SetForegroundWindow(self.hwnd);
        let id = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, 0, self.hwnd, ptr::null());
        PostMessageW(self.hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        self.handle_command(id as usize);
    }

    unsafe fn handle_command(&mut self, id: usize) {
        match id {
            ID_SETTINGS => ui::open(self.hwnd),
            ID_AUTOSTART => {
                let want = !crate::autostart::is_enabled();
                if let Err(e) = crate::autostart::set(want) {
                    self.notify("Freenapse-DeathAdder V4 Pro", &format!("设置开机自启失败：{e}"), true);
                }
            }
            ID_UPDATE => {
                ShellExecuteW(0, wide("open").as_ptr(), wide(crate::update::RELEASES_URL).as_ptr(), ptr::null(), ptr::null(), SW_SHOWNORMAL);
            }
            ID_QUIT => {
                DestroyWindow(self.hwnd);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Background device worker: coalesced latest-target switching, field-level
// differential writes, and Win/shortcut release fence.
// ---------------------------------------------------------------------------

trait SyncBackend {
    fn spec(&self) -> &DeviceSpec;
    fn profile(&self) -> u8;
    fn writes_allowed(&self) -> bool;
    fn polling_rate(&self) -> crate::hid::Result<u16>;
    fn set_polling_rate(&self, hz: u16) -> crate::hid::Result<()>;
    fn dpi(&self) -> crate::hid::Result<(u16, u16)>;
    fn dpi_stages(&self) -> crate::hid::Result<DpiStages>;
    fn set_dpi_stages(&self, active: u8, stages: &[(u16, u16)]) -> crate::hid::Result<()>;
    fn current_button(&self, code: u8, name: &str, bound: bool) -> crate::hid::Result<Option<ButtonFunction>>;
    fn set_button(&self, profile: u8, code: u8, f: ButtonFunction) -> crate::hid::Result<()>;
}

impl SyncBackend for Mouse {
    fn spec(&self) -> &DeviceSpec {
        Mouse::spec(self)
    }
    fn profile(&self) -> u8 {
        Mouse::profile(self)
    }
    fn writes_allowed(&self) -> bool {
        Mouse::writes_allowed(self)
    }
    fn polling_rate(&self) -> crate::hid::Result<u16> {
        Mouse::polling_rate(self)
    }
    fn set_polling_rate(&self, hz: u16) -> crate::hid::Result<()> {
        Mouse::set_polling_rate(self, hz)
    }
    fn dpi(&self) -> crate::hid::Result<(u16, u16)> {
        Mouse::dpi(self)
    }
    fn dpi_stages(&self) -> crate::hid::Result<DpiStages> {
        Mouse::dpi_stages(self)
    }
    fn set_dpi_stages(&self, active: u8, stages: &[(u16, u16)]) -> crate::hid::Result<()> {
        Mouse::set_dpi_stages(self, active, stages)
    }
    fn current_button(&self, code: u8, name: &str, bound: bool) -> crate::hid::Result<Option<ButtonFunction>> {
        apply::current_button(self, code, name, false, bound)
    }
    fn set_button(&self, profile: u8, code: u8, f: ButtonFunction) -> crate::hid::Result<()> {
        Mouse::set_button(self, profile, code, false, f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FenceOutcome {
    Ready,
    Cancelled,
    TimedOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncStep {
    Committed,
    Cancelled,
    Deferred,
}

fn any_vk_pressed_os(vks: &[u16]) -> bool {
    unsafe { vks.iter().any(|&vk| (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0) }
}

fn push_vk_unique(out: &mut Vec<u16>, vk: u16) {
    if !out.contains(&vk) {
        out.push(vk);
    }
}

/// Wait until no virtual key in `vks` is held down and `vks` stays quiet for the required window:
/// - If any guarded key is down initially (or goes down during an initial Win-shortcut settle check),
///   require `quiet_held_ms` of continuous release before proceeding.
/// - Otherwise, if `initial_quiet_ms > 0` (e.g. a button involving `VK_LWIN` / `VK_RWIN` is changing),
///   verify `initial_quiet_ms` of quietness so a just-released `Win+D` finishes settling on the dongle.
fn wait_until_quiet(
    vks: &[u16],
    initial_quiet_ms: u64,
    quiet_held_ms: u64,
    max_wait_ms: u64,
    mut any_down: impl FnMut(&[u16]) -> bool,
    mut is_cancelled: impl FnMut() -> bool,
    mut sleep_step: impl FnMut(u64),
) -> FenceOutcome {
    if vks.is_empty() {
        return FenceOutcome::Ready;
    }
    let mut saw_down = any_down(vks);
    if !saw_down && initial_quiet_ms == 0 {
        return FenceOutcome::Ready;
    }
    let mut quiet_for = 0u64;
    let mut waited = 0u64;
    loop {
        if is_cancelled() {
            return FenceOutcome::Cancelled;
        }
        let target_quiet = if saw_down { quiet_held_ms } else { initial_quiet_ms };
        if !any_down(vks) && quiet_for >= target_quiet {
            return FenceOutcome::Ready;
        }
        if waited >= max_wait_ms {
            return FenceOutcome::TimedOut;
        }
        sleep_step(FENCE_STEP_MS);
        waited += FENCE_STEP_MS;
        if is_cancelled() {
            return FenceOutcome::Cancelled;
        }
        if any_down(vks) {
            saw_down = true;
            quiet_for = 0;
        } else {
            quiet_for += FENCE_STEP_MS;
        }
    }
}

/// Collect virtual keys guarded during a profile transition (`VK_LWIN`, `VK_RWIN`, side buttons,
/// and all virtual keys emitted by changing or unknown normal-layer button mappings), returning `(vks, involves_win_binding)`.
fn build_switch_guard_vks(spec: &DeviceSpec, cfg: &Config, game: &GameSync, hw: &HwCache, wanted: Option<&str>) -> (Vec<u16>, bool) {
    let over = wanted.and_then(|k| cfg.game_by_key(k));
    let prev_over = game.applied.as_deref().and_then(|k| cfg.game_by_key(k));
    let mut binding_vks = Vec::new();
    for b in spec.buttons {
        let (want_func, _) = apply::normal_button_target(cfg, over.as_ref(), b);
        match hw.buttons.get(&b.code).copied() {
            Some(Some(cur)) if cur == want_func => continue,
            Some(None) => continue,
            Some(Some(cur)) => {
                cur.push_virtual_keys(&mut binding_vks);
                want_func.push_virtual_keys(&mut binding_vks);
            }
            None => {
                let (base_f, _) = apply::normal_button_target(cfg, None, b);
                base_f.push_virtual_keys(&mut binding_vks);
                if let Some(po) = prev_over.as_ref() {
                    let (pf, _) = apply::normal_button_target(cfg, Some(po), b);
                    pf.push_virtual_keys(&mut binding_vks);
                }
                want_func.push_virtual_keys(&mut binding_vks);
            }
        }
    }
    let involves_win = binding_vks.contains(&0x5b) || binding_vks.contains(&0x5c);
    let mut out = vec![0x5b, 0x5c, 0x05, 0x06]; // VK_LWIN, VK_RWIN, VK_XBUTTON1, VK_XBUTTON2
    for vk in binding_vks {
        push_vk_unique(&mut out, vk);
    }
    (out, involves_win)
}

/// Execute one incremental game-profile sync toward `wanted`, checking `is_cancelled` before every HID call
/// and guarding active shortcut/button gestures so held keys (like `Win+D`) are never torn mid-press.
#[allow(clippy::too_many_arguments)]
fn sync_game_target(
    mouse: &impl SyncBackend,
    cfg: &Config,
    wanted: &Option<String>,
    game: &mut GameSync,
    hw: &mut HwCache,
    mut any_down: impl FnMut(&[u16]) -> bool,
    mut is_cancelled: impl FnMut() -> bool,
    mut sleep_step: impl FnMut(u64),
) -> Result<SyncStep, HidError> {
    if !game.needs_sync(wanted) {
        return Ok(SyncStep::Committed);
    }
    game.begin_sync();
    if !mouse.writes_allowed() {
        game.commit_sync(wanted.clone(), None);
        return Ok(SyncStep::Committed);
    }
    if is_cancelled() {
        return Ok(SyncStep::Cancelled);
    }

    let over = wanted.as_deref().and_then(|k| cfg.game_by_key(k));
    let want_polling = over.as_ref().map(|g| g.polling).unwrap_or(cfg.polling.rate);
    let game_stage = over.as_ref().and_then(|g| g.dpi_stage);

    // Pre-switch gesture fence: do not touch polling rate, DPI or button mappings while Win / side buttons
    // or any changing button's virtual key is held down or settling.
    let (guard_vks, involves_win) = build_switch_guard_vks(mouse.spec(), cfg, game, hw, wanted.as_deref());
    let init_quiet = if involves_win { FENCE_QUIET_WIN_MS } else { 0 };
    match wait_until_quiet(&guard_vks, init_quiet, FENCE_QUIET_HELD_MS, FENCE_MAX_WAIT_MS, &mut any_down, &mut is_cancelled, &mut sleep_step) {
        FenceOutcome::Ready => {}
        FenceOutcome::Cancelled => return Ok(SyncStep::Cancelled),
        FenceOutcome::TimedOut => return Ok(SyncStep::Deferred),
    }

    // 1. Polling rate (skip GET + SET when already known equal).
    if hw.polling != Some(want_polling) {
        if is_cancelled() {
            return Ok(SyncStep::Cancelled);
        }
        let cur = match hw.polling {
            Some(p) => p,
            None => {
                let p = mouse.polling_rate()?;
                hw.polling = Some(p);
                p
            }
        };
        if cur != want_polling {
            if is_cancelled() {
                return Ok(SyncStep::Cancelled);
            }
            hw.polling = None;
            mouse.set_polling_rate(want_polling)?;
            hw.polling = Some(want_polling);
        }
    }

    // 2. DPI stage + live `(x, y)` sensor DPI: deterministic (`game_stage` in-game, `cfg.dpi.active` on desktop),
    // verifying both stage table (`04/86`) and live XY DPI (`04/85`) when cold, with zero GETs when warm.
    let want_stages = cfg.stage_pairs();
    let desktop_active = cfg.dpi.active.clamp(1, want_stages.len() as u8);
    let has_dpi_rules = cfg.games.iter().any(|g| g.dpi_stage.is_some());
    if let Some(stage) = game.plan_dpi_stage(desktop_active, game_stage, has_dpi_rules).filter(|s| (1..=want_stages.len() as u8).contains(s)) {
        let want_dpi = want_stages[stage as usize - 1];
        if hw.stages.is_none() {
            if is_cancelled() {
                return Ok(SyncStep::Cancelled);
            }
            hw.stages = Some(mouse.dpi_stages()?);
        }
        if hw.dpi.is_none() {
            if is_cancelled() {
                return Ok(SyncStep::Cancelled);
            }
            hw.dpi = Some(mouse.dpi()?);
        }
        let stages_ok = hw.stages.as_ref().is_some_and(|s| s.active == stage && s.stages == want_stages);
        let dpi_ok = hw.dpi == Some(want_dpi);
        if !stages_ok || !dpi_ok {
            if is_cancelled() {
                return Ok(SyncStep::Cancelled);
            }
            hw.stages = None;
            hw.dpi = None;
            mouse.set_dpi_stages(stage, &want_stages)?;
            hw.stages = Some(DpiStages { active: stage, stages: want_stages });
            hw.dpi = Some(want_dpi);
        }
    }

    // 3. Normal-layer buttons: skip every button whose cached assignment already equals `want_func`,
    // and re-verify the release fence right before each `set_button` write.
    let profile = mouse.profile();
    for b in mouse.spec().buttons {
        if is_cancelled() {
            return Ok(SyncStep::Cancelled);
        }
        let (want_func, bound) = apply::normal_button_target(cfg, over.as_ref(), b);
        let cur_func = match hw.buttons.get(&b.code).copied() {
            Some(Some(f)) => f,
            Some(None) => continue,
            None => match mouse.current_button(b.code, b.name, bound)? {
                Some(f) => {
                    hw.buttons.insert(b.code, Some(f));
                    f
                }
                None => {
                    hw.buttons.insert(b.code, None);
                    continue;
                }
            },
        };
        if cur_func == want_func {
            continue;
        }
        let mut btn_vks = vec![0x5b, 0x5c, 0x05, 0x06];
        cur_func.push_virtual_keys(&mut btn_vks);
        want_func.push_virtual_keys(&mut btn_vks);
        match wait_until_quiet(&btn_vks, 0, FENCE_QUIET_HELD_MS, FENCE_MAX_WAIT_MS, &mut any_down, &mut is_cancelled, &mut sleep_step) {
            FenceOutcome::Ready => {}
            FenceOutcome::Cancelled => return Ok(SyncStep::Cancelled),
            FenceOutcome::TimedOut => return Ok(SyncStep::Deferred),
        }
        if is_cancelled() {
            return Ok(SyncStep::Cancelled);
        }
        hw.buttons.remove(&b.code);
        mouse.set_button(profile, b.code, want_func)?;
        hw.buttons.insert(b.code, Some(want_func));
    }

    if is_cancelled() {
        return Ok(SyncStep::Cancelled);
    }
    game.commit_sync(wanted.clone(), game_stage);
    Ok(SyncStep::Committed)
}

fn worker_loop(hwnd: HWND, shared: Arc<WorkerShared>) {
    let mut mouse: Option<Mouse> = None;
    let mut hw = HwCache::default();
    let mut game = GameSync::default();
    let mut seen_cache_rev = 0u64;
    let mut backoff = false;

    loop {
        struct Work {
            epoch: u64,
            reopen: bool,
            unplugged: Option<String>,
            cache_stale: bool,
            config: Config,
            config_ok: bool,
            need_import: bool,
            need_apply: bool,
            from_disk_reload: bool,
            wanted_game: Option<String>,
            has_game_rules: bool,
            want_refresh: bool,
        }

        let work = {
            let mut g = shared.goal.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if g.quit {
                    return;
                }
                if g.wake_probe {
                    backoff = false;
                }
                let cache_stale = g.cache_rev != seen_cache_rev;
                let has_dev = g.reopen || g.unplugged.is_some();
                let need_import = g.need_import;
                let need_apply = g.need_base_apply && g.config_ok;
                let need_game = game.needs_sync(&g.wanted_game) && g.config_ok;
                let has_work = has_dev || cache_stale || g.want_refresh || (!backoff && (need_import || need_apply || need_game));
                if has_work {
                    break;
                }
                g = shared.cvar.wait(g).unwrap_or_else(|e| e.into_inner());
            }
            let cache_stale = g.cache_rev != seen_cache_rev;
            seen_cache_rev = g.cache_rev;
            g.wake_probe = false;
            Work {
                epoch: g.epoch,
                reopen: mem::replace(&mut g.reopen, false),
                unplugged: g.unplugged.take(),
                cache_stale,
                config: g.config.clone(),
                config_ok: g.config_ok,
                need_import: g.need_import,
                need_apply: g.need_base_apply && g.config_ok,
                from_disk_reload: mem::replace(&mut g.notify_disk_reload, false),
                wanted_game: g.wanted_game.clone(),
                has_game_rules: g.has_game_rules,
                want_refresh: mem::replace(&mut g.want_refresh, false),
            }
        };

        if let Some(path) = work.unplugged {
            if mouse.as_ref().is_some_and(|m| m.path().eq_ignore_ascii_case(&path)) {
                mouse = None;
                hw.clear();
                shared.push_report(hwnd, WorkerReport::Device { connected: false, model_name: None, config_file: config_path(), unplugged_error: true });
            }
        }
        if work.reopen {
            mouse = None;
            hw.clear();
        }
        if work.cache_stale {
            hw.clear();
            if work.has_game_rules || game.applied.is_some() {
                game.begin_sync();
            }
        }

        if let Some(m) = mouse.as_mut() {
            m.set_write_access(&work.config);
        }
        if mouse.is_none() {
            match Mouse::open() {
                Ok(mut m) => {
                    m.set_write_access(&work.config);
                    let model_name = Some(m.spec().name.to_string());
                    let cfg_file = config_path();
                    hw.clear();
                    let was_dpi_overridden = game.dpi_overridden || game.applied.is_some();
                    game = GameSync { applied: None, dirty: work.has_game_rules || was_dpi_overridden, dpi_overridden: was_dpi_overridden };
                    mouse = Some(m);
                    shared.push_report(hwnd, WorkerReport::Device { connected: true, model_name, config_file: cfg_file, unplugged_error: false });
                }
                Err(e) => {
                    backoff = true;
                    shared.push_report(hwnd, WorkerReport::Refresh { base_applied: None, from_disk_reload: false, result: Err(e) });
                    continue;
                }
            }
        }

        let m = mouse.as_ref().unwrap();

        // 1. Initial import when no config file existed yet.
        if work.need_import {
            match apply::import(m) {
                Ok(cfg) => {
                    let stages = m.dpi_stages().ok();
                    let dpi = m.dpi().ok();
                    hw.seed_from_base(m.spec(), &cfg, cfg.polling.rate, stages, dpi);
                    game.on_base_applied(!cfg.games.is_empty() || !cfg.polling.games.is_empty());
                    {
                        let mut g = shared.goal.lock().unwrap_or_else(|e| e.into_inner());
                        g.need_import = false;
                    }
                    backoff = false;
                    shared.push_report(hwnd, WorkerReport::Imported(Box::new(Ok(cfg))));
                }
                Err(e) => {
                    backoff = true;
                    if matches!(e, HidError::Win32(..) | HidError::NotFound) {
                        mouse = None;
                        hw.clear();
                    }
                    shared.push_report(hwnd, WorkerReport::Imported(Box::new(Err(e.to_string()))));
                }
            }
            continue;
        }

        // 2. Startup / disk-reload base `apply()`.
        let mut did_base_refresh = false;
        if work.need_apply {
            let writes_ok = m.writes_allowed();
            if !writes_ok {
                let mut g = shared.goal.lock().unwrap_or_else(|e| e.into_inner());
                g.need_base_apply = false;
            } else {
                let base_polling = work.config.polling.rate;
                match apply::apply(m, &work.config, base_polling) {
                    Ok(changes) => {
                        let want_stages = work.config.stage_pairs();
                        let want_active = work.config.dpi.active.clamp(1, want_stages.len() as u8);
                        let want_dpi = want_stages.get(want_active as usize - 1).copied();
                        let stages = Some(DpiStages { active: want_active, stages: want_stages });
                        hw.seed_from_base(m.spec(), &work.config, base_polling, stages.clone(), want_dpi);
                        game.on_base_applied(work.has_game_rules);
                        {
                            let mut g = shared.goal.lock().unwrap_or_else(|e| e.into_inner());
                            g.need_base_apply = false;
                        }
                        backoff = false;
                        if let Ok(bat) = m.battery() {
                            did_base_refresh = true;
                            shared.push_report(
                                hwnd,
                                WorkerReport::Refresh {
                                    base_applied: Some(changes.len()),
                                    from_disk_reload: work.from_disk_reload,
                                    result: Ok((bat, stages, Some(base_polling))),
                                },
                            );
                        }
                    }
                    Err(e) => {
                        backoff = true;
                        hw.clear();
                        if matches!(e, HidError::Invalid(_) | HidError::Unsupported(_) | HidError::Unverified(_)) {
                            let mut g = shared.goal.lock().unwrap_or_else(|e| e.into_inner());
                            g.config_ok = false;
                        }
                        let gone = matches!(e, HidError::Win32(..) | HidError::NotFound);
                        shared.push_report(hwnd, WorkerReport::Refresh { base_applied: None, from_disk_reload: false, result: Err(e) });
                        if gone {
                            mouse = None;
                            continue;
                        }
                    }
                }
            }
        }

        // 3. Per-game profile sync (top priority over periodic battery refresh; cancellable on newer `epoch`).
        if let Some(m) = mouse.as_ref()
            && work.config_ok
            && game.needs_sync(&work.wanted_game)
        {
            let my_epoch = work.epoch;
            let is_cancelled = || shared.goal.lock().unwrap_or_else(|e| e.into_inner()).epoch != my_epoch;
            let sleep_step = |ms: u64| std::thread::sleep(Duration::from_millis(ms));
            match sync_game_target(m, &work.config, &work.wanted_game, &mut game, &mut hw, any_vk_pressed_os, is_cancelled, sleep_step) {
                Ok(SyncStep::Committed) => {
                    backoff = false;
                    shared.push_report(
                        hwnd,
                        WorkerReport::Game {
                            epoch: my_epoch,
                            applied: game.applied.clone(),
                            dirty: false,
                            deferred: false,
                            polling: hw.polling,
                            stages: hw.stages.clone(),
                            error: None,
                        },
                    );
                }
                Ok(SyncStep::Cancelled) => {
                    // Newer foreground target or config already arrived: loop immediately without backoff.
                    backoff = false;
                    continue;
                }
                Ok(SyncStep::Deferred) => {
                    // Guarded shortcut/button stayed held past max_wait: keep current hardware binding untouched and retry shortly.
                    backoff = true;
                    shared.push_report(
                        hwnd,
                        WorkerReport::Game {
                            epoch: my_epoch,
                            applied: game.applied.clone(),
                            dirty: true,
                            deferred: true,
                            polling: hw.polling,
                            stages: hw.stages.clone(),
                            error: None,
                        },
                    );
                }
                Err(e) => {
                    backoff = true;
                    if matches!(e, HidError::Invalid(_) | HidError::Unsupported(_) | HidError::Unverified(_)) {
                        let mut g = shared.goal.lock().unwrap_or_else(|e| e.into_inner());
                        g.config_ok = false;
                    }
                    let msg = e.to_string();
                    let gone = matches!(e, HidError::Win32(..) | HidError::NotFound);
                    if gone {
                        mouse = None;
                        hw.clear();
                    }
                    shared.push_report(
                        hwnd,
                        WorkerReport::Game {
                            epoch: my_epoch,
                            applied: game.applied.clone(),
                            dirty: true,
                            deferred: false,
                            polling: hw.polling,
                            stages: hw.stages.clone(),
                            error: Some(msg),
                        },
                    );
                    if gone {
                        continue;
                    }
                }
            }
        }

        // 4. Periodic battery / live status refresh (skipped if already refreshed above or superseded by a newer epoch).
        let Some(m) = mouse.as_ref() else { continue };
        if work.want_refresh && !did_base_refresh && shared.goal.lock().unwrap_or_else(|e| e.into_inner()).epoch == work.epoch {
            match m.battery() {
                Ok(bat) => {
                    backoff = false;
                    let mut stages = hw.stages.clone();
                    if shared.goal.lock().unwrap_or_else(|e| e.into_inner()).epoch == work.epoch {
                        if let Ok(s) = m.dpi_stages() {
                            let stage_changed = hw.stages.as_ref() != Some(&s);
                            hw.stages = Some(s.clone());
                            stages = Some(s);
                            if (hw.dpi.is_none() || stage_changed)
                                && let Ok(d) = m.dpi()
                            {
                                hw.dpi = Some(d);
                            }
                        }
                    }
                    let polling = match hw.polling {
                        Some(p) => Some(p),
                        None if shared.goal.lock().unwrap_or_else(|e| e.into_inner()).epoch == work.epoch => m.polling_rate().ok(),
                        None => None,
                    };
                    if polling.is_some() {
                        hw.polling = polling;
                    }
                    shared.push_report(hwnd, WorkerReport::Refresh { base_applied: None, from_disk_reload: false, result: Ok((bat, stages, polling)) });
                }
                Err(e) => {
                    backoff = true;
                    let gone = matches!(e, HidError::Win32(..) | HidError::NotFound);
                    shared.push_report(hwnd, WorkerReport::Refresh { base_applied: None, from_disk_reload: false, result: Err(e) });
                    if gone {
                        mouse = None;
                        hw.clear();
                    }
                }
            }
        }
    }
}

/// Out-of-context foreground / un-minimize notification: zero DLL injection, zero polling.
unsafe extern "system" fn win_event_proc(_hook: HWINEVENTHOOK, _event: u32, _hwnd: HWND, id_object: i32, _id_child: i32, _thread: u32, _time: u32) {
    if id_object != OBJID_WINDOW {
        return;
    }
    let tray = TRAY_HWND.load(Ordering::Relaxed);
    if tray != 0 {
        // 80 ms one-shot debounce: switches swiftly while coalescing rapid Alt+Tab hops.
        SetTimer(tray, TIMER_GAME, 80, None);
    }
}

enum FgQuery {
    /// `GetForegroundWindow()` returned 0 transiently during a focus / fullscreen switch.
    TransientEmpty,
    /// Foreground window exists (`Some(path)` when readable, `None` when protected system process).
    Window(Option<String>),
}

/// Query the executable owning the foreground window without requesting VM/debug rights.
unsafe fn query_foreground() -> FgQuery {
    let hwnd = GetForegroundWindow();
    if hwnd == 0 {
        return FgQuery::TransientEmpty;
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, &mut pid);
    if pid == 0 {
        return FgQuery::TransientEmpty;
    }
    let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
    if h == 0 {
        return FgQuery::Window(None);
    }
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len);
    CloseHandle(h);
    if ok == 0 {
        return FgQuery::Window(None);
    }
    FgQuery::Window(Some(String::from_utf16_lossy(&buf[..len as usize])))
}

// ---------------------------------------------------------------------------
// Hot-plug, resume, config file watching and the update check. The tray window
// is message-only, so broadcasts do not reach it: device and power events are
// registered for explicitly, and the two threads post their own messages.
// ---------------------------------------------------------------------------

/// WM_DEVICECHANGE for every HID interface that comes or goes.
unsafe fn register_device_notifications(hwnd: HWND) {
    use windows_sys::Win32::Devices::HumanInterfaceDevice::HidD_GetHidGuid;
    let mut filter: DEV_BROADCAST_DEVICEINTERFACE_W = mem::zeroed();
    filter.dbcc_size = mem::size_of::<DEV_BROADCAST_DEVICEINTERFACE_W>() as u32;
    filter.dbcc_devicetype = DBT_DEVTYP_DEVICEINTERFACE;
    HidD_GetHidGuid(&mut filter.dbcc_classguid);
    RegisterDeviceNotificationW(hwnd, &filter as *const _ as *const c_void, DEVICE_NOTIFY_WINDOW_HANDLE);
}

/// WM_POWERBROADCAST on suspend and resume.
unsafe fn register_resume_notifications(hwnd: HWND) {
    use windows_sys::Win32::System::Power::RegisterSuspendResumeNotification;
    RegisterSuspendResumeNotification(hwnd, DEVICE_NOTIFY_WINDOW_HANDLE);
}

/// The interface path carried by a WM_DEVICECHANGE, when it is about a device interface.
unsafe fn broadcast_interface_name(lparam: LPARAM) -> Option<String> {
    if lparam == 0 {
        return None;
    }
    let hdr = &*(lparam as *const DEV_BROADCAST_HDR);
    if hdr.dbch_devicetype != DBT_DEVTYP_DEVICEINTERFACE {
        return None;
    }
    let d = lparam as *const DEV_BROADCAST_DEVICEINTERFACE_W;
    let name_at = mem::offset_of!(DEV_BROADCAST_DEVICEINTERFACE_W, dbcc_name);
    let size = (*d).dbcc_size as usize;
    if size <= name_at {
        return None;
    }
    let units = std::slice::from_raw_parts(ptr::addr_of!((*d).dbcc_name) as *const u16, (size - name_at) / 2);
    let len = units.iter().position(|c| *c == 0).unwrap_or(units.len());
    Some(String::from_utf16_lossy(&units[..len]))
}

/// A thread that posts `WM_CONFIG_FILE` whenever something in the config
/// folder is written. The tray then decides whether the config file changed.
fn watch_config(hwnd: HWND) {
    let Some(dir) = config_path().parent().map(|p| p.to_path_buf()) else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    std::thread::spawn(move || unsafe {
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FindFirstChangeNotificationW, FindNextChangeNotification,
        };
        use windows_sys::Win32::System::Threading::{INFINITE, WaitForSingleObject};
        let path = wide(&dir.to_string_lossy());
        let h = FindFirstChangeNotificationW(path.as_ptr(), 0, FILE_NOTIFY_CHANGE_LAST_WRITE | FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_SIZE);
        if h == INVALID_HANDLE_VALUE || h == 0 {
            return;
        }
        loop {
            if WaitForSingleObject(h, INFINITE) != 0 {
                break;
            }
            PostMessageW(hwnd, WM_CONFIG_FILE, 0, 0);
            if FindNextChangeNotification(h) == 0 {
                break;
            }
        }
    });
}

/// Ask GitHub for the latest release on a thread; the answer arrives as `WM_UPDATE_RESULT`.
fn check_updates(hwnd: HWND) {
    std::thread::spawn(move || {
        let newer = crate::update::fetch_latest_tag().ok().filter(|t| crate::update::is_newer(t, crate::VERSION));
        let lparam = newer.map(|t| Box::into_raw(Box::new(t)) as LPARAM).unwrap_or(0);
        unsafe { PostMessageW(hwnd, WM_UPDATE_RESULT, 0, lparam) };
    });
}

// ---------------------------------------------------------------------------
// Icon rendering: a battery outline with the percentage drawn in a 3x5 font.
// ---------------------------------------------------------------------------

const FONT_3X5: [[u8; 5]; 10] = [
    [0b111, 0b101, 0b101, 0b101, 0b111], // 0
    [0b010, 0b110, 0b010, 0b010, 0b111], // 1
    [0b111, 0b001, 0b111, 0b100, 0b111], // 2
    [0b111, 0b001, 0b111, 0b001, 0b111], // 3
    [0b101, 0b101, 0b111, 0b001, 0b001], // 4
    [0b111, 0b100, 0b111, 0b001, 0b111], // 5
    [0b111, 0b100, 0b111, 0b101, 0b111], // 6
    [0b111, 0b001, 0b001, 0b010, 0b010], // 7
    [0b111, 0b101, 0b111, 0b101, 0b111], // 8
    [0b111, 0b101, 0b111, 0b001, 0b111], // 9
];

struct Canvas {
    w: i32,
    h: i32,
    px: Vec<[u8; 4]>, // BGRA, premultiplied (alpha is 0 or 255 here)
}

impl Canvas {
    fn new(w: i32, h: i32) -> Canvas {
        Canvas { w, h, px: vec![[0, 0, 0, 0]; (w * h) as usize] }
    }
    fn set(&mut self, x: i32, y: i32, rgb: (u8, u8, u8)) {
        if x >= 0 && y >= 0 && x < self.w && y < self.h {
            self.px[(y * self.w + x) as usize] = [rgb.2, rgb.1, rgb.0, 255];
        }
    }
    fn rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, rgb: (u8, u8, u8)) {
        for y in y0..y1 {
            for x in x0..x1 {
                self.set(x, y, rgb);
            }
        }
    }
    fn digit(&mut self, d: u8, x: i32, y: i32, scale: i32, rgb: (u8, u8, u8)) {
        let glyph = FONT_3X5[(d % 10) as usize];
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..3 {
                if bits & (0b100 >> col) != 0 {
                    self.rect(x + col * scale, y + row as i32 * scale, x + (col + 1) * scale, y + (row as i32 + 1) * scale, rgb);
                }
            }
        }
    }
}

unsafe fn make_battery_icon(battery: Option<Battery>, connected: bool) -> HICON {
    let size = GetSystemMetrics(SM_CXSMICON).max(16);
    let scale = (size / 16).max(1);
    let mut c = Canvas::new(size, size);

    let white = (235, 235, 235);
    let grey = (140, 140, 140);
    let color = match battery {
        Some(b) if b.charging || b.wired => (80, 160, 255),
        Some(b) if b.percent <= 15 => (235, 60, 60),
        Some(b) if b.percent <= 35 => (240, 180, 40),
        Some(_) => (90, 200, 90),
        None => grey,
    };

    // Battery body: outline 1px, with a nub on the right; occupies rows 2..14 of a 16px grid.
    let x0 = 0;
    let y0 = 2 * scale;
    let x1 = 14 * scale;
    let y1 = 14 * scale;
    let outline = if connected { white } else { grey };
    c.rect(x0, y0, x1, y0 + scale, outline);
    c.rect(x0, y1 - scale, x1, y1, outline);
    c.rect(x0, y0, x0 + scale, y1, outline);
    c.rect(x1 - scale, y0, x1, y1, outline);
    c.rect(x1, y0 + 3 * scale, x1 + 2 * scale, y1 - 3 * scale, outline);

    match battery {
        Some(b) => {
            // Fill level along the bottom edge inside the outline.
            let inner_w = x1 - x0 - 2 * scale;
            let fill = (inner_w * b.percent as i32 / 100).max(if b.percent > 0 { scale } else { 0 });
            c.rect(x0 + scale, y1 - 3 * scale, x0 + scale + fill, y1 - scale, color);
            if b.wired {
                return canvas_to_icon(&c);
            }
            // Percentage digits, centred in the upper area.
            let digits: Vec<u8> = if b.percent >= 100 {
                vec![1, 0, 0]
            } else if b.percent >= 10 {
                vec![b.percent / 10, b.percent % 10]
            } else {
                vec![b.percent]
            };
            let text_w = (digits.len() as i32 * 4 - 1) * scale;
            let mut x = x0 + (x1 - x0 - text_w) / 2;
            let y = y0 + 2 * scale;
            for d in digits {
                c.digit(d, x, y, scale, white);
                x += 4 * scale;
            }
        }
        None => {
            // A dash when nothing is known.
            c.rect(x0 + 4 * scale, (y0 + y1) / 2, x1 - 4 * scale, (y0 + y1) / 2 + scale, grey);
        }
    }
    canvas_to_icon(&c)
}

unsafe fn canvas_to_icon(c: &Canvas) -> HICON {
    let hdc = GetDC(0);
    let mut bmi: BITMAPINFO = mem::zeroed();
    bmi.bmiHeader.biSize = mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = c.w;
    bmi.bmiHeader.biHeight = -c.h;
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = BI_RGB;
    let mut bits: *mut c_void = ptr::null_mut();
    let color = CreateDIBSection(hdc, &bmi, DIB_RGB_COLORS, &mut bits, 0, 0);
    ReleaseDC(0, hdc);
    if color == 0 || bits.is_null() {
        return 0;
    }
    ptr::copy_nonoverlapping(c.px.as_ptr() as *const u8, bits as *mut u8, c.px.len() * 4);
    let mask = CreateBitmap(c.w, c.h, 1, 1, ptr::null());
    let info = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
    let icon = CreateIconIndirect(&info);
    DeleteObject(color);
    DeleteObject(mask);
    icon
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    use super::{FENCE_MAX_WAIT_MS, FENCE_QUIET_HELD_MS, FENCE_STEP_MS, GameSync, HwCache, SyncBackend, SyncStep, sync_game_target};
    use crate::config::{Config, GameConfig, parse_button};
    use crate::device::{ButtonFunction, DeviceSpec, DpiStages};
    use crate::hid::HidError;
    use crate::protocol::Status;

    #[test]
    fn base_reapply_marks_game_dirty_until_foreground_resynced() {
        let mut gs = GameSync::default();
        gs.on_base_applied(true);
        assert!(gs.dirty, "base apply must keep game sync dirty when game rules exist");
        assert!(gs.needs_sync(&Some("c:\\games\\shooter".into())));
        assert!(gs.needs_sync(&None));
        gs.begin_sync();
        gs.commit_sync(None, None);
        assert!(!gs.dirty);
        assert!(!gs.needs_sync(&None));
    }

    #[test]
    fn partial_game_failure_still_restores_base_and_dpi_on_exit() {
        let mut gs = GameSync::default();
        gs.commit_sync(None, None);

        let wanted_game = Some("c:\\games\\shooter".to_string());
        assert!(gs.needs_sync(&wanted_game));
        gs.begin_sync();
        assert_eq!(gs.plan_dpi_stage(2, Some(1), true), Some(1));
        assert!(gs.dpi_overridden);
        assert_eq!(gs.applied, None);
        assert!(gs.dirty);

        assert!(gs.needs_sync(&None));
        gs.begin_sync();
        assert_eq!(gs.plan_dpi_stage(2, None, true), Some(2));
        gs.commit_sync(None, None);
        assert!(!gs.dpi_overridden);
        assert!(!gs.dirty);
    }

    #[test]
    fn desktop_dpi_stage_is_restored_across_multiple_games() {
        let mut gs = GameSync::default();
        gs.commit_sync(None, None);

        let game_a = Some("game:a".to_string());
        gs.begin_sync();
        assert_eq!(gs.plan_dpi_stage(2, None, false), None);
        assert!(!gs.dpi_overridden);
        gs.commit_sync(game_a, None);

        let game_b = Some("game:b".to_string());
        gs.begin_sync();
        assert_eq!(gs.plan_dpi_stage(2, Some(4), true), Some(4));
        assert!(gs.dpi_overridden);
        gs.commit_sync(game_b, Some(4));

        // Switching to a game without its own `dpi_stage` restores desktop stage 2 immediately.
        let game_c = Some("game:c".to_string());
        gs.begin_sync();
        assert_eq!(gs.plan_dpi_stage(2, None, true), Some(2));
        gs.commit_sync(game_c, None);
        assert!(!gs.dpi_overridden);

        gs.begin_sync();
        assert_eq!(gs.plan_dpi_stage(2, None, true), Some(2));
        gs.commit_sync(None, None);
        assert!(!gs.dpi_overridden);
    }

    struct MockMouse {
        spec: DeviceSpec,
        polling: RefCell<u16>,
        dpi: RefCell<(u16, u16)>,
        stages: RefCell<DpiStages>,
        buttons: RefCell<BTreeMap<u8, ButtonFunction>>,
        ops: RefCell<Vec<String>>,
        fail_set_dpi_xy_once: RefCell<bool>,
        fail_set_button: RefCell<Option<u8>>,
        on_after_set_polling: RefCell<Option<Box<dyn FnMut()>>>,
    }

    impl MockMouse {
        fn new_dav4() -> Self {
            let spec = DeviceSpec::lookup(0x00BF);
            let mut buttons = BTreeMap::new();
            for b in spec.buttons {
                buttons.insert(b.code, b.default);
            }
            buttons.insert(0x04, parse_button("media:mute").unwrap().unwrap());
            buttons.insert(0x05, parse_button("key:win+d").unwrap().unwrap());
            MockMouse {
                spec,
                polling: RefCell::new(500),
                dpi: RefCell::new((3000, 3000)),
                stages: RefCell::new(DpiStages { active: 2, stages: vec![(1000, 1000), (3000, 3000)] }),
                buttons: RefCell::new(buttons),
                ops: RefCell::new(Vec::new()),
                fail_set_dpi_xy_once: RefCell::new(false),
                fail_set_button: RefCell::new(None),
                on_after_set_polling: RefCell::new(None),
            }
        }

        fn take_ops(&self) -> Vec<String> {
            std::mem::take(&mut *self.ops.borrow_mut())
        }
    }

    impl SyncBackend for MockMouse {
        fn spec(&self) -> &DeviceSpec {
            &self.spec
        }
        fn profile(&self) -> u8 {
            1
        }
        fn writes_allowed(&self) -> bool {
            true
        }
        fn polling_rate(&self) -> crate::hid::Result<u16> {
            self.ops.borrow_mut().push("get_polling".into());
            Ok(*self.polling.borrow())
        }
        fn set_polling_rate(&self, hz: u16) -> crate::hid::Result<()> {
            self.ops.borrow_mut().push(format!("set_polling:{hz}"));
            *self.polling.borrow_mut() = hz;
            if let Some(cb) = self.on_after_set_polling.borrow_mut().as_mut() {
                cb();
            }
            Ok(())
        }
        fn dpi(&self) -> crate::hid::Result<(u16, u16)> {
            self.ops.borrow_mut().push("get_dpi".into());
            Ok(*self.dpi.borrow())
        }
        fn dpi_stages(&self) -> crate::hid::Result<DpiStages> {
            self.ops.borrow_mut().push("get_stages".into());
            Ok(self.stages.borrow().clone())
        }
        fn set_dpi_stages(&self, active: u8, stages: &[(u16, u16)]) -> crate::hid::Result<()> {
            self.ops.borrow_mut().push(format!("set_stages:{active}"));
            *self.stages.borrow_mut() = DpiStages { active, stages: stages.to_vec() };
            if *self.fail_set_dpi_xy_once.borrow() {
                *self.fail_set_dpi_xy_once.borrow_mut() = false;
                return Err(HidError::Status(Status::Timeout));
            }
            if let Some(&xy) = stages.get(active.saturating_sub(1) as usize) {
                *self.dpi.borrow_mut() = xy;
            }
            Ok(())
        }
        fn current_button(&self, code: u8, _name: &str, _bound: bool) -> crate::hid::Result<Option<ButtonFunction>> {
            self.ops.borrow_mut().push(format!("get_btn:{code:02x}"));
            Ok(self.buttons.borrow().get(&code).copied())
        }
        fn set_button(&self, _profile: u8, code: u8, f: ButtonFunction) -> crate::hid::Result<()> {
            self.ops.borrow_mut().push(format!("set_btn:{code:02x}"));
            if *self.fail_set_button.borrow() == Some(code) {
                *self.fail_set_button.borrow_mut() = None;
                return Err(HidError::Status(Status::Timeout));
            }
            self.buttons.borrow_mut().insert(code, f);
            Ok(())
        }
    }

    fn overwatch_config() -> (Config, String) {
        let mut cfg = Config::default();
        cfg.dpi.stages = vec![1000, 3000];
        cfg.dpi.active = 2;
        cfg.polling.rate = 500;
        cfg.buttons.insert("back".into(), "media:mute".into());
        cfg.buttons.insert("forward".into(), "key:win+d".into());
        let mut g = GameConfig { name: "守望先锋".into(), dir: "D:\\Overwatch".into(), dpi_stage: Some(1), polling: Some(1000), ..Default::default() };
        g.buttons.insert("back".into(), "key:j".into());
        g.buttons.insert("forward".into(), "key:f".into());
        let key = g.key();
        cfg.games.push(g);
        (cfg, key)
    }

    #[test]
    fn warm_cache_differential_switch_skips_unchanged_reads_and_writes() {
        let (cfg, ow_key) = overwatch_config();
        let mock = MockMouse::new_dav4();
        let mut hw = HwCache::default();
        hw.seed_from_base(mock.spec(), &cfg, 500, Some(mock.stages.borrow().clone()), Some((3000, 3000)));
        let mut gs = GameSync::default();
        gs.commit_sync(None, None);

        // Desktop -> Overwatch: 0 GETs at all! Only writes polling, stage 1 (1000 DPI), and the 2 side buttons.
        let step = sync_game_target(&mock, &cfg, &Some(ow_key.clone()), &mut gs, &mut hw, |_| false, || false, |_| {}).unwrap();
        assert_eq!(step, SyncStep::Committed);
        assert_eq!(mock.take_ops(), vec!["set_polling:1000", "set_stages:1", "set_btn:04", "set_btn:05"]);
        assert_eq!(*mock.dpi.borrow(), (1000, 1000));

        // Overwatch -> Desktop: 0 GETs at all! Only writes polling, stage 2 (3000 DPI), and the 2 side buttons.
        let step = sync_game_target(&mock, &cfg, &None, &mut gs, &mut hw, |_| false, || false, |_| {}).unwrap();
        assert_eq!(step, SyncStep::Committed);
        assert_eq!(mock.take_ops(), vec!["set_polling:500", "set_stages:2", "set_btn:04", "set_btn:05"]);
        assert_eq!(*mock.dpi.borrow(), (3000, 3000));
    }

    #[test]
    fn mid_game_reopen_or_restart_never_poisons_desktop_dpi_on_exit() {
        let (cfg, ow_key) = overwatch_config();
        let mock = MockMouse::new_dav4();
        let mut hw = HwCache::default();
        hw.seed_from_base(mock.spec(), &cfg, 500, Some(mock.stages.borrow().clone()), Some((3000, 3000)));
        let mut gs = GameSync::default();
        gs.commit_sync(None, None);

        // Enter Overwatch -> mouse is now at Stage 1 (1000 DPI), back=J, forward=F.
        sync_game_target(&mock, &cfg, &Some(ow_key.clone()), &mut gs, &mut hw, |_| false, || false, |_| {}).unwrap();
        mock.take_ops();
        assert_eq!(mock.stages.borrow().active, 1);
        assert_eq!(*mock.dpi.borrow(), (1000, 1000));

        // Simulate device reopen or app restart mid-game (cache cleared, GameSync fresh while hardware is ALREADY at Stage 1!).
        hw.clear();
        gs = GameSync { applied: None, dirty: true, dpi_overridden: false };
        let step = sync_game_target(&mock, &cfg, &Some(ow_key), &mut gs, &mut hw, |_| false, || false, |_| {}).unwrap();
        assert_eq!(step, SyncStep::Committed);
        mock.take_ops();

        // Exit Overwatch to Desktop -> MUST restore Stage 2 (3000 DPI) as well as buttons!
        let step = sync_game_target(&mock, &cfg, &None, &mut gs, &mut hw, |_| false, || false, |_| {}).unwrap();
        assert_eq!(step, SyncStep::Committed);
        assert_eq!(mock.take_ops(), vec!["set_polling:500", "set_stages:2", "set_btn:04", "set_btn:05"]);
        assert_eq!(mock.stages.borrow().active, 2);
        assert_eq!(*mock.dpi.borrow(), (3000, 3000));
    }

    #[test]
    fn partial_dpi_xy_timeout_retries_set_stages_even_when_stage_index_updated() {
        let (cfg, ow_key) = overwatch_config();
        let mock = MockMouse::new_dav4();
        let mut hw = HwCache::default();
        hw.seed_from_base(mock.spec(), &cfg, 500, Some(mock.stages.borrow().clone()), Some((3000, 3000)));
        let mut gs = GameSync::default();
        gs.commit_sync(None, None);

        sync_game_target(&mock, &cfg, &Some(ow_key), &mut gs, &mut hw, |_| false, || false, |_| {}).unwrap();
        mock.take_ops();

        // Simulate `04/06` updating `stages.active = 2` while `04/05` (`set_dpi_xy`) times out leaving live DPI at `(1000, 1000)`.
        *mock.fail_set_dpi_xy_once.borrow_mut() = true;
        let res = sync_game_target(&mock, &cfg, &None, &mut gs, &mut hw, |_| false, || false, |_| {});
        assert!(res.is_err());
        assert_eq!(mock.stages.borrow().active, 2);
        assert_eq!(*mock.dpi.borrow(), (1000, 1000));

        // Next retry detects `dpi != (3000, 3000)` and re-runs `set_stages:2` to bring live DPI in sync.
        let step = sync_game_target(&mock, &cfg, &None, &mut gs, &mut hw, |_| false, || false, |_| {}).unwrap();
        assert_eq!(step, SyncStep::Committed);
        assert_eq!(mock.take_ops(), vec!["set_polling:500", "set_stages:2", "get_stages", "get_dpi", "set_stages:2", "set_btn:04", "set_btn:05"]);
        assert_eq!(*mock.dpi.borrow(), (3000, 3000));
    }

    #[test]
    fn cancelled_desktop_switch_converges_back_to_game_without_touching_buttons() {
        let (cfg, ow_key) = overwatch_config();
        let mock = MockMouse::new_dav4();
        let mut hw = HwCache::default();
        hw.seed_from_base(mock.spec(), &cfg, 500, Some(mock.stages.borrow().clone()), Some((3000, 3000)));
        let mut gs = GameSync::default();
        gs.commit_sync(None, None);

        sync_game_target(&mock, &cfg, &Some(ow_key.clone()), &mut gs, &mut hw, |_| false, || false, |_| {}).unwrap();
        mock.take_ops();

        let cancel_flag = std::rc::Rc::new(std::cell::Cell::new(false));
        let flag_clone = cancel_flag.clone();
        *mock.on_after_set_polling.borrow_mut() = Some(Box::new(move || flag_clone.set(true)));
        let step = sync_game_target(&mock, &cfg, &None, &mut gs, &mut hw, |_| false, || cancel_flag.get(), |_| {}).unwrap();
        assert_eq!(step, SyncStep::Cancelled);
        assert_eq!(mock.take_ops(), vec!["set_polling:500"]);

        *mock.on_after_set_polling.borrow_mut() = None;
        let step = sync_game_target(&mock, &cfg, &Some(ow_key), &mut gs, &mut hw, |_| false, || false, |_| {}).unwrap();
        assert_eq!(step, SyncStep::Committed);
        assert_eq!(mock.take_ops(), vec!["set_polling:1000"]);
    }

    #[test]
    fn held_win_d_shortcut_delays_switch_until_quiet_and_defers_on_timeout() {
        let (cfg, ow_key) = overwatch_config();
        let mock = MockMouse::new_dav4();
        let mut hw = HwCache::default();
        hw.seed_from_base(mock.spec(), &cfg, 500, Some(mock.stages.borrow().clone()), Some((3000, 3000)));
        let mut gs = GameSync::default();
        gs.commit_sync(None, None);

        let mut slept_ms = 0u64;
        let step = sync_game_target(&mock, &cfg, &Some(ow_key.clone()), &mut gs, &mut hw, |vks| vks.contains(&0x5b), || false, |ms| slept_ms += ms).unwrap();
        assert_eq!(step, SyncStep::Deferred);
        assert!(slept_ms >= FENCE_MAX_WAIT_MS);
        assert!(mock.take_ops().is_empty(), "must never rewrite mouse while Win+D is held down");
        assert!(gs.dirty);

        let elapsed = std::cell::Cell::new(0u64);
        let step = sync_game_target(
            &mock,
            &cfg,
            &Some(ow_key),
            &mut gs,
            &mut hw,
            |vks| elapsed.get() <= 3 * FENCE_STEP_MS && vks.contains(&0x44),
            || false,
            |ms| elapsed.set(elapsed.get() + ms),
        )
        .unwrap();
        assert_eq!(step, SyncStep::Committed);
        assert_eq!(elapsed.get(), 3 * FENCE_STEP_MS + FENCE_QUIET_HELD_MS);
        assert_eq!(mock.take_ops(), vec!["set_polling:1000", "set_stages:1", "set_btn:04", "set_btn:05"]);
    }

    #[test]
    fn partial_button_timeout_invalidates_only_failed_button() {
        let (cfg, ow_key) = overwatch_config();
        let mock = MockMouse::new_dav4();
        let mut hw = HwCache::default();
        hw.seed_from_base(mock.spec(), &cfg, 500, Some(mock.stages.borrow().clone()), Some((3000, 3000)));
        let mut gs = GameSync::default();
        gs.commit_sync(None, None);

        *mock.fail_set_button.borrow_mut() = Some(0x05);
        let res = sync_game_target(&mock, &cfg, &Some(ow_key.clone()), &mut gs, &mut hw, |_| false, || false, |_| {});
        assert!(res.is_err());
        assert_eq!(mock.take_ops(), vec!["set_polling:1000", "set_stages:1", "set_btn:04", "set_btn:05"]);
        assert!(gs.dirty);
        assert!(!hw.buttons.contains_key(&0x05), "timed-out button must be marked unknown");
        assert!(hw.buttons.contains_key(&0x04), "already-written button stays cached");

        let step = sync_game_target(&mock, &cfg, &Some(ow_key), &mut gs, &mut hw, |_| false, || false, |_| {}).unwrap();
        assert_eq!(step, SyncStep::Committed);
        assert_eq!(mock.take_ops(), vec!["get_btn:05", "set_btn:05"]);
    }
}
