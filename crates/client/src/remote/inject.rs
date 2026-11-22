//! Win32: куда попадёт клик/клавиша у стримера (проверки безопасности) и сам ввод (SendInput или журнал).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HWND, POINT, RECT};
use windows_sys::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO};
use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyboardLayout, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
    KEYEVENTF_SCANCODE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, SendInput,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GW_OWNER, GetAncestor, GetForegroundWindow, GetSystemMetrics, GetWindow, GetWindowThreadProcessId, IsIconic, IsWindow,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, WindowFromPoint,
};

use super::geom::ScreenRect;
use super::proto::Btn;

const XBUTTON1: u32 = 1;
const XBUTTON2: u32 = 2;

/// Что транслирует стример (из `capture_win::SourceId`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Monitor(isize),
    Window(isize),
}

/// Почему ввод не выполнен.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blocked {
    /// Окно самого Murmur (запрос, плашка «Забрать», настройки).
    OwnWindow,
    /// Поверх транслируемого окна другое окно.
    OtherWindow,
    /// Окно запущено от администратора.
    Elevated,
    /// Транслируемое окно закрыто/свёрнуто.
    Outside,
    /// Активно не транслируемое окно (для клавиш).
    NotForeground,
}

impl Blocked {
    pub fn text(self) -> &'static str {
        match self {
            Blocked::OwnWindow => "это окно Murmur — его нажимать нельзя",
            Blocked::OtherWindow => "поверх окна другое окно",
            Blocked::Elevated => "окно запущено от администратора — управление недоступно",
            Blocked::Outside => "окно недоступно",
            Blocked::NotForeground => "кликни в окно",
        }
    }
}

pub fn virtual_desk() -> ScreenRect {
    unsafe {
        ScreenRect {
            left: GetSystemMetrics(SM_XVIRTUALSCREEN),
            top: GetSystemMetrics(SM_YVIRTUALSCREEN),
            width: GetSystemMetrics(SM_CXVIRTUALSCREEN),
            height: GetSystemMetrics(SM_CYVIRTUALSCREEN),
        }
    }
}

fn rect(r: RECT) -> ScreenRect {
    ScreenRect { left: r.left, top: r.top, width: r.right - r.left, height: r.bottom - r.top }
}

/// Прямоугольник того, что видно в трансляции (монитор целиком или рамка окна, как её захватывает WGC).
pub fn target_rect(t: &Target) -> Option<ScreenRect> {
    match *t {
        Target::Monitor(h) => {
            let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
            info.cbSize = size_of::<MONITORINFO>() as u32;
            (unsafe { GetMonitorInfoW(h as HMONITOR, &mut info) } != 0).then(|| rect(info.rcMonitor))
        }
        Target::Window(h) => {
            let hwnd = h as HWND;
            if unsafe { IsWindow(hwnd) } == 0 {
                return None;
            }
            let mut r: RECT = unsafe { std::mem::zeroed() };
            let hr = unsafe {
                DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS as u32, (&raw mut r).cast(), size_of::<RECT>() as u32)
            };
            (hr >= 0 && r.right > r.left && r.bottom > r.top).then(|| rect(r))
        }
    }
}

/// Как `target_rect`, но None для свёрнутого окна и для прямоугольника вне рабочего стола.
pub fn visible_rect(t: &Target) -> Option<ScreenRect> {
    if let Target::Window(h) = *t
        && unsafe { IsIconic(h as HWND) } != 0
    {
        return None;
    }
    let r = target_rect(t)?;
    let d = virtual_desk();
    let overlaps = r.left < d.left + d.width && r.left + r.width > d.left && r.top < d.top + d.height && r.top + r.height > d.top;
    overlaps.then_some(r)
}

fn pid_of(h: HWND) -> u32 {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(h, &mut pid) };
    pid
}

fn token_elevated(process: HANDLE) -> Option<bool> {
    let mut token: HANDLE = std::ptr::null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return None;
    }
    let mut e = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut len = 0u32;
    let ok = unsafe {
        GetTokenInformation(token, TokenElevation, (&raw mut e).cast(), size_of::<TOKEN_ELEVATION>() as u32, &mut len)
    };
    unsafe { CloseHandle(token) };
    (ok != 0).then_some(e.TokenIsElevated != 0)
}

