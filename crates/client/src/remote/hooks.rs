//! Низкоуровневые хуки клавиатуры и мыши.
//! Стример: настоящий ввод (не SendInput) → пауза управления; Ctrl+Alt+Пробел → забрать всё; состояние настоящих клавиш для рации.
//! Управляющий: пока он управляет и окно Murmur в фокусе — клавиши уходят другу и не достаются своей системе.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use tokio::sync::mpsc::UnboundedSender;
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED, LLMHF_INJECTED, MSG,
    MSLLHOOKSTRUCT, PostThreadMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL, WH_MOUSE_LL,
    WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE,
    WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

use super::keys::{is_panic_combo, route_key, KeyRoute};

/// Сдвиг мыши стримера, после которого управление замирает.
const MOVE_THRESHOLD: i32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    /// Стример сам двигает мышь / нажимает клавиши.
    RealInput,
    /// Стример нажал Ctrl+Alt+Пробел.
    Panic,
    /// Клавиша управляющего — переслать другу.
    GuestKey { vk: u32, sc: u16, ext: bool, down: bool },
    /// Управляющий нажал правый Ctrl — вернуть указку.
    GuestReturn,
}

#[derive(Debug, Clone, Copy)]
pub struct Kbd {
    pub vk: u32,
    pub sc: u16,
    pub ext: bool,
    pub down: bool,
    pub injected: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
}

/// Shift/Ctrl/Alt (кроме правого Ctrl): пересылаются, но остаются и у своей системы (иначе не работает Alt+Tab).
fn is_modifier(vk: u32) -> bool {
    matches!(vk, 0x10 | 0x11 | 0x12 | 0xA0 | 0xA1 | 0xA2 | 0xA4 | 0xA5)
}

/// Решение по клавише: событие наружу и «проглотить» ли её.
pub fn classify_key(k: &Kbd, mods: Mods, host_watch: bool, guest_ptt: Option<u32>) -> (Option<HookEvent>, bool) {
    if k.injected {
        return (None, false);
    }
    if host_watch && is_panic_combo(k.vk, mods.ctrl, mods.alt) {
        return (k.down.then_some(HookEvent::Panic), true);
    }
    if let Some(ptt) = guest_ptt {
        return match route_key(k.vk, mods.ctrl, mods.alt, ptt) {
            KeyRoute::ReturnToPointer => (k.down.then_some(HookEvent::GuestReturn), true),
            KeyRoute::Local => (None, false),
            KeyRoute::Forward => {
                (Some(HookEvent::GuestKey { vk: k.vk, sc: k.sc, ext: k.ext, down: k.down }), !is_modifier(k.vk))
            }
        };
    }
    if host_watch && k.down {
        return (Some(HookEvent::RealInput), false);
    }
    (None, false)
}

/// Мышь стримера: кнопка/колесо или сдвиг больше порога — настоящий ввод.
pub fn classify_mouse(injected: bool, pos: (i32, i32), last: Option<(i32, i32)>, button: bool) -> Option<HookEvent> {
    if injected {
        return None;
    }
    if button {
        return Some(HookEvent::RealInput);
    }
    let (lx, ly) = last?;
    ((pos.0 - lx).abs() > MOVE_THRESHOLD || (pos.1 - ly).abs() > MOVE_THRESHOLD).then_some(HookEvent::RealInput)
}

static HOST_WATCH: AtomicBool = AtomicBool::new(false);
static GUEST_ON: AtomicBool = AtomicBool::new(false);
static GUEST_PTT: AtomicU32 = AtomicU32::new(0);
static CTRL: AtomicBool = AtomicBool::new(false);
static ALT: AtomicBool = AtomicBool::new(false);
/// Последнее положение настоящей мыши (x << 32 | y), i64::MIN — нет.
static LAST_MOUSE: AtomicI64 = AtomicI64::new(i64::MIN);
static REAL_DOWN: [AtomicBool; 256] = [const { AtomicBool::new(false) }; 256];
static TX: OnceLock<Mutex<Option<UnboundedSender<HookEvent>>>> = OnceLock::new();

fn emit(ev: HookEvent) {
    if let Some(tx) = TX.get().and_then(|m| m.lock().ok()).and_then(|g| g.clone()) {
        let _ = tx.send(ev);
    }
}

