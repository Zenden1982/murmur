//! Интеграция с Windows: одна копия приложения, окно в трей (спрятать/показать), автозапуск (HKCU\…\Run),
//! ожидание выхода старой копии после обновления, открытие ссылок в браузере.
//!
//! Окно прячется и показывается напрямую через `ShowWindow`, а не `ViewportCommand::Visible`: так winit не
//! путается в своём флаге видимости, а показать окно можно из обработчика трея, пока кадры не рисуются.

use std::path::{Path, PathBuf};

/// Аргумент автозапуска: стартовать свёрнутым в трей.
pub const ARG_TRAY: &str = "--tray";
/// Аргумент перезапуска после обновления: `--after-update <pid старой копии>`.
pub const ARG_AFTER_UPDATE: &str = "--after-update";

/// Окно приложения (HWND как число — его можно передавать между потоками).
#[derive(Clone, Copy, Debug, Default)]
pub struct Window(pub isize);

impl Window {
    pub fn from_cc(cc: &eframe::CreationContext<'_>) -> Self {
        #[cfg(windows)]
        {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            if let Ok(h) = cc.window_handle()
                && let RawWindowHandle::Win32(h) = h.as_raw()
            {
                return Self(h.hwnd.get());
            }
        }
        let _ = cc;
        Self(0)
    }
}

/// Имя «метки» копии приложения: своя для каждого каталога настроек (второй тестовый клиент на том же ПК — отдельно).
fn instance_tag() -> String {
    let dir = std::env::var("AMADEUS_CONFIG_DIR").unwrap_or_default();
    // FNV-1a: стабильно между запусками (DefaultHasher — нет)
    let h = dir.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    format!("Amadeus.Instance.{h:016x}")
}

