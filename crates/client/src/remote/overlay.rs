//! Окно указки у стримера: стрелка + ник поверх всего, мышь проходит насквозь, в захват экрана не попадает.
//! Своё окно на каждую указку, все — в одном потоке с циклом сообщений.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreatePen, CreateSolidBrush, DT_LEFT, DT_SINGLELINE, DT_VCENTER, DeleteObject, DrawTextW, EndPaint,
    FillRect, InvalidateRect, PAINTSTRUCT, PS_SOLID, Polygon, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, GetWindowDisplayAffinity, HWND_TOPMOST,
    LWA_COLORKEY, MSG, PostThreadMessageW, RegisterClassW, SWP_NOACTIVATE, SWP_SHOWWINDOW, SetLayeredWindowAttributes,
    SetWindowDisplayAffinity, SetWindowPos, TranslateMessage, WDA_EXCLUDEFROMCAPTURE, WM_APP, WM_PAINT, WM_QUIT, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

const WM_SYNC: u32 = WM_APP + 1;
const W: i32 = 240;
const H: i32 = 44;
/// Цвет-«прозрачность» (не рисуется).
const KEY: COLORREF = 0x00FF00FF;

#[derive(Debug, Clone, PartialEq)]
pub struct OverlayPointer {
    /// Острие стрелки, пиксели экрана.
    pub pos: (i32, i32),
    /// RGBA цвета участника.
    pub color: [u8; 4],
    pub name: String,
    /// Управляет — стрелку рисует настоящий курсор, показываем только ник.
    pub hidden_arrow: bool,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn rgb(c: [u8; 4]) -> COLORREF {
    c[0] as u32 | (c[1] as u32) << 8 | (c[2] as u32) << 16
}

thread_local! {
    static LOOK: RefCell<HashMap<isize, OverlayPointer>> = RefCell::new(HashMap::new());
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_PAINT {
        let mut ps: PAINTSTRUCT = unsafe { std::mem::zeroed() };
        let dc = unsafe { BeginPaint(hwnd, &mut ps) };
        let look = LOOK.with(|l| l.borrow().get(&(hwnd as isize)).cloned());
        unsafe {
            let bg = CreateSolidBrush(KEY);
            FillRect(dc, &RECT { left: 0, top: 0, right: W, bottom: H }, bg);
            DeleteObject(bg);
            if let Some(p) = look {
                let fill = CreateSolidBrush(rgb(p.color));
                let pen = CreatePen(PS_SOLID, 1, 0x00202020);
                let old_b = SelectObject(dc, fill);
                let old_p = SelectObject(dc, pen);
                if !p.hidden_arrow {
                    let arrow = [
                        POINT { x: 0, y: 0 },
                        POINT { x: 0, y: 19 },
                        POINT { x: 5, y: 14 },
                        POINT { x: 9, y: 22 },
                        POINT { x: 12, y: 21 },
                        POINT { x: 8, y: 13 },
                        POINT { x: 14, y: 13 },
                    ];
                    Polygon(dc, arrow.as_ptr(), arrow.len() as i32);
                }
                let label = RECT { left: 14, top: 20, right: W - 1, bottom: H - 1 };
                let font = CreateFontW(-14, 0, 0, 0, 600, 0, 0, 0, 1, 0, 0, 5, 0, wide("Segoe UI").as_ptr());
                let old_f = SelectObject(dc, font);
                let text: Vec<u16> = p.name.chars().take(24).collect::<String>().encode_utf16().collect();
                let mut measure = label;
                DrawTextW(dc, text.as_ptr(), text.len() as i32, &mut measure, DT_LEFT | DT_SINGLELINE | 0x400 /* DT_CALCRECT */);
                let bar = RECT { left: label.left, top: label.top, right: (measure.right + 12).min(W - 1), bottom: label.bottom };
                FillRect(dc, &bar, fill);
                SetBkMode(dc, TRANSPARENT as i32);
                SetTextColor(dc, 0x00FFFFFF);
                let mut tr = RECT { left: bar.left + 6, ..bar };
                DrawTextW(dc, text.as_ptr(), text.len() as i32, &mut tr, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
                SelectObject(dc, old_f);
                SelectObject(dc, old_b);
                SelectObject(dc, old_p);
                DeleteObject(font);
                DeleteObject(fill);
                DeleteObject(pen);
            }
            EndPaint(hwnd, &ps);
        }
        return 0;
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn create_window(class: &[u16]) -> Option<HWND> {
    let h = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class.as_ptr(),
            wide("murmur-pointer").as_ptr(),
            WS_POPUP,
            -32000,
            -32000,
            W,
            H,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            GetModuleHandleW(std::ptr::null()),
            std::ptr::null(),
        )
    };
    if h.is_null() {
        return None;
    }
    unsafe { SetLayeredWindowAttributes(h, KEY, 255, LWA_COLORKEY) };
    // Без исключения из захвата не показываем: иначе указка попадёт в трансляцию.
    let mut aff = 0u32;
    let ok = unsafe { SetWindowDisplayAffinity(h, WDA_EXCLUDEFROMCAPTURE) } != 0
        && unsafe { GetWindowDisplayAffinity(h, &mut aff) } != 0
        && aff == WDA_EXCLUDEFROMCAPTURE;
    if !ok {
        unsafe { DestroyWindow(h) };
        return None;
    }
    Some(h)
}

pub struct Overlay {
    thread_id: u32,
    wanted: Arc<Mutex<Vec<OverlayPointer>>>,
    #[cfg_attr(not(test), allow(dead_code))] // для тестов
    count: Arc<AtomicUsize>,
    #[cfg_attr(not(test), allow(dead_code))] // для тестов
    probe_ok: bool,
}

impl Overlay {
    /// None — окно указки не исключается из захвата (старая Windows): указку у стримера не показываем.
    pub fn new() -> Option<Overlay> {
        let wanted: Arc<Mutex<Vec<OverlayPointer>>> = Arc::default();
        let count = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = std::sync::mpsc::channel();
        let (w2, c2) = (wanted.clone(), count.clone());
        std::thread::Builder::new()
            .name("rc-overlay".into())
            .spawn(move || {
                let class = wide("MurmurRcPointer");
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(wnd_proc),
                    hInstance: unsafe { GetModuleHandleW(std::ptr::null()) },
                    lpszClassName: class.as_ptr(),
                    ..unsafe { std::mem::zeroed() }
                };
                unsafe { RegisterClassW(&wc) };
                let probe = create_window(&class);
                let _ = tx.send((unsafe { GetCurrentThreadId() }, probe.is_some()));
                let Some(probe) = probe else { return };
                let mut windows: HashMap<String, HWND> = HashMap::new();
                let mut msg: MSG = unsafe { std::mem::zeroed() };
                while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
                    if msg.hwnd.is_null() && msg.message == WM_SYNC {
                        let list = w2.lock().unwrap().clone();
                        sync(&class, &mut windows, &list);
                        c2.store(windows.len(), Ordering::Relaxed);
                        continue;
                    }
                    unsafe {
                        TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
                for (_, h) in windows.drain() {
                    unsafe { DestroyWindow(h) };
                }
                unsafe { DestroyWindow(probe) };
                c2.store(0, Ordering::Relaxed);
            })
            .ok()?;
        let (thread_id, probe_ok) = rx.recv().ok()?;
        if !probe_ok {
            log::warn!("управление: окно указки нельзя исключить из захвата — указка у стримера не показывается");
            return None;
        }
        Some(Overlay { thread_id, wanted, count, probe_ok })
    }

    pub fn set_pointers(&self, list: Vec<OverlayPointer>) {
        let mut w = self.wanted.lock().unwrap();
        if *w == list {
            return;
        }
        *w = list;
        drop(w);
        unsafe { PostThreadMessageW(self.thread_id, WM_SYNC, 0, 0) };
    }

    #[cfg_attr(not(test), allow(dead_code))] // для тестов
    pub fn probe_excluded(&self) -> bool {
        self.probe_ok
    }

    #[cfg_attr(not(test), allow(dead_code))] // для тестов
    pub fn window_count(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) };
    }
}