/// Запомнить состояние настоящей (не введённой программно) клавиши.
pub fn record_key(k: &Kbd) {
    if k.injected {
        return;
    }
    if let Some(slot) = REAL_DOWN.get(k.vk as usize) {
        slot.store(k.down, Ordering::Relaxed);
    }
    match k.vk {
        0x11 | 0xA2 | 0xA3 => CTRL.store(k.down, Ordering::Relaxed),
        0x12 | 0xA4 | 0xA5 => ALT.store(k.down, Ordering::Relaxed),
        _ => {}
    }
    // общий код Shift/Ctrl/Alt — для рации, назначенной на «Ctrl» и т. п.
    let generic = match k.vk {
        0xA0 | 0xA1 => Some(0x10),
        0xA2 | 0xA3 => Some(0x11),
        0xA4 | 0xA5 => Some(0x12),
        _ => None,
    };
    if let Some(g) = generic {
        REAL_DOWN[g].store(k.down, Ordering::Relaxed);
    }
}

/// Нажата ли клавиша/кнопка мыши по-настоящему (для рации, пока у стримера кто-то управляет).
pub fn real_key_down(vk: u32) -> bool {
    REAL_DOWN.get(vk as usize).is_some_and(|s| s.load(Ordering::Relaxed))
}

/// Хуки следят за стримером (у кого-то есть доступ к нашей трансляции).
pub fn host_watch_active() -> bool {
    HOST_WATCH.load(Ordering::Relaxed)
}

unsafe extern "system" fn kbd_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let info = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        let k = Kbd {
            vk: info.vkCode,
            sc: info.scanCode as u16,
            ext: info.flags & LLKHF_EXTENDED != 0,
            down: wparam as u32 == WM_KEYDOWN || wparam as u32 == WM_SYSKEYDOWN,
            injected: info.flags & LLKHF_INJECTED != 0,
        };
        let mods = Mods { ctrl: CTRL.load(Ordering::Relaxed), alt: ALT.load(Ordering::Relaxed) };
        record_key(&k);
        let guest = GUEST_ON.load(Ordering::Relaxed).then(|| GUEST_PTT.load(Ordering::Relaxed));
        let (ev, swallow) = classify_key(&k, mods, HOST_WATCH.load(Ordering::Relaxed), guest);
        if let Some(ev) = ev {
            emit(ev);
        }
        if swallow {
            return 1;
        }
    }
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && HOST_WATCH.load(Ordering::Relaxed) {
        let info = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
        let injected = info.flags & LLMHF_INJECTED != 0;
        let msg = wparam as u32;
        let pos = (info.pt.x, info.pt.y);
        if !injected {
            let xbtn = (info.mouseData >> 16) as u16;
            let vk = match msg {
                WM_LBUTTONDOWN | WM_LBUTTONUP => Some(0x01),
                WM_RBUTTONDOWN | WM_RBUTTONUP => Some(0x02),
                WM_MBUTTONDOWN | WM_MBUTTONUP => Some(0x04),
                WM_XBUTTONDOWN | WM_XBUTTONUP => Some(if xbtn == 1 { 0x05 } else { 0x06 }),
                _ => None,
            };
            if let Some(vk) = vk {
                let down = matches!(msg, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN);
                REAL_DOWN[vk].store(down, Ordering::Relaxed);
            }
        }
        // кнопки, колесо (WM_MOUSEWHEEL/WM_MOUSEHWHEEL) — всё, кроме движения
        let button = msg != WM_MOUSEMOVE;
        let packed = LAST_MOUSE.load(Ordering::Relaxed);
        let last = (packed != i64::MIN).then_some(((packed >> 32) as i32, packed as i32));
        let ev = classify_mouse(injected, pos, last, button);
        if !injected && (ev.is_some() || last.is_none()) {
            LAST_MOUSE.store(((pos.0 as i64) << 32) | (pos.1 as u32 as i64), Ordering::Relaxed);
        }
        if let Some(ev) = ev {
            emit(ev);
        }
    }
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

/// Поток с хуками. Пока жив — хуки стоят; `drop` снимает их.
pub struct Hooks {
    thread_id: u32,
}

