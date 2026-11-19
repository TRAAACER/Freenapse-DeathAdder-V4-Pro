//! Tray application: battery icon, DPI stage / polling rate switching,
//! per-game profiles, software macros, low-battery notification. Pure Win32.
//! Started by `freenapse.exe` when it is launched without arguments.

#![allow(non_snake_case, unsafe_op_in_unsafe_fn)]

use std::ffi::c_void;
use std::mem;
use std::ptr;
use std::time::Instant;

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::{CreateMutexW, GetCurrentThreadId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
use windows_sys::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows_sys::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::config::Config;
use crate::device::{Battery, DpiStages, Mouse, RATES_HYPERPOLLING};
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

const ID_QUIT: usize = 1;
const ID_APPLY: usize = 2;
const ID_OPEN_CONFIG: usize = 3;
const ID_REFRESH: usize = 4;
const ID_SETTINGS: usize = 5;
const ID_UPDATE: usize = 6;
const ID_AUTOSTART: usize = 7;
const ID_ONBOARD_ONLY: usize = 8;
const ID_STAGE_BASE: usize = 100;
const ID_POLL_BASE: usize = 200;

const RETRY_BASE_MS: u32 = 1200;
const RETRY_MAX_MS: u32 = 4800;

/// Pure per-game profile state machine, separated from Win32/HID for deterministic testing.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
struct GameSync {
    /// Game profile confirmed written in full to the hardware (`None` = base layout confirmed).
    applied: Option<String>,
    /// True while hardware may hold a partial/unconfirmed game layout, or right after base `apply()` ran.
    dirty: bool,
    /// User's DPI stage before the first game DPI override took effect; restored when leaving DPI-overridden games.
    stage_before_game: Option<u8>,
    /// Consecutive transient `GetForegroundWindow() == 0` checks (bounded to 3 before treating as desktop).
    fg_empty_retries: u8,
}

impl GameSync {
    fn on_base_applied(&mut self, has_rules: bool) {
        self.applied = None;
        self.dirty = has_rules;
    }

    fn needs_sync(&self, wanted: &Option<String>) -> bool {
        self.dirty || *wanted != self.applied
    }

    fn begin_sync(&mut self) {
        self.dirty = true;
    }

    /// Decide which 1-based DPI stage to write (`None` = leave DPI stage untouched),
    /// persistently recording `stage_before_game` before the first game DPI override.
    fn plan_dpi_stage(&mut self, game_stage: Option<u8>, has_game: bool, current_active: u8) -> Option<u8> {
        match game_stage {
            Some(s) => {
                if self.stage_before_game.is_none() {
                    self.stage_before_game = Some(current_active);
                }
                Some(s)
            }
            None if has_game => None,
            None => self.stage_before_game,
        }
    }

    fn commit_sync(&mut self, wanted: Option<String>) {
        if wanted.is_none() {
            self.stage_before_game = None;
        }
        self.applied = wanted;
        self.dirty = false;
    }
}

struct App {
    hwnd: HWND,
    nid: NOTIFYICONDATAW,
    hicon: HICON,
    config: Config,
    config_error: Option<String>,
    mouse: Option<Mouse>,
    onboard_macros: bool,
    battery: Option<Battery>,
    stages: Option<DpiStages>,
    polling: Option<u16>,
    config_applied: bool,
    /// No config file yet: build one from the mouse's current settings once it answers.
    need_import: bool,
    /// The file `config` was loaded from: another mouse has another file.
    config_file: std::path::PathBuf,
    game: GameSync,
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
}

static TRAY_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

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

        load_soft_macros(&config, None, true);
        let poll_secs = config.tray.poll_seconds.max(5);
        let app = Box::new(App {
            hwnd: 0,
            nid: mem::zeroed(),
            hicon: 0,
            config,
            config_error,
            mouse: None,
            onboard_macros: true,
            battery: None,
            stages: None,
            polling: None,
            config_applied: false,
            need_import,
            config_file: config_path(),
            game: GameSync::default(),
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
            (*app).refresh();
            (*app).sync_fg_hooks();
            (*app).check_game();
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
                    (*app).refresh();
                    if (*app).game.dirty {
                        (*app).check_game();
                    }
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
                    // every 4th tick also probes once in case the mouse was already awake without cursor motion.
                    if user_active() || (*app).retry_ticks.is_multiple_of(4) {
                        let need_base = (*app).need_import || (!(*app).config_applied && (*app).config_error.is_none()) || (*app).battery.is_none();
                        if need_base {
                            (*app).refresh();
                        }
                        if (*app).game.dirty {
                            (*app).check_game();
                        }
                    }
                    (*app).sync_retry_timer();
                }
                TIMER_REPLUG => {
                    KillTimer(hwnd, TIMER_REPLUG);
                    // The handle may be stale after a resume; a fresh open finds the device again.
                    (*app).mouse = None;
                    (*app).battery = None;
                    (*app).refresh();
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
            apply_active_soft_mask(0);
            let app = app_from(hwnd);
            if !app.is_null() {
                (*app).unhook_fg();
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
                ui::WM_UI_CONFIG_CHANGED => {
                    // The settings window saved the config file (and wrote the mouse when wparam == 1).
                    match Config::load(&config_path()) {
                        Ok(c) => {
                            (*app).config = c;
                            (*app).config_error = None;
                        }
                        Err(e) => (*app).config_error = Some(e),
                    }
                    load_soft_macros(&(*app).config, (*app).game.applied.as_deref(), (*app).onboard_macros);
                    (*app).sync_poll_timer();
                    (*app).sync_fg_hooks();
                    if let Some(m) = (*app).mouse.as_mut() {
                        m.set_write_access(&(*app).config);
                    }
                    (*app).note_config_mtime();
                    (*app).config_applied = wparam == 1;
                    // The config (and the mouse) now hold the base layout; a running game is re-detected.
                    (*app).game.on_base_applied((*app).has_game_rules());
                    (*app).refresh();
                    (*app).check_game();
                    (*app).sync_retry_timer();
                    0
                }
                ui::WM_UI_CLOSED => 0,
                WM_DEVICECHANGE => {
                    let event = wparam as u32;
                    if event == DBT_DEVICEARRIVAL {
                        // The interface exists before the device answers: look again shortly.
                        SetTimer(hwnd, TIMER_REPLUG, 1500, None);
                    } else if event == DBT_DEVICEREMOVECOMPLETE {
                        if let Some(name) = broadcast_interface_name(lparam) {
                            if (*app).mouse.as_ref().is_some_and(|m| m.path().eq_ignore_ascii_case(&name)) {
                                (*app).mouse = None;
                                (*app).battery = None;
                                (*app).last_error = Some("mouse unplugged".into());
                                (*app).update_tray();
                                (*app).sync_retry_timer();
                            }
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
        let new_icon = make_battery_icon(self.battery, self.mouse.is_some());
        if new_icon != 0 {
            if self.hicon != 0 {
                DestroyIcon(self.hicon);
            }
            self.hicon = new_icon;
            self.nid.hIcon = self.hicon;
        }
    }

    unsafe fn update_tray(&mut self) {
        self.set_icon();
        let mut tip = format!("Freenapse-DeathAdder V4 Pro ({})", crate::VERSION);
        match (self.battery, self.mouse.as_ref()) {
            (Some(b), _) => {
                if b.wired {
                    tip.push_str("\n有线直连（无电池）");
                } else {
                    tip.push_str(&format!("\n电量 {}%{}", b.percent, if b.charging { "（充电中）" } else { "" }));
                }
            }
            (None, Some(_)) => tip.push_str("\n鼠标休眠中：移动鼠标唤醒"),
            (None, None) => tip.push_str("\n未检测到鼠标"),
        }
        if let Some(st) = &self.stages {
            if let Some(d) = st.stages.get(st.active.saturating_sub(1) as usize) {
                tip.push_str(&format!("\nDPI {}", d.0));
            }
        }
        if let Some(p) = self.polling {
            tip.push_str(&format!("\n{p} Hz"));
        }
        if let Some(g) = self.game.applied.as_deref().and_then(|k| self.config.game_by_key(k)) {
            tip.push_str(&format!("\n游戏配置：{}", g.name));
        }
        if self.config.tray.onboard_only {
            tip.push_str("\n纯板载反作弊模式");
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

    /// Polling rates the menu offers: the open mouse's, else the full HyperPolling list.
    fn rates(&self) -> &'static [u16] {
        self.mouse.as_ref().map(|m| m.spec().polling_rates).unwrap_or(&RATES_HYPERPOLLING)
    }

    fn has_game_rules(&self) -> bool {
        !self.config.games.is_empty() || !self.config.polling.games.is_empty()
    }

    /// Reconnect if needed, apply the config once, read battery/DPI/polling.
    unsafe fn refresh(&mut self) {
        // The config may have changed since the mouse was opened (the Devices page's switch).
        if let Some(m) = self.mouse.as_mut() {
            m.set_write_access(&self.config);
        }
        if self.mouse.is_none() {
            match Mouse::open() {
                Ok(m) => {
                    self.mouse = Some(m);
                    if config_path() != self.config_file {
                        self.switch_config();
                    }
                }
                Err(e) => {
                    self.last_error = Some(e.to_string());
                    self.battery = None;
                    self.update_tray();
                    return;
                }
            }
        }
        struct Read {
            applied: Option<usize>,
            onboard_macros: Option<bool>,
            battery: Battery,
            stages: Option<DpiStages>,
            polling: Option<u16>,
        }
        if self.need_import {
            let imported = apply::import(self.mouse.as_ref().unwrap());
            match imported {
                Ok(cfg) => {
                    if let Err(e) = Config::write_new(&config_path(), &cfg) {
                        self.config_error = Some(e);
                    }
                    self.onboard_macros = self.mouse.as_ref().and_then(|m| m.macro_support().ok()).unwrap_or(false);
                    load_soft_macros(&cfg, None, self.onboard_macros);
                    self.config = cfg;
                    self.need_import = false;
                    self.note_config_mtime();
                    self.config_applied = true;
                    self.game.on_base_applied(self.has_game_rules());
                    self.sync_poll_timer();
                    self.sync_fg_hooks();
                    self.notify("Freenapse-DeathAdder V4 Pro", "已从鼠标板载内存导入初始配置并保存", false);
                }
                Err(e) => {
                    self.last_error = Some(e.to_string());
                    self.update_tray();
                    return;
                }
            }
        }
        // A read-only mouse (unverified, writes not allowed) gets no apply: it would read
        // everything, be refused at the first write and be retried every few seconds,
        // keeping the mouse busy while the window waits to talk to it.
        if !self.mouse.as_ref().unwrap().writes_allowed() {
            self.config_applied = true;
        }
        let need_apply = !self.config_applied && self.config_error.is_none();
        let base_polling = self.config.polling.rate;
        let result = {
            let mouse = self.mouse.as_ref().unwrap();
            (|| -> Result<Read, crate::hid::HidError> {
                let applied = if need_apply { Some(apply::apply(mouse, &self.config, base_polling)?.len()) } else { None };
                let onboard_macros = mouse.macro_support().ok();
                Ok(Read { applied, onboard_macros, battery: mouse.battery()?, stages: mouse.dpi_stages().ok(), polling: mouse.polling_rate().ok() })
            })()
        };
        match result {
            Ok(r) => {
                if let Some(ob) = r.onboard_macros {
                    self.onboard_macros = ob;
                }
                if let Some(n) = r.applied {
                    self.config_applied = true;
                    // Base layout was just written: mark game overlay dirty so check_game re-applies any active game.
                    self.game.on_base_applied(self.has_game_rules());
                    update_active_soft_macros(&self.config, None, self.onboard_macros);
                    if n > 0 {
                        self.notify("Freenapse-DeathAdder V4 Pro", &format!("已将 {n} 项配置同步至鼠标板载内存"), false);
                    }
                } else {
                    update_active_soft_macros(&self.config, self.game.applied.as_deref(), self.onboard_macros);
                }
                self.battery = Some(r.battery);
                self.stages = r.stages;
                self.polling = r.polling;
                self.last_refresh = Some(Instant::now());
                self.last_error = None;
                let low = self.config.tray.notify_percent;
                let b = r.battery;
                if !b.wired && !b.charging && b.percent <= low && !self.notified_low {
                    self.notified_low = true;
                    let name = self.mouse.as_ref().map(|m| ui::display_model(&m.spec().name)).unwrap_or_else(|| "鼠标".into());
                    self.notify("鼠标电量过低", &format!("{name} 剩余电量 {}%，请及时充电。", b.percent), true);
                }
                if b.charging || b.percent > low + 5 {
                    self.notified_low = false;
                }
            }
            Err(e) => {
                let msg = e.to_string();
                match e {
                    // Permanent validation / unsupported errors: stop retrying until the config changes.
                    crate::hid::HidError::Invalid(_) | crate::hid::HidError::Unsupported(_) | crate::hid::HidError::Unverified(_) => {
                        self.config_error = Some(msg.clone());
                    }
                    // Device unplugged: drop the handle so the next refresh re-enumerates.
                    crate::hid::HidError::Win32(..) | crate::hid::HidError::NotFound => {
                        self.mouse = None;
                        self.battery = None;
                    }
                    // Asleep / transient timeout: keep the last known battery value and retry on wake.
                    _ => {}
                }
                self.last_error = Some(msg);
            }
        }
        self.update_tray();
    }

    /// Per-game profiles: when a configured game owns the foreground window,
    /// write its polling rate, DPI stage and button layout; restore the base
    /// config when it goes away.
    unsafe fn check_game(&mut self) {
        if !self.has_game_rules() && !self.game.dirty && self.game.applied.is_none() {
            update_active_soft_macros(&self.config, None, self.onboard_macros);
            return;
        }
        let path = match query_foreground() {
            FgQuery::TransientEmpty if self.game.fg_empty_retries < 3 => {
                self.game.fg_empty_retries += 1;
                SetTimer(self.hwnd, TIMER_GAME, 150, None);
                return;
            }
            FgQuery::TransientEmpty => {
                self.game.fg_empty_retries = 0;
                String::new()
            }
            FgQuery::Window(opt) => {
                self.game.fg_empty_retries = 0;
                opt.unwrap_or_default()
            }
        };
        let wanted = self.config.game_for(&path).map(|g| g.key);
        // Isolate & cancel disallowed software macros immediately, without waiting for HID writes.
        update_active_soft_macros(&self.config, wanted.as_deref(), self.onboard_macros);

        if !self.game.needs_sync(&wanted) {
            return;
        }
        self.game.begin_sync();
        if !user_active() && !self.retry_ticks.is_multiple_of(4) {
            return;
        }
        let Some(mouse) = self.mouse.as_ref() else {
            return;
        };
        if !mouse.writes_allowed() {
            self.game.commit_sync(wanted);
            return;
        }
        let over = wanted.as_deref().and_then(|g| self.config.game_by_key(g));
        let polling = over.as_ref().map(|g| g.polling).unwrap_or(self.config.polling.rate);
        let cfg = &self.config;
        let game = &mut self.game;
        let result: Result<(), crate::hid::HidError> = (|| {
            if mouse.polling_rate()? != polling {
                mouse.set_polling_rate(polling)?;
            }
            let st = mouse.dpi_stages()?;
            let stage = game.plan_dpi_stage(over.as_ref().and_then(|g| g.dpi_stage), over.is_some(), st.active);
            if let Some(stage) = stage.filter(|s| *s != st.active && (1..=st.stages.len() as u8).contains(s)) {
                mouse.set_dpi_stages(stage, &st.stages)?;
            }
            apply::apply_game_buttons(mouse, cfg, over.as_ref())?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.game.commit_sync(wanted);
                self.polling = mouse.polling_rate().ok();
                self.stages = mouse.dpi_stages().ok();
                self.last_error = None;
                self.update_tray();
            }
            Err(e) => {
                self.last_error = Some(e.to_string());
            }
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

    /// Arm the wake-up retry timer with bounded backoff (`1200..=4800 ms`) only while transient work is pending.
    unsafe fn sync_retry_timer(&mut self) {
        let need_base = self.need_import || (!self.config_applied && self.config_error.is_none()) || self.battery.is_none();
        let need = (need_base || self.game.dirty) && self.config_error.is_none();
        if need {
            let next_ms = if self.retry_ms == 0 {
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
        load_soft_macros(&self.config, None, self.onboard_macros);
        self.sync_poll_timer();
        self.sync_fg_hooks();
        self.config_applied = false;
        self.game = GameSync::default();
        self.note_config_mtime();
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
        load_soft_macros(&self.config, self.game.applied.as_deref(), self.onboard_macros);
        self.sync_poll_timer();
        self.sync_fg_hooks();
        self.config_applied = false;
        self.game.on_base_applied(self.has_game_rules());
        self.refresh();
        self.check_game();
        self.sync_retry_timer();
        if self.config_applied {
            self.notify("Freenapse-DeathAdder V4 Pro", "检测到配置文件修改：已自动写入鼠标", false);
        }
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
        let status = match (self.battery, self.mouse.as_ref()) {
            (Some(b), _) if b.wired => "有线供电（无内置电池）".to_string(),
            (Some(b), _) => format!("当前电量 {}%{}", b.percent, if b.charging { "（充电中）" } else { "" }),
            (None, Some(_)) => "鼠标已休眠（移动唤醒）".to_string(),
            (None, None) => "未检测到鼠标".to_string(),
        };
        AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, wide(&status).as_ptr());
        if let Some(e) = &self.last_error {
            if self.battery.is_none() {
                AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, wide(e).as_ptr());
            }
        }
        if let Some(v) = &self.update_tag {
            AppendMenuW(menu, MF_STRING, ID_UPDATE, wide(&format!("查看上游版本 {v}...")).as_ptr());
        }
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());

        let stages = self.stages.clone().map(|s| s.stages).unwrap_or_else(|| self.config.stage_pairs());
        let active = self.stages.as_ref().map(|s| s.active).unwrap_or(self.config.dpi.active);
        for (i, (x, _)) in stages.iter().enumerate() {
            let checked = if i + 1 == active as usize { MF_CHECKED } else { 0 };
            AppendMenuW(menu, MF_STRING | checked, ID_STAGE_BASE + i, wide(&format!("DPI 档位 {}：{x}", i + 1)).as_ptr());
        }
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        for (i, hz) in self.rates().iter().enumerate() {
            let checked = if Some(*hz) == self.polling { MF_CHECKED } else { 0 };
            AppendMenuW(menu, MF_STRING | checked, ID_POLL_BASE + i, wide(&format!("回报率 {hz} Hz")).as_ptr());
        }
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(menu, MF_STRING, ID_SETTINGS, wide("打开控制中心...").as_ptr());
        let auto_chk = if crate::autostart::is_enabled() { MF_CHECKED } else { 0 };
        AppendMenuW(menu, MF_STRING | auto_chk, ID_AUTOSTART, wide("开机静默自启").as_ptr());
        let ob_chk = if self.config.tray.onboard_only { MF_CHECKED } else { 0 };
        AppendMenuW(menu, MF_STRING | ob_chk, ID_ONBOARD_ONLY, wide("纯板载反作弊模式").as_ptr());
        AppendMenuW(menu, MF_STRING, ID_REFRESH, wide("立即刷新状态").as_ptr());
        AppendMenuW(menu, MF_STRING, ID_APPLY, wide("重新写入配置").as_ptr());
        AppendMenuW(menu, MF_STRING, ID_OPEN_CONFIG, wide("打开配置文件").as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(menu, MF_STRING, ID_QUIT, wide("退出托盘").as_ptr());

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
            0 => {}
            ID_QUIT => {
                DestroyWindow(self.hwnd);
            }
            ID_REFRESH => self.refresh(),
            ID_SETTINGS => {
                ui::open(self.hwnd);
            }
            ID_AUTOSTART => {
                let want = !crate::autostart::is_enabled();
                if let Err(e) = crate::autostart::set(want) {
                    self.notify("Freenapse-DeathAdder V4 Pro", &format!("设置开机自启失败：{e}"), true);
                }
            }
            ID_ONBOARD_ONLY => {
                self.config.tray.onboard_only = !self.config.tray.onboard_only;
                if let Err(e) = Config::write_new(&config_path(), &self.config) {
                    self.notify("Freenapse-DeathAdder V4 Pro", &format!("保存配置失败：{e}"), true);
                }
                self.note_config_mtime();
                update_active_soft_macros(&self.config, self.game.applied.as_deref(), self.onboard_macros);
                if !self.config.soft_macro_slots().is_empty() {
                    self.config_applied = false;
                    self.game.on_base_applied(self.has_game_rules());
                    self.refresh();
                    self.check_game();
                    self.sync_retry_timer();
                } else {
                    self.update_tray();
                }
            }
            ID_UPDATE => {
                ShellExecuteW(0, wide("open").as_ptr(), wide(crate::update::RELEASES_URL).as_ptr(), ptr::null(), ptr::null(), SW_SHOWNORMAL);
            }
            ID_APPLY => {
                match Config::load_or_create(&config_path()) {
                    Ok(c) => {
                        self.config = c;
                        self.config_error = None;
                    }
                    Err(e) => self.config_error = Some(e),
                }
                load_soft_macros(&self.config, self.game.applied.as_deref(), self.onboard_macros);
                self.sync_poll_timer();
                self.sync_fg_hooks();
                self.note_config_mtime();
                self.config_applied = false;
                self.game.on_base_applied(self.has_game_rules());
                self.refresh();
                self.check_game();
                self.sync_retry_timer();
            }
            ID_OPEN_CONFIG => {
                let path = config_path();
                let _ = Config::load_or_create(&path);
                ShellExecuteW(0, wide("open").as_ptr(), wide(&path.to_string_lossy()).as_ptr(), ptr::null(), ptr::null(), SW_SHOWNORMAL);
            }
            id if (ID_STAGE_BASE..ID_STAGE_BASE + 5).contains(&id) => {
                let idx = id - ID_STAGE_BASE;
                if let (Some(mouse), Some(st)) = (self.mouse.as_ref(), self.stages.as_ref()) {
                    if idx < st.stages.len() {
                        let r = mouse.set_dpi_stages((idx + 1) as u8, &st.stages);
                        if let Err(e) = r {
                            self.last_error = Some(e.to_string());
                        }
                        self.stages = mouse.dpi_stages().ok();
                        self.update_tray();
                    }
                }
            }
            id if (ID_POLL_BASE..ID_POLL_BASE + self.rates().len()).contains(&id) => {
                let hz = self.rates()[id - ID_POLL_BASE];
                if let Some(mouse) = self.mouse.as_ref() {
                    match mouse.set_polling_rate(hz) {
                        Ok(()) => {
                            self.polling = Some(hz);
                        }
                        Err(e) => self.last_error = Some(e.to_string()),
                    }
                    self.update_tray();
                }
            }
            _ => {}
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
        // 120 ms one-shot debounce: switches almost instantly while coalescing rapid Alt+Tab hops.
        SetTimer(tray, TIMER_GAME, 120, None);
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
    let Some(dir) = config_path().parent().map(|p| p.to_path_buf()) else { return };
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
// Software macros. For a mouse without macro memory (`Mouse::macro_support`)
// the macro is played from the PC, like Synapse does: the bound button sends F13..F24, a low-level
// keyboard hook swallows that key and plays the macro's events.
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::macros::{Event, Playback};
use crate::player;

struct SoftSlot {
    events: Arc<Vec<Event>>,
    play: Playback,
    /// Flag of the playback loop in progress ("while held" / toggle / times / sequence); set to stop it.
    stop: Mutex<Arc<AtomicBool>>,
    /// Serializes runs on this slot so a cancelled run releases held keys/buttons before a new one starts.
    run_lock: Mutex<()>,
    running: AtomicBool,
    down: AtomicBool,
    seq: AtomicUsize,
}

static SOFT: Mutex<Vec<Arc<SoftSlot>>> = Mutex::new(Vec::new());
static ACTIVE_SOFT_MASK: AtomicU16 = AtomicU16::new(0);
static HOOK_STATE: Mutex<Option<(u32, std::thread::JoinHandle<()>)>> = Mutex::new(None);

/// Rebuild the slot table from the config (slot i = key F13 + i, same order as `Config::soft_macro_slots`),
/// then activate only the slots permitted in the current foreground profile.
fn load_soft_macros(cfg: &Config, game_key: Option<&str>, onboard_macros: bool) {
    let slots: Vec<Arc<SoftSlot>> = cfg
        .soft_macro_slots()
        .into_iter()
        .map(|(id, play)| {
            let events = cfg.macro_by_id(id).and_then(|m| m.events().ok()).unwrap_or_default();
            Arc::new(SoftSlot {
                events: Arc::new(events),
                play,
                stop: Mutex::new(Arc::new(AtomicBool::new(false))),
                run_lock: Mutex::new(()),
                running: AtomicBool::new(false),
                down: AtomicBool::new(false),
                seq: AtomicUsize::new(0),
            })
        })
        .collect();
    {
        let mut table = SOFT.lock().unwrap_or_else(|e| e.into_inner());
        for s in table.iter() {
            s.running.store(false, Ordering::Relaxed);
            s.down.store(false, Ordering::Relaxed);
            s.stop.lock().unwrap_or_else(|e| e.into_inner()).store(true, Ordering::Relaxed);
        }
        *table = slots;
    }
    update_active_soft_macros(cfg, game_key, onboard_macros);
}

/// Recompute which software-macro slots are allowed in the current foreground profile
/// (`None` = desktop, `Some(key)` = active game), cancelling disallowed loops and toggling `WH_KEYBOARD_LL`.
fn update_active_soft_macros(cfg: &Config, game_key: Option<&str>, onboard_macros: bool) {
    let mask = cfg.active_soft_slot_mask(game_key, onboard_macros);
    apply_active_soft_mask(mask);
}

fn apply_active_soft_mask(mask: u16) {
    ACTIVE_SOFT_MASK.store(mask, Ordering::Relaxed);
    {
        let table = SOFT.lock().unwrap_or_else(|e| e.into_inner());
        for (i, s) in table.iter().enumerate() {
            if i >= 12 || (mask & (1u16 << i)) == 0 {
                s.running.store(false, Ordering::Relaxed);
                s.down.store(false, Ordering::Relaxed);
                s.stop.lock().unwrap_or_else(|e| e.into_inner()).store(true, Ordering::Relaxed);
            }
        }
    }
    sync_macro_hook(mask != 0);
}

/// Start the `WH_KEYBOARD_LL` hook thread on demand when `need` is true, and
/// cleanly `UnhookWindowsHookEx` + join the thread when no software macros are active.
fn sync_macro_hook(need: bool) {
    let mut guard = HOOK_STATE.lock().unwrap_or_else(|e| e.into_inner());
    if need {
        if guard.is_none() {
            let (tx, rx) = std::sync::mpsc::sync_channel::<Result<u32, ()>>(1);
            let handle = std::thread::spawn(move || unsafe {
                let tid = GetCurrentThreadId();
                let mut msg: MSG = mem::zeroed();
                // Ensure the thread message queue exists before publishing `tid` so WM_QUIT never races.
                PeekMessageW(&mut msg, 0, WM_USER, WM_USER, PM_NOREMOVE);
                let hhook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(kb_hook), GetModuleHandleW(ptr::null()), 0);
                if hhook == 0 {
                    let _ = tx.send(Err(()));
                    return;
                }
                if tx.send(Ok(tid)).is_err() {
                    UnhookWindowsHookEx(hhook);
                    return;
                }
                while GetMessageW(&mut msg, 0, 0, 0) > 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                UnhookWindowsHookEx(hhook);
            });
            if let Ok(Ok(tid)) = rx.recv() {
                *guard = Some((tid, handle));
            }
        }
    } else if let Some((tid, handle)) = guard.take() {
        let posted = unsafe { PostThreadMessageW(tid, WM_QUIT, 0, 0) != 0 };
        drop(guard);
        if posted {
            let _ = handle.join();
        }
    }
}

unsafe extern "system" fn kb_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let k = &*(lparam as *const KBDLLHOOKSTRUCT);
        let own = k.dwExtraInfo == player::INJECT_MARK;
        if !own && (player::VK_F13 as u32..=player::VK_F24 as u32).contains(&k.vkCode) {
            let slot = (k.vkCode - player::VK_F13 as u32) as usize;
            let down = wparam as u32 == WM_KEYDOWN || wparam as u32 == WM_SYSKEYDOWN;
            let active = slot < 12 && (ACTIVE_SOFT_MASK.load(Ordering::Relaxed) & (1u16 << slot)) != 0;
            let s = SOFT.lock().ok().and_then(|t| t.get(slot).cloned());
            if let Some(s) = s {
                if active || (!down && s.down.load(Ordering::Relaxed)) {
                    soft_trigger(&s, down);
                    return 1;
                }
            }
        }
    }
    CallNextHookEx(0, code, wparam, lparam)
}

/// Start a fresh stop flag for a playback run, ending the previous one.
fn new_run(s: &SoftSlot) -> Arc<AtomicBool> {
    let mut g = s.stop.lock().unwrap_or_else(|e| e.into_inner());
    g.store(true, Ordering::Relaxed);
    *g = Arc::new(AtomicBool::new(false));
    g.clone()
}

fn soft_trigger(s: &Arc<SoftSlot>, down: bool) {
    let was_down = s.down.swap(down, Ordering::Relaxed);
    if down && was_down {
        return; // keyboard auto-repeat of the held key
    }
    if down && s.events.is_empty() {
        return;
    }
    match s.play {
        Playback::Times(n) => {
            if down {
                let s = s.clone();
                let stop = new_run(&s);
                std::thread::spawn(move || {
                    let _run = s.run_lock.lock().unwrap_or_else(|e| e.into_inner());
                    for _ in 0..n.max(1) {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        player::play(&s.events, &stop);
                    }
                });
            }
        }
        Playback::Held => {
            if down {
                let s = s.clone();
                let stop = new_run(&s);
                std::thread::spawn(move || {
                    let _run = s.run_lock.lock().unwrap_or_else(|e| e.into_inner());
                    if player::has_launch(&s.events) {
                        player::play(&s.events, &stop);
                        return;
                    }
                    let need_yield = player::loop_needs_yield(&s.events);
                    while !stop.load(Ordering::Relaxed) {
                        player::play(&s.events, &stop);
                        if need_yield && !stop.load(Ordering::Relaxed) {
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                    }
                });
            } else {
                s.stop.lock().unwrap_or_else(|e| e.into_inner()).store(true, Ordering::Relaxed);
            }
        }
        Playback::Toggle => {
            if !down {
                return;
            }
            if s.running.swap(false, Ordering::Relaxed) {
                s.stop.lock().unwrap_or_else(|e| e.into_inner()).store(true, Ordering::Relaxed);
            } else {
                s.running.store(true, Ordering::Relaxed);
                let s = s.clone();
                let stop = new_run(&s);
                std::thread::spawn(move || {
                    let _run = s.run_lock.lock().unwrap_or_else(|e| e.into_inner());
                    if player::has_launch(&s.events) {
                        player::play(&s.events, &stop);
                        s.running.store(false, Ordering::Relaxed);
                        return;
                    }
                    let need_yield = player::loop_needs_yield(&s.events);
                    while !stop.load(Ordering::Relaxed) {
                        player::play(&s.events, &stop);
                        if need_yield && !stop.load(Ordering::Relaxed) {
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                    }
                    s.running.store(false, Ordering::Relaxed);
                });
            }
        }
        Playback::Sequence => {
            if down {
                let steps = player::sequence_steps(&s.events);
                if steps.is_empty() {
                    return;
                }
                let k = s.seq.fetch_add(1, Ordering::Relaxed) % steps.len();
                let step = steps[k].clone();
                let s = s.clone();
                let stop = new_run(&s);
                std::thread::spawn(move || {
                    let _run = s.run_lock.lock().unwrap_or_else(|e| e.into_inner());
                    player::play(&step, &stop);
                });
            }
        }
    }
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
    use super::GameSync;

    #[test]
    fn base_reapply_marks_game_dirty_until_foreground_resynced() {
        let mut gs = GameSync::default();
        // Mouse wakes up after initial retry and applies base layout while a game is already in foreground.
        gs.on_base_applied(true);
        assert!(gs.dirty, "base apply must keep game sync dirty when game rules exist");
        assert!(gs.needs_sync(&Some("c:\\games\\shooter".into())));
        // Even if foreground turns out to be desktop, one check_game pass confirms and clears dirty.
        assert!(gs.needs_sync(&None));
        gs.begin_sync();
        gs.commit_sync(None);
        assert!(!gs.dirty);
        assert!(!gs.needs_sync(&None));
    }

    #[test]
    fn partial_game_failure_still_restores_base_and_dpi_on_exit() {
        let mut gs = GameSync::default();
        gs.commit_sync(None);

        // Entering a game: polling & DPI stage succeeded (active stage 2 -> 1), then button write failed.
        let wanted_game = Some("c:\\games\\shooter".to_string());
        assert!(gs.needs_sync(&wanted_game));
        gs.begin_sync();
        assert_eq!(gs.plan_dpi_stage(Some(1), true, 2), Some(1));
        assert_eq!(gs.stage_before_game, Some(2));
        // Notice: commit_sync is NOT called because apply_game_buttons returned Err.
        assert_eq!(gs.applied, None);
        assert!(gs.dirty);

        // User Alt+Tabs back to desktop before retry succeeds: must STILL sync and restore stage 2!
        assert!(gs.needs_sync(&None));
        gs.begin_sync();
        assert_eq!(gs.plan_dpi_stage(None, false, 1), Some(2));
        gs.commit_sync(None);
        assert_eq!(gs.stage_before_game, None);
        assert!(!gs.dirty);
    }

    #[test]
    fn stage_before_game_survives_across_multiple_games() {
        let mut gs = GameSync::default();
        gs.commit_sync(None);

        // Desktop at stage 2 -> Game A (no DPI override): leave stage untouched, do not clobber stage_before_game.
        let game_a = Some("game:a".to_string());
        gs.begin_sync();
        assert_eq!(gs.plan_dpi_stage(None, true, 2), None);
        assert_eq!(gs.stage_before_game, None);
        gs.commit_sync(game_a);

        // Game A -> Game B (DPI override stage 4): remember original stage 2 and switch to 4.
        let game_b = Some("game:b".to_string());
        gs.begin_sync();
        assert_eq!(gs.plan_dpi_stage(Some(4), true, 2), Some(4));
        assert_eq!(gs.stage_before_game, Some(2));
        gs.commit_sync(game_b);

        // Game B -> Game C (DPI override stage 1): keep original desktop stage 2!
        let game_c = Some("game:c".to_string());
        gs.begin_sync();
        assert_eq!(gs.plan_dpi_stage(Some(1), true, 4), Some(1));
        assert_eq!(gs.stage_before_game, Some(2));
        gs.commit_sync(game_c);

        // Game C -> Desktop: restore stage 2 and clear saved stage.
        gs.begin_sync();
        assert_eq!(gs.plan_dpi_stage(None, false, 1), Some(2));
        gs.commit_sync(None);
        assert_eq!(gs.stage_before_game, None);
    }
}