fn sync(class: &[u16], windows: &mut HashMap<String, HWND>, list: &[OverlayPointer]) {
    windows.retain(|name, h| {
        let keep = list.iter().any(|p| &p.name == name);
        if !keep {
            LOOK.with(|l| l.borrow_mut().remove(&(*h as isize)));
            unsafe { DestroyWindow(*h) };
        }
        keep
    });
    for p in list {
        let h = match windows.get(&p.name) {
            Some(h) => *h,
            None => {
                let Some(h) = create_window(class) else { continue };
                windows.insert(p.name.clone(), h);
                h
            }
        };
        let changed = LOOK.with(|l| {
            let mut l = l.borrow_mut();
            let prev = l.insert(h as isize, p.clone());
            prev.is_none_or(|o| o.color != p.color || o.hidden_arrow != p.hidden_arrow || o.name != p.name)
        });
        unsafe {
            SetWindowPos(h, HWND_TOPMOST, p.pos.0, p.pos.1, W, H, SWP_NOACTIVATE | SWP_SHOWWINDOW);
            if changed {
                InvalidateRect(h, std::ptr::null(), 1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_excluded_from_capture() {
        let o = Overlay::new().expect("overlay");
        assert!(o.probe_excluded());
        o.set_pointers(vec![OverlayPointer { pos: (-30000, -30000), color: [200, 80, 80, 255], name: "тест".into(), hidden_arrow: false }]);
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(o.window_count(), 1);
        o.set_pointers(Vec::new());
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(o.window_count(), 0);
    }
}