#[cfg(windows)]
mod win {
    use super::*;
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HWND, LPARAM};
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW};
    use windows_sys::Win32::System::Threading::{CreateMutexW, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetPropW, IsIconic, IsWindowVisible, SW_HIDE, SW_RESTORE, SW_SHOW, SW_SHOWMINNOACTIVE, SW_SHOWNORMAL,
        SetForegroundWindow, SetPropW, ShowWindow,
    };

    pub fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Строковое значение из HKCU (`value` = "" — значение по умолчанию). Нет ключа/значения — None.
    pub fn hkcu_string(key: &str, value: &str) -> Option<String> {
        use windows_sys::Win32::System::Registry::{RRF_RT_REG_SZ, RegGetValueW};
        let (k, v) = (wide(key), wide(value));
        let mut buf = vec![0u16; 1024];
        let mut len = (buf.len() * 2) as u32;
        let rc = unsafe {
            RegGetValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut len)
        };
        if rc != 0 {
            return None;
        }
        let n = (len as usize / 2).saturating_sub(1);
        Some(String::from_utf16_lossy(&buf[..n]))
    }

    /// Открыть файл как из проводника (установщик запускается со своим UAC/окном).
    pub fn run_file(path: &Path) -> bool {
        let (verb, file) = (wide("open"), wide(&path.to_string_lossy()));
        let h = unsafe { ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL) };
        h as isize > 32
    }

    pub fn claim_single_instance() -> bool {
        let name = wide(&format!("Local\\{}", instance_tag()));
        // Мьютекс не закрываем: живёт до конца процесса.
        let h = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if h.is_null() {
            return true; // не смогли проверить — запускаемся
        }
        unsafe { GetLastError() != ERROR_ALREADY_EXISTS }
    }

    pub fn mark(w: Window) {
        let tag = wide(&instance_tag());
        unsafe { SetPropW(w.0 as HWND, tag.as_ptr(), 1 as _) };
    }

    unsafe extern "system" fn find_marked(hwnd: HWND, lparam: LPARAM) -> windows_sys::core::BOOL {
        let (tag, found) = unsafe { &mut *(lparam as *mut (Vec<u16>, isize)) };
        if !unsafe { GetPropW(hwnd, tag.as_ptr()) }.is_null() {
            *found = hwnd as isize;
            return 0; // хватит
        }
        1
    }

    pub fn activate_other() -> bool {
        let mut st: (Vec<u16>, isize) = (wide(&instance_tag()), 0);
        unsafe { EnumWindows(Some(find_marked), &mut st as *mut _ as LPARAM) };
        if st.1 == 0 {
            return false;
        }
        show(Window(st.1));
        true
    }

    pub fn show(w: Window) {
        let h = w.0 as HWND;
        if h.is_null() {
            return;
        }
        unsafe {
            ShowWindow(h, if IsIconic(h) != 0 { SW_RESTORE } else { SW_SHOW });
            SetForegroundWindow(h);
        }
    }

    pub fn is_visible(w: Window) -> bool {
        w.0 != 0 && unsafe { IsWindowVisible(w.0 as HWND) } != 0
    }

    pub fn minimize_inactive(w: Window) {
        if w.0 != 0 {
            unsafe { ShowWindow(w.0 as HWND, SW_SHOWMINNOACTIVE) };
        }
    }

    pub fn hide(w: Window) {
        if w.0 != 0 {
            unsafe { ShowWindow(w.0 as HWND, SW_HIDE) };
        }
    }

    const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";

    pub fn set_autostart(exe: Option<&Path>) -> Result<(), String> {
        let key = wide(RUN_KEY);
        // до 0.3.4 запись называлась «Amadeus» и вела на amadeus.exe — убираем её, пишем «Murmur»
        let old = wide("Amadeus");
        unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), old.as_ptr()) };
        let name = wide("Murmur");
        let err = match exe {
            Some(exe) => {
                let cmd = wide(&format!("\"{}\" {ARG_TRAY}", exe.display()));
                unsafe {
                    RegSetKeyValueW(
                        HKEY_CURRENT_USER,
                        key.as_ptr(),
                        name.as_ptr(),
                        REG_SZ,
                        cmd.as_ptr().cast(),
                        (cmd.len() * 2) as u32,
                    )
                }
            }
            None => match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) } {
                2 => 0, // ERROR_FILE_NOT_FOUND — и так нет
                e => e,
            },
        };
        if err == 0 { Ok(()) } else { Err(format!("реестр: ошибка {err}")) }
    }

    pub fn wait_pid_exit(pid: u32, timeout_ms: u32) {
        let h = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if h.is_null() {
            return; // уже вышел
        }
        unsafe {
            WaitForSingleObject(h, timeout_ms);
            CloseHandle(h);
        }
    }

    pub fn open_url(url: &str) {
        let op = wide("open");
        let url = wide(url);
        unsafe {
            ShellExecuteW(std::ptr::null_mut(), op.as_ptr(), url.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL)
        };
    }
}

