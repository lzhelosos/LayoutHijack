use chrono::Local;
use std::fs::{create_dir_all, OpenOptions};
use std::io::Write;
use std::mem::{size_of, zeroed};
use std::path::PathBuf;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use winapi::ctypes::c_void;
use winapi::shared::minwindef::{DWORD, HKL, LPARAM, LRESULT, WPARAM};
use winapi::shared::windef::{HHOOK, HICON, HWND};
use winapi::um::libloaderapi::GetModuleHandleW;
use winapi::um::shellapi::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use winapi::um::wincon::{GetConsoleProcessList, GetConsoleWindow};
use winapi::um::winnt::{KEY_WRITE, REG_SZ};
use winapi::um::winreg::{RegCloseKey, RegOpenKeyExW, RegSetValueExW, HKEY_CURRENT_USER};
use winapi::um::winuser::*;

#[link(name = "imm32")]
extern "system" {
    fn ImmGetDefaultIMEWnd(hWnd: HWND) -> HWND;
}

#[link(name = "kernel32")]
extern "system" {
    fn GetStdHandle(nStdHandle: DWORD) -> *mut c_void;
    fn GetConsoleMode(hConsoleHandle: *mut c_void, lpMode: *mut DWORD) -> i32;
    fn SetConsoleMode(hConsoleHandle: *mut c_void, dwMode: DWORD) -> i32;
}

static WORKER_TX: Mutex<Option<Sender<WorkerEvent>>> = Mutex::new(None);
static mut HOOK_HANDLE: HHOOK = null_mut();
static TRAY_WINDOW: AtomicUsize = AtomicUsize::new(0);
static CONSOLE_HIDDEN: AtomicBool = AtomicBool::new(true);
static COOLDOWN_ENABLED: AtomicBool = AtomicBool::new(true);

const TRAY_ICON_ID: u32 = 1;
const WM_TRAY: u32 = WM_APP + 1;
const TIMER_LAYOUT: usize = 1;
const MENU_CONSOLE: usize = 2001;
const MENU_EXIT: usize = 2002;
const MENU_COOLDOWN: usize = 2003;
const ICON_EN: u16 = 101;
const ICON_RU: u16 = 102;

const DEBOUNCE_COOLDOWN_MS: u128 = 150;

// Return the embedded icon selected for the foreground window's active layout.
unsafe fn foreground_icon_id() -> u16 {
    let foreground = GetForegroundWindow();
    if foreground.is_null() {
        return ICON_EN;
    }
    let class = get_window_class(foreground);
    let is_console = class == "ConsoleWindowClass";
    let thread_id = GetWindowThreadProcessId(foreground, null_mut());
    let mut info: GUITHREADINFO = zeroed();
    info.cbSize = size_of::<GUITHREADINFO>() as DWORD;
    let target = if !is_console
        && GetGUIThreadInfo(thread_id, &mut info) != 0
        && !info.hwndFocus.is_null()
    {
        info.hwndFocus
    } else {
        foreground
    };
    let (hkl, _) = query_window_hkl(foreground, target, is_console);
    if (hkl as usize & 0xFFFF) == 0x0419 {
        ICON_RU
    } else {
        ICON_EN
    }
}

unsafe fn load_embedded_icon(id: u16) -> HICON {
    LoadImageW(
        GetModuleHandleW(null_mut()),
        id as usize as *const u16,
        IMAGE_ICON,
        0,
        0,
        LR_DEFAULTSIZE | LR_SHARED,
    ) as HICON
}

unsafe fn notification_data(hwnd: HWND, icon: HICON) -> NOTIFYICONDATAW {
    let mut data: NOTIFYICONDATAW = zeroed();
    data.cbSize = size_of::<NOTIFYICONDATAW>() as DWORD;
    data.hWnd = hwnd;
    data.uID = TRAY_ICON_ID;
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    data.uCallbackMessage = WM_TRAY;
    data.hIcon = icon;
    let tooltip = to_wide("LayoutHijack - переключение раскладки");
    let n = tooltip.len().min(data.szTip.len());
    data.szTip[..n].copy_from_slice(&tooltip[..n]);
    data
}

