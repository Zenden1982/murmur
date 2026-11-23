//! Уведомления: toast Windows, мигание кнопки в панели задач, число на значке, режим «Не беспокоить».
//! Что и когда показывать — решает приложение (app.rs); здесь — чистая логика (упоминания, «Не беспокоить»)
//! и обёртки над Windows.

use std::sync::Mutex;

use proto::{Message, User};
use serde::{Deserialize, Serialize};

use crate::desktop::Window;

/// «Не беспокоить»: выкл / вкл / авто (пока идёт моя трансляция).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Dnd {
    #[default]
    Off,
    On,
    Auto,
}

impl Dnd {
    pub fn active(self, streaming: bool) -> bool {
        match self {
            Dnd::Off => false,
            Dnd::On => true,
            Dnd::Auto => streaming,
        }
    }

}

/// Есть ли в сообщении упоминание меня: @логин, @отображаемое имя, @все / @everyone / @here.
pub fn mentions(m: &Message, me: &User) -> bool {
    let text = m.content.to_lowercase();
    let mut names = vec![me.name.to_lowercase(), "все".to_string(), "everyone".into(), "here".into()];
    if let Some(d) = me.display_name.as_deref().filter(|d| !d.trim().is_empty()) {
        names.push(d.trim().to_lowercase());
    }
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices('@').any(|(i, _)| {
        let rest = &text[i + 1..];
        // перед «@» не буква (иначе это почта)
        let before_ok = text[..i].chars().next_back().is_none_or(|c| !is_word(c));
        before_ok
            && names.iter().any(|n| {
                rest.strip_prefix(n.as_str()).is_some_and(|tail| tail.chars().next().is_none_or(|c| !is_word(c)))
            })
    })
}

/// Короткий текст сообщения для toast.
pub fn preview(m: &Message) -> String {
    let t: String = m.content.split_whitespace().collect::<Vec<_>>().join(" ");
    let t = if t.is_empty() {
        if m.sticker.is_some() {
            "Стикер".to_string()
        } else if !m.attachments.is_empty() {
            "Вложение".to_string()
        } else {
            String::new()
        }
    } else {
        t
    };
    if t.chars().count() > 120 { format!("{}…", t.chars().take(120).collect::<String>()) } else { t }
}

/// Куда перейти по клику на toast: "dm:<id>" / "ch:<id>".
static CLICK: Mutex<Option<String>> = Mutex::new(None);

pub fn take_click() -> Option<String> {
    CLICK.lock().ok().and_then(|mut c| c.take())
}

#[cfg(windows)]
pub use imp::Notifier;
#[cfg(not(windows))]
pub use stub::Notifier;

#[cfg(windows)]
mod imp {
    use std::sync::mpsc::{Sender, channel};