/// Занять «единственную копию». false — уже запущено (та копия показана), этой надо выйти.
pub fn claim_single_instance() -> bool {
    #[cfg(windows)]
    {
        if win::claim_single_instance() {
            return true;
        }
        // окно первой копии может ещё создаваться — пару раз подождать
        for _ in 0..10 {
            if win::activate_other() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        false
    }
    #[cfg(not(windows))]
    true
}

/// Пометить окно — по метке его находит вторая копия, чтобы показать.
pub fn mark_window(w: Window) {
    #[cfg(windows)]
    win::mark(w);
    #[cfg(not(windows))]
    let _ = w;
}

pub fn show(w: Window) {
    #[cfg(windows)]
    win::show(w);
    #[cfg(not(windows))]
    let _ = w;
}

/// Окно видно (в том числе свёрнутым в панель задач)?
pub fn is_visible(w: Window) -> bool {
    #[cfg(windows)]
    return win::is_visible(w);
    #[cfg(not(windows))]
    {
        let _ = w;
        true
    }
}

/// Свернуть, не активируя (запуск из автозагрузки).
pub fn minimize_inactive(w: Window) {
    #[cfg(windows)]
    win::minimize_inactive(w);
    #[cfg(not(windows))]
    let _ = w;
}

pub fn hide(w: Window) {
    #[cfg(windows)]
    win::hide(w);
    #[cfg(not(windows))]
    let _ = w;
}

/// Подождать, пока завершится старая копия (после обновления), но не дольше `timeout_ms`.
pub fn wait_pid_exit(pid: u32, timeout_ms: u32) {
    #[cfg(windows)]
    win::wait_pid_exit(pid, timeout_ms);
    #[cfg(not(windows))]
    let _ = (pid, timeout_ms);
}

pub fn open_url(url: &str) {
    #[cfg(windows)]
    win::open_url(url);
    #[cfg(not(windows))]
    let _ = url;
}

/// Строковое значение реестра HKCU (только Windows).
pub fn hkcu_string(key: &str, value: &str) -> Option<String> {
    #[cfg(windows)]
    {
        win::hkcu_string(key, value)
    }
    #[cfg(not(windows))]
    {
        let _ = (key, value);
        None
    }
}

/// Запустить файл (установщик).
/// Запустить программу с аргументами (без окна консоли и без ожидания).
pub fn spawn_with_args(path: &Path, args: &[&str]) -> bool {
    std::process::Command::new(path).args(args).spawn().is_ok()
}

pub fn run_file(path: &Path) -> bool {
    #[cfg(windows)]
    {
        win::run_file(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        false
    }
}

/// Завершить процесс немедленно. `std::process::exit` выгружает DLL по очереди — если поток видеодрайвера/WGC/WebRTC
/// завис (у друга 21.11 — после зависшей остановки захвата), выход мог висеть бесконечно. TerminateProcess не ждёт никого.
pub fn exit_now() -> ! {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
        TerminateProcess(GetCurrentProcess(), 0);
    }
    std::process::exit(0)
}

/// Путь к своему exe.
pub fn current_exe() -> Option<PathBuf> {
    std::env::current_exe().ok().map(|p| dunce_simplify(&p))
}

/// `\\?\C:\…` → `C:\…` (так путь можно писать в реестр и показывать).
fn dunce_simplify(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => p.to_path_buf(),
    }
}

/// Установлено установщиком (рядом лежит `uninstall.exe`)? Только тогда трогаем автозапуск:
/// сборку из `target\release` в автозагрузку не прописываем.
pub fn is_installed() -> bool {
    current_exe().and_then(|e| e.parent().map(|d| d.join("uninstall.exe").is_file())).unwrap_or(false)
}

/// Прописать (Some) или убрать (None) автозапуск: `"…\murmur.exe" --tray`.
pub fn set_autostart(on: bool) -> Result<(), String> {
    #[cfg(windows)]
    {
        let exe = if on { Some(current_exe().ok_or("не найден путь к программе")?) } else { None };
        win::set_autostart(exe.as_deref())
    }
    #[cfg(not(windows))]
    {
        let _ = on;
        Ok(())
    }
}

/// Аргументы командной строки, которые понимает клиент.
#[derive(Debug, Default, PartialEq)]
pub struct Args {
    /// Запуск из автозагрузки: сразу в трей.
    pub tray: bool,
    /// Перезапуск после обновления: pid старой копии.
    pub after_update: Option<u32>,
}

impl Args {
    pub fn parse(mut it: impl Iterator<Item = String>) -> Self {
        let mut a = Self::default();
        while let Some(s) = it.next() {
            match s.as_str() {
                ARG_TRAY => a.tray = true,
                ARG_AFTER_UPDATE => a.after_update = it.next().and_then(|p| p.parse().ok()),
                _ => {}
            }
        }
        a
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args() {
        let p = |v: &[&str]| Args::parse(v.iter().map(|s| s.to_string()));
        assert_eq!(p(&[]), Args::default());
        assert_eq!(p(&["--tray"]), Args { tray: true, after_update: None });
        assert_eq!(p(&["--after-update", "1234", "--x"]), Args { tray: false, after_update: Some(1234) });
        assert_eq!(p(&["--after-update"]), Args::default());
    }

    #[test]
    fn simplify_verbatim_path() {
        assert_eq!(dunce_simplify(Path::new(r"\\?\C:\a\b.exe")), PathBuf::from(r"C:\a\b.exe"));
        assert_eq!(dunce_simplify(Path::new(r"C:\a.exe")), PathBuf::from(r"C:\a.exe"));
    }

    #[test]
    fn instance_tag_is_stable() {
        assert_eq!(instance_tag(), instance_tag());
        assert!(instance_tag().starts_with("Amadeus.Instance."));
    }
}