unsafe fn add_tray_icon(hwnd: HWND) -> bool {
    let mut data = notification_data(hwnd, load_embedded_icon(foreground_icon_id()));
    Shell_NotifyIconW(NIM_ADD, &mut data) != 0
}

unsafe fn update_tray_icon(hwnd: HWND) {
    let new_id = foreground_icon_id();
    // The current icon ID is stored in the hidden window's user data.
    if GetWindowLongPtrW(hwnd, GWLP_USERDATA) as u16 != new_id {
        let mut data = notification_data(hwnd, load_embedded_icon(new_id));
        if Shell_NotifyIconW(NIM_MODIFY, &mut data) != 0 {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, new_id as isize);
        }
    }
}

unsafe fn set_console_hidden(hidden: bool) {
    let console = GetConsoleWindow();
    if !console.is_null() {
        // Never hide a PowerShell/cmd window shared with the calling shell.
        let mut process_ids = [0u32; 2];
        if hidden && GetConsoleProcessList(process_ids.as_mut_ptr(), 2) > 1 {
            CONSOLE_HIDDEN.store(false, Ordering::Relaxed);
            return;
        }
        ShowWindow(console, if hidden { SW_HIDE } else { SW_SHOW });
        CONSOLE_HIDDEN.store(hidden, Ordering::Relaxed);
    }
}

unsafe fn show_tray_menu(hwnd: HWND) {
    let menu = CreatePopupMenu();
    if menu.is_null() {
        return;
    }
    let console_label = to_wide("Скрывать консоль в трей");
    let cooldown_label = to_wide("Кулдаун 150 мс (антидребезг)");
    let exit_label = to_wide("Выход");

    let console_checked = if CONSOLE_HIDDEN.load(Ordering::Relaxed) {
        MF_CHECKED
    } else {
        MF_UNCHECKED
    };
    let cooldown_checked = if COOLDOWN_ENABLED.load(Ordering::Relaxed) {
        MF_CHECKED
    } else {
        MF_UNCHECKED
    };

    AppendMenuW(
        menu,
        MF_STRING | console_checked,
        MENU_CONSOLE,
        console_label.as_ptr(),
    );
    AppendMenuW(
        menu,
        MF_STRING | cooldown_checked,
        MENU_COOLDOWN,
        cooldown_label.as_ptr(),
    );
    AppendMenuW(menu, MF_SEPARATOR, 0, null_mut());
    AppendMenuW(menu, MF_STRING, MENU_EXIT, exit_label.as_ptr());

    let mut point: winapi::shared::windef::POINT = zeroed();
    GetCursorPos(&mut point);
    SetForegroundWindow(hwnd);
    TrackPopupMenu(
        menu,
        TPM_RIGHTBUTTON,
        point.x,
        point.y,
        0,
        hwnd,
        null_mut(),
    );
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(menu);
}