fn self_elevated() -> bool {
    token_elevated(unsafe { GetCurrentProcess() }).unwrap_or(false)
}

/// Окно другого процесса «от администратора» (или процесс не открыть) при нашем обычном — SendInput туда не дойдёт.
fn elevated_over_us(pid: u32) -> bool {
    if self_elevated() {
        return false;
    }
    let p = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if p.is_null() {
        return true;
    }
    let e = token_elevated(p).unwrap_or(true);
    unsafe { CloseHandle(p) };
    e
}

/// Окно `w` относится к транслируемому `target`: оно само, его всплывающие (owner) или окно того же процесса.
fn belongs(w: HWND, target: HWND) -> bool {
    let root = unsafe { GetAncestor(w, GA_ROOT) };
    if root == target {
        return true;
    }
    let mut o = root;
    for _ in 0..16 {
        o = unsafe { GetWindow(o, GW_OWNER) };
        if o.is_null() {
            break;
        }
        if o == target {
            return true;
        }
    }
    pid_of(root) == pid_of(target)
}

fn check_window(w: HWND, t: &Target, missing: Blocked) -> Result<(), Blocked> {
    if w.is_null() {
        return Err(missing);
    }
    let pid = pid_of(w);
    if pid == unsafe { GetCurrentProcessId() } {
        return Err(Blocked::OwnWindow);
    }
    if let Target::Window(h) = *t {
        let target = h as HWND;
        if unsafe { IsWindow(target) } == 0 {
            return Err(Blocked::Outside);
        }
        if !belongs(w, target) {
            return Err(missing);
        }
    }
    if elevated_over_us(pid) {
        return Err(Blocked::Elevated);
    }
    Ok(())
}

/// Можно ли кликнуть в точку экрана (пиксели).
pub fn check_point(t: &Target, px: i32, py: i32) -> Result<(), Blocked> {
    let w = unsafe { WindowFromPoint(POINT { x: px, y: py }) };
    check_window(w, t, Blocked::OtherWindow)
}

/// Можно ли нажимать клавиши: в фокусе должно быть транслируемое окно (или его всплывающее), не Murmur.
pub fn check_keys(t: &Target) -> Result<(), Blocked> {
    let w = unsafe { GetForegroundWindow() };
    check_window(w, t, Blocked::NotForeground)
}

/// Раскладка активного окна: «RU», «EN», … или код языка (4 hex).
pub fn foreground_layout() -> String {
    let w = unsafe { GetForegroundWindow() };
    let tid = if w.is_null() { 0 } else { unsafe { GetWindowThreadProcessId(w, std::ptr::null_mut()) } };
    let hkl = unsafe { GetKeyboardLayout(tid) } as usize;
    let lang = (hkl & 0xffff) as u16;
    match lang & 0x3ff {
        0x19 => "RU".into(),
        0x09 => "EN".into(),
        0x22 => "UK".into(),
        0x23 => "BE".into(),
        0x3f => "KK".into(),
        0x07 => "DE".into(),
        0x0c => "FR".into(),
        _ => format!("{lang:04x}"),
    }
}

pub trait Injector: Send {
    /// Абсолютные координаты SendInput (0..65535 по виртуальному рабочему столу).
    fn mouse_to(&mut self, abs: (i32, i32));
    fn button(&mut self, b: Btn, down: bool);
    fn wheel(&mut self, dx: i16, dy: i16);
    fn key(&mut self, sc: u16, ext: bool, down: bool);
}

pub struct SendInputInjector;

fn mouse(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

fn send(inputs: &[INPUT]) {
    let n = unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), size_of::<INPUT>() as i32) };
    if n as usize != inputs.len() {
        log::warn!("SendInput: отправлено {n} из {}", inputs.len());
    }
}

impl Injector for SendInputInjector {
    fn mouse_to(&mut self, abs: (i32, i32)) {
        send(&[mouse(MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK, abs.0, abs.1, 0)]);
    }