    use eframe::egui;
    use windows::Foundation::TypedEventHandler;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject};
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
    use windows::Win32::UI::Shell::{ITaskbarList3, TaskbarList};
    use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, DestroyIcon, HICON, ICONINFO};
    use windows::core::{HSTRING, PCWSTR};

    use super::{CLICK, Window};

    /// Идентификатор приложения — тот же, что у ярлыков установщика (иначе Windows не покажет toast).
    const AUMID: &str = "ru.murmurchat.murmur";

    struct Toast {
        title: String,
        body: String,
        tag: String,
    }

    pub struct Notifier {
        window: Window,
        tx: Sender<Toast>,
        badge: u32,
        taskbar: Option<ITaskbarList3>,
    }

    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn register_aumid() {
        use windows_sys::Win32::System::Registry::*;
        let key = wide(&format!("Software\\Classes\\AppUserModelId\\{AUMID}"));
        let name = wide("DisplayName");
        let val = wide("Murmur");
        unsafe {
            let mut h: HKEY = std::ptr::null_mut();
            if RegCreateKeyExW(HKEY_CURRENT_USER, key.as_ptr(), 0, std::ptr::null(), 0, KEY_SET_VALUE, std::ptr::null(), &mut h, std::ptr::null_mut()) == 0 {
                RegSetValueExW(h, name.as_ptr(), 0, REG_SZ, val.as_ptr() as *const u8, (val.len() * 2) as u32);
                RegCloseKey(h);
            }
            let id = wide(AUMID);
            windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(id.as_ptr());
        }
    }

    fn show_toast(t: &Toast, window: Window, ctx: &egui::Context) -> windows::core::Result<()> {
        let xml = windows::Data::Xml::Dom::XmlDocument::new()?;
        xml.LoadXml(&HSTRING::from(format!(
            "<toast><visual><binding template=\"ToastText02\"><text id=\"1\">{}</text><text id=\"2\">{}</text></binding></visual><audio silent=\"true\"/></toast>",
            xml_escape(&t.title),
            xml_escape(&t.body)
        )))?;
        let toast = ToastNotification::CreateToastNotification(&xml)?;
        let (tag, ctx) = (t.tag.clone(), ctx.clone());
        toast.Activated(&TypedEventHandler::new(move |_, _| {
            if let Ok(mut c) = CLICK.lock() {
                *c = Some(tag.clone());
            }
            crate::desktop::show(window);
            ctx.request_repaint();
            Ok(())
        }))?;
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))?.Show(&toast)
    }

    impl Notifier {
        pub fn new(window: Window, ctx: egui::Context) -> Self {
            register_aumid();
            let (tx, rx) = channel::<Toast>();
            // отдельный поток с COM: обработчик клика живёт, пока живёт поток
            std::thread::spawn(move || {
                unsafe {
                    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                }
                for t in rx {
                    if let Err(e) = show_toast(&t, window, &ctx) {
                        log::warn!("toast не показан: {e}");
                    }
                }
            });
            Self { window, tx, badge: 0, taskbar: None }
        }

        pub fn toast(&self, title: &str, body: &str, tag: &str) {
            let _ = self.tx.send(Toast { title: title.into(), body: body.into(), tag: tag.into() });
        }

        /// Мигание кнопки в панели задач, пока окно не выйдет на передний план.
        pub fn flash(&self) {
            use windows_sys::Win32::UI::WindowsAndMessaging::{FLASHW_TIMERNOFG, FLASHW_TRAY, FLASHWINFO, FlashWindowEx};
            if self.window.0 == 0 {
                return;
            }
            let fi = FLASHWINFO {
                cbSize: size_of::<FLASHWINFO>() as u32,
                hwnd: self.window.0 as _,
                dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG,
                uCount: 0,
                dwTimeout: 0,
            };
            unsafe { FlashWindowEx(&fi) };
        }

        /// Число на значке в панели задач; 0 — убрать. Только из UI-потока.
        pub fn set_badge(&mut self, n: u32) {
            if n == self.badge || self.window.0 == 0 {
                return;
            }
            self.badge = n;
            if self.taskbar.is_none() {
                self.taskbar = unsafe { CoCreateInstance::<_, ITaskbarList3>(&TaskbarList, None, CLSCTX_INPROC_SERVER) }
                    .ok()
                    .filter(|t| unsafe { t.HrInit() }.is_ok());
            }
            let Some(tb) = &self.taskbar else { return };
            let hwnd = HWND(self.window.0 as _);
            unsafe {
                if n == 0 {
                    let _ = tb.SetOverlayIcon(hwnd, HICON::default(), PCWSTR::null());
                } else if let Some(icon) = badge_icon(n) {
                    let desc = HSTRING::from(format!("Непрочитанных: {n}"));
                    let _ = tb.SetOverlayIcon(hwnd, icon, &desc);
                    let _ = DestroyIcon(icon);
                }
            }
        }
    }

    /// Красный кружок 32×32 с числом (шрифт 3×5 пикселей).
    fn badge_icon(n: u32) -> Option<HICON> {
        const GLYPHS: [[u8; 5]; 11] = [
            [0b111, 0b101, 0b101, 0b101, 0b111],
            [0b010, 0b110, 0b010, 0b010, 0b111],
            [0b111, 0b001, 0b111, 0b100, 0b111],
            [0b111, 0b001, 0b111, 0b001, 0b111],
            [0b101, 0b101, 0b111, 0b001, 0b001],
            [0b111, 0b100, 0b111, 0b001, 0b111],
            [0b111, 0b100, 0b111, 0b101, 0b111],
            [0b111, 0b001, 0b010, 0b010, 0b010],
            [0b111, 0b101, 0b111, 0b101, 0b111],
            [0b111, 0b101, 0b111, 0b001, 0b111],
            [0b000, 0b010, 0b111, 0b010, 0b000], // «+»
        ];
        let digits: Vec<usize> = if n > 99 {
            vec![9, 10]
        } else {
            n.to_string().bytes().map(|b| (b - b'0') as usize).collect()
        };
        const S: usize = 32;
        let mut px = vec![0u8; S * S * 4]; // BGRA
        for y in 0..S {
            for x in 0..S {
                let (dx, dy) = (x as f32 + 0.5 - 16.0, y as f32 + 0.5 - 16.0);
                if dx * dx + dy * dy <= 15.5 * 15.5 {
                    let i = (y * S + x) * 4;
                    px[i..i + 4].copy_from_slice(&[0x3a, 0x3a, 0xe0, 0xff]); // красный
                }
            }
        }
        let scale = if digits.len() == 1 { 4 } else { 3 };
        let total_w = (digits.len() * 3 + digits.len() - 1) * scale;
        let (x0, y0) = ((S - total_w) / 2, (S - 5 * scale) / 2);
        for (k, d) in digits.iter().enumerate() {
            for (row, bits) in GLYPHS[*d].iter().enumerate() {
                for col in 0..3 {
                    if bits & (0b100 >> col) != 0 {
                        for sy in 0..scale {
                            for sx in 0..scale {
                                let x = x0 + (k * 4 + col) * scale + sx;
                                let y = y0 + row * scale + sy;
                                let i = (y * S + x) * 4;
                                px[i..i + 4].copy_from_slice(&[0xff, 0xff, 0xff, 0xff]);
                            }
                        }
                    }
                }
            }
        }
        unsafe {
            let color = CreateBitmap(S as i32, S as i32, 1, 32, Some(px.as_ptr() as *const _));
            let mask = CreateBitmap(S as i32, S as i32, 1, 1, None);
            let info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
            let icon = CreateIconIndirect(&info).ok();
            let _ = DeleteObject(color.into());
            let _ = DeleteObject(mask.into());
            icon
        }
    }
}