unsafe extern "system" fn tray_window_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    if msg == RegisterWindowMessageW(to_wide("TaskbarCreated").as_ptr()) {
        if add_tray_icon(hwnd) {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, foreground_icon_id() as isize);
        }
        return 0;
    }
    match msg {
        WM_TRAY => {
            if lp as u32 == WM_RBUTTONUP || lp as u32 == WM_CONTEXTMENU {
                show_tray_menu(hwnd);
            }
            0
        }
        WM_COMMAND => {
            match wp & 0xFFFF {
                MENU_CONSOLE => set_console_hidden(!CONSOLE_HIDDEN.load(Ordering::Relaxed)),
                MENU_COOLDOWN => {
                    let new_state = !COOLDOWN_ENABLED.load(Ordering::Relaxed);
                    COOLDOWN_ENABLED.store(new_state, Ordering::Relaxed);
                    write_daily_log(&format!(
                        "[CONFIG] Кулдаун {} мс (антидребезг): {}",
                        DEBOUNCE_COOLDOWN_MS,
                        if new_state { "ВКЛЮЧЕН" } else { "ВЫКЛЮЧЕН" }
                    ));
                }
                MENU_EXIT => {
                    DestroyWindow(hwnd);
                }
                _ => {}
            }
            0
        }
        WM_TIMER if wp == TIMER_LAYOUT => {
            update_tray_icon(hwnd);
            0
        }
        WM_DESTROY => {
            KillTimer(hwnd, TIMER_LAYOUT);
            let mut data = notification_data(hwnd, null_mut());
            Shell_NotifyIconW(NIM_DELETE, &mut data);
            TRAY_WINDOW.store(0, Ordering::Relaxed);
            write_daily_log(
                "Штатное завершение работы LayoutHijack. Системный Alt+Shift остаётся активным.",
            );
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe fn create_tray_window() -> HWND {
    let h_instance = GetModuleHandleW(null_mut());
    let class_name = to_wide("LayoutHijackTrayWindow");
    let mut wc: WNDCLASSW = zeroed();
    wc.lpfnWndProc = Some(tray_window_proc);
    wc.hInstance = h_instance;
    wc.lpszClassName = class_name.as_ptr();
    if RegisterClassW(&wc) == 0 {
        return null_mut();
    }
    let hwnd = CreateWindowExW(
        0,
        class_name.as_ptr(),
        to_wide("LayoutHijack").as_ptr(),
        0,
        0,
        0,
        0,
        0,
        null_mut(),
        null_mut(),
        h_instance,
        null_mut(),
    );
    if !hwnd.is_null() {
        TRAY_WINDOW.store(hwnd as usize, Ordering::Relaxed);
        if add_tray_icon(hwnd) {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, foreground_icon_id() as isize);
        } else {
            write_daily_log("WARN: Не удалось добавить иконку в системный трей.");
        }
        SetTimer(hwnd, TIMER_LAYOUT, 250, None);
    }
    hwnd
}

#[derive(Debug)]
enum WorkerEvent {
    SwitchTriggered {
        chord_name: &'static str,
        hold_ms: u128,
    },
    SwitchDebounced {
        chord_name: &'static str,
        hold_ms: u128,
        since_last_ms: u128,
    },
    ChordAborted {
        vk_code: u32,
    },
}

struct KeyStateMachine {
    alt_down: bool,
    shift_down: bool,
    chord_armed: bool,
    other_key_pressed: bool,
    chord_start: Option<Instant>,
    last_switch_time: Option<Instant>,
    chord_name: &'static str,
}

static STATE: Mutex<KeyStateMachine> = Mutex::new(KeyStateMachine {
    alt_down: false,
    shift_down: false,
    chord_armed: false,
    other_key_pressed: false,
    chord_start: None,
    last_switch_time: None,
    chord_name: "",
});

struct SwitchTracker {
    last_hwnd: usize,
    last_raw_hkl: usize,
    last_sent_hkl: usize,
}

fn main() {
    disable_own_console_quick_edit();
    unsafe {
        set_console_hidden(true);
    }

    let _ = create_dir_all(log_dir());
    write_daily_log("============================================================");
    write_daily_log("Запуск LayoutHijack v0.4 (Трей + Перехват 0xFF + Кулдаун 150 мс)");

    // 1. Восстанавливаем штатный переключатель Windows в реестре (чтобы он всегда работал после закрытия программы)
    if restore_windows_default_toggle() {
        write_daily_log(
            "Штатный переключатель Windows восстановлен (Hotkey=1, Language Hotkey=1, Layout Hotkey=2).",
        );
    } else {
        write_daily_log("WARN: Не удалось обновить ключи HKCU\\Keyboard Layout\\Toggle.");
    }

    let _ = ctrlc::set_handler(move || {
        let hwnd = TRAY_WINDOW.load(Ordering::Relaxed) as HWND;
        if !hwnd.is_null() {
            unsafe {
                PostMessageW(hwnd, WM_CLOSE, 0, 0);
            }
        } else {
            std::process::exit(0);
        }
    });

    // 2. Запускаем рабочий поток переключения и логирования
    let (tx, rx) = mpsc::channel::<WorkerEvent>();
    *WORKER_TX.lock().unwrap() = Some(tx);

    thread::spawn(move || {
        let mut tracker = SwitchTracker {
            last_hwnd: 0,
            last_raw_hkl: 0,
            last_sent_hkl: 0,
        };

        for event in rx {
            match event {
                WorkerEvent::SwitchTriggered {
                    chord_name,
                    hold_ms,
                } => {
                    perform_reliable_switch(chord_name, hold_ms, &mut tracker);
                    let tray_hwnd = TRAY_WINDOW.load(Ordering::Relaxed) as HWND;
                    if !tray_hwnd.is_null() {
                        unsafe {
                            PostMessageW(tray_hwnd, WM_TIMER, TIMER_LAYOUT, 0);
                        }
                    }
                }
                WorkerEvent::SwitchDebounced {
                    chord_name,
                    hold_ms,
                    since_last_ms,
                } => {
                    write_daily_log(&format!(
                        "[DEBOUNCE] Пропущен повторный {} (удержание {} мс, прошло {} мс < {} мс)",
                        chord_name, hold_ms, since_last_ms, DEBOUNCE_COOLDOWN_MS
                    ));
                }
                WorkerEvent::ChordAborted { vk_code } => {
                    write_daily_log(&format!(
                        "[ABORT] Комбинация отменена: нажата посторонняя клавиша VK=0x{:02X}",
                        vk_code
                    ));
                }
            }
        }
    });

    // 3. Добавляем скрытое окно для трея. На механизм переключения оно не влияет.
    unsafe {
        if create_tray_window().is_null() {
            write_daily_log("FATAL: Не удалось создать окно системного трея!");
            return;
        }
    }

    // 4. Устанавливаем глобальный низкоуровневый хук клавиатуры
    unsafe {
        let h_instance = GetModuleHandleW(null_mut());
        HOOK_HANDLE =
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(low_level_keyboard_proc), h_instance, 0);

        if HOOK_HANDLE.is_null() {
            write_daily_log("FATAL: Не удалось установить WH_KEYBOARD_LL хук!");
            return;
        }
        write_daily_log(
            "Хук WH_KEYBOARD_LL успешно установлен. Нативный Alt+Shift гасится на лету через инъекцию 0xFF.",
        );

        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        UnhookWindowsHookEx(HOOK_HANDLE);
    }
}

