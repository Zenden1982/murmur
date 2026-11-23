//! Диагностический режим (сборка «с отладкой» для владельца): подробные логи трансляции раз в секунду.
//! Включается фичей `diag` (`cargo build -p client --release --features diag`) или `MURMUR_DIAG=1`.

use std::mem::size_of;
use std::sync::OnceLock;

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, GetDC, GetDIBits, ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowTextW};

/// Включён ли диагностический режим.
pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| cfg!(feature = "diag") || std::env::var("MURMUR_DIAG").is_ok_and(|v| v == "1"))
}

const SAMPLE: i32 = 64;

/// Хэш пикселей ЭКРАНА (не захвата) в квадрате 64×64 по центру окна: меняется ли картинка на самом экране.
/// Если хэш меняется, а живых кадров захвата нет — молчит WGC; если не меняется — программа сама не рисует
/// (или окно чем-то закрыто — смотри «перекрыто»).
pub fn screen_sample(hwnd: isize) -> Option<u64> {
    let h = HWND(hwnd as *mut std::ffi::c_void);
    let mut r = RECT::default();
    unsafe { DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS, (&raw mut r).cast(), size_of::<RECT>() as u32) }.ok()?;
    let (cx, cy) = ((r.left + r.right) / 2 - SAMPLE / 2, (r.top + r.bottom) / 2 - SAMPLE / 2);
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, SAMPLE, SAMPLE);
        let old = SelectObject(mem, bmp.into());
        let ok = BitBlt(mem, 0, 0, SAMPLE, SAMPLE, Some(screen), cx, cy, SRCCOPY).is_ok();
        let mut buf = vec![0u8; (SAMPLE * SAMPLE * 4) as usize];
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: SAMPLE,
                biHeight: -SAMPLE,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let lines = GetDIBits(mem, bmp, 0, SAMPLE as u32, Some(buf.as_mut_ptr().cast()), &mut bi, DIB_RGB_COLORS);
        SelectObject(mem, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        if !ok || lines == 0 {
            return None;
        }
        Some(buf.iter().fold(0xcbf29ce484222325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3)))
    }
}

/// Заголовок окна на переднем плане (что у стримера в фокусе).
pub fn foreground_title() -> String {
    let mut buf = [0u16; 128];
    let n = unsafe { GetWindowTextW(GetForegroundWindow(), &mut buf) };
    let s = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
    if s.chars().count() > 40 { s.chars().take(40).chain(['…']).collect() } else { s }
}

/// Следит, меняется ли экран в области окна (для строки диагностики).
#[derive(Default)]
pub struct ScreenWatch {
    last: Option<u64>,
}

impl ScreenWatch {
    /// «да» / «нет» / «?» (окно за экраном или GDI не смог).
    pub fn changed(&mut self, hwnd: isize) -> &'static str {
        let now = screen_sample(hwnd);
        let r = match (self.last, now) {
            (Some(a), Some(b)) if a != b => "да",
            (Some(_), Some(_)) => "нет",
            _ => "?",
        };
        self.last = now;
        r
    }
}