    fn button(&mut self, b: Btn, down: bool) {
        let (flags, data) = match (b, down) {
            (Btn::L, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (Btn::L, false) => (MOUSEEVENTF_LEFTUP, 0),
            (Btn::R, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (Btn::R, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (Btn::M, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (Btn::M, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (Btn::X1, d) => (if d { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, XBUTTON1 as i32),
            (Btn::X2, d) => (if d { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, XBUTTON2 as i32),
        };
        send(&[mouse(flags, 0, 0, data)]);
    }

    fn wheel(&mut self, dx: i16, dy: i16) {
        if dy != 0 {
            send(&[mouse(MOUSEEVENTF_WHEEL, 0, 0, dy as i32)]);
        }
        if dx != 0 {
            send(&[mouse(MOUSEEVENTF_HWHEEL, 0, 0, dx as i32)]);
        }
    }

    fn key(&mut self, sc: u16, ext: bool, down: bool) {
        let mut flags = KEYEVENTF_SCANCODE;
        if ext {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if !down {
            flags |= KEYEVENTF_KEYUP;
        }
        send(&[INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: 0, wScan: sc, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
        }]);
    }
}

/// Стенд (`AMADEUS_RC_DRYRUN=1`): вместо ввода — строки в журнал. Настоящий курсор не двигается.
#[derive(Default, Clone)]
pub struct DryRun {
    n: Arc<AtomicUsize>,
}

impl DryRun {
    #[cfg_attr(not(test), allow(dead_code))] // для тестов
    pub fn count(&self) -> usize {
        self.n.load(Ordering::Relaxed)
    }

    fn note(&self, what: std::fmt::Arguments) {
        self.n.fetch_add(1, Ordering::Relaxed);
        log::info!("ДИАГ rc: {what}");
    }
}

impl Injector for DryRun {
    fn mouse_to(&mut self, abs: (i32, i32)) {
        self.note(format_args!("мышь {abs:?}"));
    }
    fn button(&mut self, b: Btn, down: bool) {
        self.note(format_args!("кнопка {b:?} {}", if down { "вниз" } else { "вверх" }));
    }
    fn wheel(&mut self, dx: i16, dy: i16) {
        self.note(format_args!("колесо {dx} {dy}"));
    }
    fn key(&mut self, sc: u16, ext: bool, down: bool) {
        self.note(format_args!("клавиша sc={sc:#x} ext={ext} {}", if down { "вниз" } else { "вверх" }));
    }
}

pub fn dry_run() -> bool {
    std::env::var("AMADEUS_RC_DRYRUN").is_ok_and(|v| v == "1")
}

pub fn injector() -> Box<dyn Injector> {
    if dry_run() { Box::new(DryRun::default()) } else { Box::new(SendInputInjector) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    #[test]
    fn own_window_blocked() {
        let class = wide("BUTTON");
        let h = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class.as_ptr(),
                wide("rc-test").as_ptr(),
                WS_POPUP | WS_VISIBLE,
                0,
                0,
                120,
                80,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        assert!(!h.is_null());
        let desk = virtual_desk();
        assert!(desk.width > 0);
        let r = check_point(&Target::Monitor(0), 60, 40);
        unsafe { DestroyWindow(h) };
        assert_eq!(r, Err(Blocked::OwnWindow));
    }

    #[test]
    fn dry_run_counts_and_never_sendinput() {
        let mut d = DryRun::default();
        d.mouse_to((100, 200));
        d.button(Btn::L, true);
        d.wheel(0, -120);
        d.key(0x1E, false, true);
        assert_eq!(d.count(), 4);
    }

    #[test]
    fn layout_two_letters_or_hex() {
        let l = foreground_layout();
        assert!(
            (l.len() == 2 && l.chars().all(|c| c.is_ascii_uppercase())) || (l.len() == 4 && l.chars().all(|c| c.is_ascii_hexdigit())),
            "{l}"
        );
    }

    #[test]
    fn target_rect_of_missing_window_is_none() {
        assert_eq!(target_rect(&Target::Window(0x7fff_fff0)), None);
    }
}