fn disable_own_console_quick_edit() {
    unsafe {
        const STD_INPUT_HANDLE: DWORD = (-10i32) as DWORD;
        const ENABLE_QUICK_EDIT_MODE: DWORD = 0x0040;
        const ENABLE_EXTENDED_FLAGS: DWORD = 0x0080;

        let h_in = GetStdHandle(STD_INPUT_HANDLE);
        if !h_in.is_null() {
            let mut mode: DWORD = 0;
            if GetConsoleMode(h_in, &mut mode) != 0 {
                mode &= !ENABLE_QUICK_EDIT_MODE;
                mode |= ENABLE_EXTENDED_FLAGS;
                SetConsoleMode(h_in, mode);
            }
        }
    }
}

// ============================================================================
// НИЗКОУРОВНЕВЫЙ ХУК КЛАВИАТУРЫ (WH_KEYBOARD_LL)
// ============================================================================
unsafe extern "system" fn low_level_keyboard_proc(
    n_code: i32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    if n_code == HC_ACTION {
        let kb = &*(l_param as *const KBDLLHOOKSTRUCT);

        // Игнорируем инъекции (в том числе наш собственный 0xFF)
        if (kb.flags & LLKHF_INJECTED) == 0 {
            let vk = kb.vkCode as i32;
            let is_down = w_param == WM_KEYDOWN as usize || w_param == WM_SYSKEYDOWN as usize;
            let is_up = w_param == WM_KEYUP as usize || w_param == WM_SYSKEYUP as usize;

            let is_alt = vk == VK_LMENU || vk == VK_RMENU || vk == VK_MENU;
            let is_shift = vk == VK_LSHIFT || vk == VK_RSHIFT || vk == VK_SHIFT;

            if let Ok(mut st) = STATE.lock() {
                if is_down {
                    if is_alt {
                        if !st.alt_down {
                            st.alt_down = true;
                            if st.shift_down && !st.other_key_pressed {
                                st.chord_armed = true;
                                st.chord_start = Some(Instant::now());
                                st.chord_name = "Shift+Alt";
                            }
                        }
                    } else if is_shift {
                        if !st.shift_down {
                            st.shift_down = true;
                            if st.alt_down && !st.other_key_pressed {
                                st.chord_armed = true;
                                st.chord_start = Some(Instant::now());
                                st.chord_name = "Alt+Shift";
                            }
                        }
                    } else {
                        if st.chord_armed {
                            st.chord_armed = false;
                            send_worker_event(WorkerEvent::ChordAborted {
                                vk_code: kb.vkCode,
                            });
                        }
                        if st.alt_down || st.shift_down {
                            st.other_key_pressed = true;
                        }
                    }
                } else if is_up {
                    if is_alt || is_shift {
                        if st.chord_armed && !st.other_key_pressed {
                            st.chord_armed = false;
                            let hold_ms = st
                                .chord_start
                                .map(|t| t.elapsed().as_millis())
                                .unwrap_or(0);
                            let name = st.chord_name;

                            // Всегда гасим нативное переключение Windows и меню через 0xFF до CallNextHookEx,
                            // даже если сработал антидребезг (иначе Windows переключит язык сама на дребезге).
                            suppress_native_switch_and_menu();

                            let since_last = st.last_switch_time.map(|t| t.elapsed().as_millis());
                            let is_debounced = COOLDOWN_ENABLED.load(Ordering::Relaxed)
                                && since_last.map_or(false, |ms| ms < DEBOUNCE_COOLDOWN_MS);

                            if is_debounced {
                                send_worker_event(WorkerEvent::SwitchDebounced {
                                    chord_name: name,
                                    hold_ms,
                                    since_last_ms: since_last.unwrap_or(0),
                                });
                            } else {
                                st.last_switch_time = Some(Instant::now());
                                send_worker_event(WorkerEvent::SwitchTriggered {
                                    chord_name: name,
                                    hold_ms,
                                });
                            }
                        }

                        if is_alt {
                            st.alt_down = false;
                        }
                        if is_shift {
                            st.shift_down = false;
                        }

                        if !st.alt_down && !st.shift_down {
                            st.other_key_pressed = false;
                            st.chord_armed = false;
                        }
                    }
                }
            }
        }
    }

    CallNextHookEx(HOOK_HANDLE, n_code, w_param, l_param)
}