impl Hooks {
    pub fn start(tx: UnboundedSender<HookEvent>) -> Hooks {
        *TX.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(tx);
        let (id_tx, id_rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("rc-hooks".into())
            .spawn(move || unsafe {
                let module = GetModuleHandleW(std::ptr::null());
                let kb = SetWindowsHookExW(WH_KEYBOARD_LL, Some(kbd_proc), module, 0);
                let ms = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), module, 0);
                if kb.is_null() || ms.is_null() {
                    log::warn!("управление: хуки ввода не установлены");
                }
                let _ = id_tx.send(GetCurrentThreadId());
                let mut msg: MSG = std::mem::zeroed();
                while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                if !kb.is_null() {
                    UnhookWindowsHookEx(kb);
                }
                if !ms.is_null() {
                    UnhookWindowsHookEx(ms);
                }
            })
            .expect("rc-hooks thread");
        Hooks { thread_id: id_rx.recv().unwrap_or(0) }
    }

    pub fn set_host_watch(&self, on: bool) {
        if !on {
            LAST_MOUSE.store(i64::MIN, Ordering::Relaxed);
        }
        HOST_WATCH.store(on, Ordering::Relaxed);
    }

    pub fn set_guest_capture(&self, on: bool, ptt_vk: u32) {
        GUEST_PTT.store(ptt_vk, Ordering::Relaxed);
        GUEST_ON.store(on, Ordering::Relaxed);
    }
}

impl Drop for Hooks {
    fn drop(&mut self) {
        HOST_WATCH.store(false, Ordering::Relaxed);
        GUEST_ON.store(false, Ordering::Relaxed);
        if self.thread_id != 0 {
            unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) };
        }
        if let Some(m) = TX.get() {
            *m.lock().unwrap() = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PTT: u32 = 0x56;

    fn k(vk: u32, down: bool, injected: bool) -> Kbd {
        Kbd { vk, sc: 0x1E, ext: false, down, injected }
    }

    #[test]
    fn injected_keys_ignored() {
        let m = Mods::default();
        assert_eq!(classify_key(&k(0x41, true, true), m, true, Some(PTT)), (None, false));
    }

    #[test]
    fn host_real_key_is_real_input() {
        assert_eq!(classify_key(&k(0x41, true, false), Mods::default(), true, None), (Some(HookEvent::RealInput), false));
        assert_eq!(classify_key(&k(0x41, true, false), Mods::default(), false, None), (None, false));
    }

    #[test]
    fn host_panic_combo_swallowed() {
        let m = Mods { ctrl: true, alt: true };
        assert_eq!(classify_key(&k(0x20, true, false), m, true, None), (Some(HookEvent::Panic), true));
        // отпускание пробела после — тоже проглатываем, чтобы не напечатать
        assert!(classify_key(&k(0x20, false, false), m, true, None).1);
    }

    #[test]
    fn guest_forward_swallows() {
        let ev = classify_key(&k(0x41, true, false), Mods::default(), false, Some(PTT));
        assert_eq!(ev, (Some(HookEvent::GuestKey { vk: 0x41, sc: 0x1E, ext: false, down: true }), true));
    }

    #[test]
    fn guest_modifiers_forwarded_not_swallowed() {
        let ev = classify_key(&k(0xA2, true, false), Mods::default(), false, Some(PTT));
        assert_eq!(ev, (Some(HookEvent::GuestKey { vk: 0xA2, sc: 0x1E, ext: false, down: true }), false));
    }

    #[test]
    fn guest_local_and_return() {
        assert_eq!(classify_key(&k(0x5B, true, false), Mods::default(), false, Some(PTT)), (None, false));
        assert_eq!(classify_key(&k(PTT, true, false), Mods::default(), false, Some(PTT)), (None, false));
        assert_eq!(classify_key(&k(0xA3, true, false), Mods::default(), false, Some(PTT)), (Some(HookEvent::GuestReturn), true));
        assert_eq!(classify_key(&k(0xA3, false, false), Mods::default(), false, Some(PTT)), (None, true));
    }

    #[test]
    fn real_key_state_ignores_injected() {
        record_key(&Kbd { vk: 0xE9, sc: 0, ext: false, down: true, injected: true });
        assert!(!real_key_down(0xE9));
        record_key(&Kbd { vk: 0xE9, sc: 0, ext: false, down: true, injected: false });
        assert!(real_key_down(0xE9));
        record_key(&Kbd { vk: 0xE9, sc: 0, ext: false, down: false, injected: false });
        assert!(!real_key_down(0xE9));
    }

    #[test]
    fn mouse_small_moves_ignored() {
        assert_eq!(classify_mouse(false, (10, 10), Some((12, 12)), false), None);
        assert_eq!(classify_mouse(false, (10, 10), Some((14, 10)), false), Some(HookEvent::RealInput));
        assert_eq!(classify_mouse(false, (10, 10), Some((10, 10)), true), Some(HookEvent::RealInput));
        assert_eq!(classify_mouse(true, (10, 10), Some((400, 10)), true), None);
    }
}