#[cfg(not(windows))]
mod stub {
    use super::Window;
    use eframe::egui;

    pub struct Notifier;

    impl Notifier {
        pub fn new(_w: Window, _ctx: egui::Context) -> Self {
            Self
        }
        pub fn toast(&self, _title: &str, _body: &str, _tag: &str) {}
        pub fn flash(&self) {}
        pub fn set_badge(&mut self, _n: u32) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user() -> User {
        serde_json::from_str(r#"{"id":1,"name":"Neo","is_admin":false,"display_name":"Нео"}"#).unwrap()
    }
    fn msg(text: &str) -> Message {
        serde_json::from_value(serde_json::json!({
            "id": 1, "channel_id": 2, "author": {"id": 3, "name": "x"}, "content": text, "created_at": 0
        }))
        .unwrap()
    }

    #[test]
    fn mention_detection() {
        let me = user();
        assert!(mentions(&msg("привет @neo!"), &me));
        assert!(mentions(&msg("@Нео, ты тут?"), &me));
        assert!(mentions(&msg("всем @все"), &me));
        assert!(mentions(&msg("@everyone"), &me));
        assert!(!mentions(&msg("neo без собаки"), &me));
        assert!(!mentions(&msg("пиши на a@neo.ru"), &me));
        assert!(!mentions(&msg("@neon"), &me));
    }

    #[test]
    fn dnd_modes() {
        assert!(!Dnd::Off.active(true));
        assert!(Dnd::On.active(false));
        assert!(Dnd::Auto.active(true));
        assert!(!Dnd::Auto.active(false));
    }
}