unsafe fn suppress_native_switch_and_menu() {
    keybd_event(0xFF, 0, 0, 0);
    keybd_event(0xFF, 0, KEYEVENTF_KEYUP, 0);
}

fn send_worker_event(ev: WorkerEvent) {
    if let Ok(guard) = WORKER_TX.lock() {
        if let Some(ref tx) = *guard {
            let _ = tx.send(ev);
        }
    }
}

// ============================================================================
// ОПРЕДЕЛЕНИЕ РЕАЛЬНОЙ РАСКЛАДКИ (С ОБХОДОМ БАГА CONSOLEWINDOWCLASS)
// ============================================================================
unsafe fn query_window_hkl(
    fg_hwnd: HWND,
    target_hwnd: HWND,
    is_console: bool,
) -> (HKL, &'static str) {
    if is_console {
        let ime_hwnd = ImmGetDefaultIMEWnd(fg_hwnd);
        if !ime_hwnd.is_null() {
            let ime_tid = GetWindowThreadProcessId(ime_hwnd, null_mut());
            if ime_tid != 0 {
                let ime_hkl = GetKeyboardLayout(ime_tid);
                if !ime_hkl.is_null() {
                    return (ime_hkl, "IME-Conhost");
                }
            }
        }
    }

    let target_tid = GetWindowThreadProcessId(target_hwnd, null_mut());
    let raw_hkl = GetKeyboardLayout(target_tid);
    if !raw_hkl.is_null() {
        return (raw_hkl, "Thread");
    }

    let ime_hwnd = ImmGetDefaultIMEWnd(fg_hwnd);
    if !ime_hwnd.is_null() {
        let ime_tid = GetWindowThreadProcessId(ime_hwnd, null_mut());
        if ime_tid != 0 {
            let ime_hkl = GetKeyboardLayout(ime_tid);
            if !ime_hkl.is_null() {
                return (ime_hkl, "IME-Fallback");
            }
        }
    }

    (null_mut(), "Null")
}

// ============================================================================
// БРОНЕБОЙНОЕ ПЕРЕКЛЮЧЕНИЕ РАСКЛАДКИ
// ============================================================================
fn perform_reliable_switch(chord_name: &str, hold_ms: u128, tracker: &mut SwitchTracker) {
    unsafe {
        let fg_hwnd = GetForegroundWindow();
        if fg_hwnd.is_null() {
            write_daily_log(&format!(
                "[ERROR] Сработал {} ({} мс), но GetForegroundWindow() вернул NULL!",
                chord_name, hold_ms
            ));
            return;
        }

        let fg_class = get_window_class(fg_hwnd);
        let win_title = get_window_title(fg_hwnd);
        let is_console = fg_class == "ConsoleWindowClass";

        let fg_thread_id = GetWindowThreadProcessId(fg_hwnd, null_mut());
        let mut gui_info: GUITHREADINFO = zeroed();
        gui_info.cbSize = size_of::<GUITHREADINFO>() as DWORD;

        let target_hwnd = if !is_console
            && GetGUIThreadInfo(fg_thread_id, &mut gui_info) != 0
            && !gui_info.hwndFocus.is_null()
        {
            gui_info.hwndFocus
        } else {
            fg_hwnd
        };

        let target_class = get_window_class(target_hwnd);

        let count = GetKeyboardLayoutList(0, null_mut());
        if count <= 1 {
            write_daily_log(&format!(
                "[WARN] В системе найдена всего {} раскладка.",
                count
            ));
            return;
        }

        let mut layouts: Vec<HKL> = vec![null_mut(); count as usize];
        GetKeyboardLayoutList(count, layouts.as_mut_ptr());

        let (detected_hkl, detect_source) = query_window_hkl(fg_hwnd, target_hwnd, is_console);
        let detected_val = detected_hkl as usize;

        let effective_current_hkl: HKL = if detected_val == 0 {
            if tracker.last_sent_hkl != 0 {
                tracker.last_sent_hkl as HKL
            } else {
                layouts[0]
            }
        } else if (fg_hwnd as usize) == tracker.last_hwnd
            && detected_val == tracker.last_raw_hkl
            && tracker.last_sent_hkl != 0
            && tracker.last_sent_hkl != detected_val
        {
            tracker.last_sent_hkl as HKL
        } else {
            detected_hkl
        };

        let cur_index = layouts
            .iter()
            .position(|&h| h == effective_current_hkl)
            .unwrap_or(0);
        let next_hkl = layouts[(cur_index + 1) % layouts.len()];

        tracker.last_hwnd = fg_hwnd as usize;
        tracker.last_raw_hkl = detected_val;
        tracker.last_sent_hkl = next_hkl as usize;

        PostMessageW(
            target_hwnd,
            WM_INPUTLANGCHANGEREQUEST,
            0,
            next_hkl as LPARAM,
        );

        thread::sleep(Duration::from_millis(35));
        let (verify_hkl_1, _) = query_window_hkl(fg_hwnd, target_hwnd, is_console);

        if verify_hkl_1 == next_hkl {
            write_daily_log(&format!(
                "[OK] {} ({} мс) | 0x{:08X} -> 0x{:08X} [src:{}] | HWND={:?} | Class='{}' | Title='{}'",
                chord_name,
                hold_ms,
                effective_current_hkl as usize,
                next_hkl as usize,
                detect_source,
                target_hwnd,
                target_class,
                win_title
            ));
            return;
        }

        if is_console {
            write_daily_log(&format!(
                "[OK-CONSOLE] {} ({} мс) | 0x{:08X} -> 0x{:08X} (raw=0x{:08X}, src:{}) | HWND={:?} | Title='{}'",
                chord_name,
                hold_ms,
                effective_current_hkl as usize,
                next_hkl as usize,
                detected_val,
                detect_source,
                fg_hwnd,
                win_title
            ));
            return;
        }

        const INPUTLANGCHANGE_SYSCHARSET: WPARAM = 0x0001;
        PostMessageW(
            fg_hwnd,
            WM_INPUTLANGCHANGEREQUEST,
            INPUTLANGCHANGE_SYSCHARSET,
            next_hkl as LPARAM,
        );

        thread::sleep(Duration::from_millis(40));
        let (verify_hkl_2, _) = query_window_hkl(fg_hwnd, fg_hwnd, is_console);

        if verify_hkl_2 == next_hkl {
            write_daily_log(&format!(
                "[OK-FALLBACK] {} ({} мс) | 0x{:08X} -> 0x{:08X} | fg_hwnd={:?} | Class='{}' | Title='{}'",
                chord_name,
                hold_ms,
                effective_current_hkl as usize,
                next_hkl as usize,
                fg_hwnd,
                fg_class,
                win_title
            ));
        } else {
            write_daily_log(&format!(
                "[FAIL] Отказ менять раскладку! Осталась 0x{:08X} (ожидалась 0x{:08X}) | fg_hwnd={:?} | Class='{}' | Title='{}'",
                verify_hkl_2 as usize,
                next_hkl as usize,
                fg_hwnd,
                fg_class,
                win_title
            ));
        }
    }
}

// ============================================================================
// ВОССТАНОВЛЕНИЕ ШТАТНЫХ НАСТРОЕК WINDOWS В РЕЕСТРЕ
// ============================================================================
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Восстанавливает стандартную конфигурацию Windows:
/// Language Hotkey = "1" (Alt+Shift), Layout Hotkey = "2" (Ctrl+Shift, без конфликта)
fn restore_windows_default_toggle() -> bool {
    unsafe {
        let mut hkey = null_mut();
        let subkey = to_wide("Keyboard Layout\\Toggle");
        if RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_WRITE, &mut hkey) != 0 {
            return false;
        }

        let pairs = [
            ("Hotkey", "1"),
            ("Language Hotkey", "1"),
            ("Layout Hotkey", "2"),
        ];

        for (name, val) in &pairs {
            let w_name = to_wide(name);
            let w_val = to_wide(val);
            let byte_len = (w_val.len() * 2) as DWORD;
            RegSetValueExW(
                hkey,
                w_name.as_ptr(),
                0,
                REG_SZ,
                w_val.as_ptr() as *const u8,
                byte_len,
            );
        }

        RegCloseKey(hkey);
        SystemParametersInfoW(SPI_SETLANGTOGGLE, 0, null_mut(), 0) != 0
    }
}

// ============================================================================
// УТИЛИТЫ ОКОН И ЕЖЕДНЕВНОГО ЛОГИРОВАНИЯ
// ============================================================================
unsafe fn get_window_title(hwnd: HWND) -> String {
    let mut buf: [u16; 256] = [0; 256];
    let len = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
    if len > 0 {
        String::from_utf16_lossy(&buf[..len as usize])
    } else {
        String::from("<Без заголовка>")
    }
}

unsafe fn get_window_class(hwnd: HWND) -> String {
    let mut buf: [u16; 128] = [0; 128];
    let len = GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
    if len > 0 {
        String::from_utf16_lossy(&buf[..len as usize])
    } else {
        String::from("<Неизвестный класс>")
    }
}

fn write_daily_log(message: &str) {
    let now = Local::now();
    let date_str = now.format("%Y-%m-%d").to_string();
    let time_str = now.format("%H:%M:%S%.3f").to_string();

    let log_path = log_dir().join(format!("{}.log", date_str));
    let log_line = format!("[{}] {}\n", time_str, message);

    print!("{}", log_line);

    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(log_path) {
        let _ = file.write_all(log_line.as_bytes());
    }
}

// Logs are located alongside LayoutHijack.exe even when started by a shortcut
// or via Windows startup with an unrelated working directory.
fn log_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|parent| parent.join("logs")))
        .unwrap_or_else(|| PathBuf::from("logs"))
}