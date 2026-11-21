//! Активность «играет в …» (только Windows). Раз в 20 с — без чтения памяти процессов и всего, что античит
//! мог бы счесть вмешательством: только реестр Steam и список имён процессов.
//! 1. Steam: `HKCU\Software\Valve\Steam\RunningAppID` ≠ 0 → название из `appmanifest_<id>.acf` в библиотеках Steam.
//! 2. Иначе — известные игры по имени .exe (`assets/games.json`).
//!
//! На сервер уходят только название и время начала.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

const PERIOD: Duration = Duration::from_secs(20);

/// Во что играю: (название, unix-время начала).
pub type Game = Option<(String, i64)>;

pub struct Detector {
    rx: mpsc::Receiver<Game>,
    stop: Arc<AtomicBool>,
}

impl Drop for Detector {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Detector {
    /// Последнее изменение (если было).
    pub fn poll(&self) -> Option<Game> {
        let mut last = None;
        while let Ok(g) = self.rx.try_recv() {
            last = Some(g);
        }
        last
    }
}

/// Запустить фоновую проверку; первое значение придёт сразу.
pub fn start(ctx: eframe::egui::Context) -> Detector {
    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let _ = std::thread::Builder::new().name("amadeus-activity".into()).spawn(move || {
        let known = known_games();
        let mut cur: Game = None;
        let mut first = true;
        while !stop2.load(Ordering::Relaxed) {
            let name = detect(&known);
            if first || name.as_deref() != cur.as_ref().map(|c| c.0.as_str()) {
                first = false;
                cur = name.map(|n| (n, now()));
                log::info!("активность: {}", cur.as_ref().map(|c| c.0.as_str()).unwrap_or("—"));
                if tx.send(cur.clone()).is_err() {
                    return;
                }
                ctx.request_repaint();
            }
            // спим кусками, чтобы быстро остановиться
            for _ in 0..(PERIOD.as_millis() / 500) {
                if stop2.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    });
    Detector { rx, stop }
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Имя exe (в нижнем регистре) → название игры.
pub fn known_games() -> HashMap<String, String> {
    serde_json::from_str::<HashMap<String, String>>(include_str!("../assets/games.json"))
        .unwrap_or_default()
        .into_iter()
        .map(|(k, v)| (k.to_lowercase(), v))
        .collect()
}

fn detect(known: &HashMap<String, String>) -> Option<String> {
    steam_game().or_else(|| process_game(known))
}

/// Первая известная игра среди имён процессов.
pub fn pick_known(names: impl Iterator<Item = String>, known: &HashMap<String, String>) -> Option<&String> {
    let mut found: Vec<&String> = names.filter_map(|n| known.get(&n.to_lowercase())).collect();
    found.sort();
    found.first().copied()
}

/// Значение `"key"\t\t"value"` из файла Valve KeyValues (.acf/.vdf), все вхождения.
pub fn vdf_values(text: &str, key: &str) -> Vec<String> {
    let pat = format!("\"{key}\"");
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            let rest = l.strip_prefix(&pat)?.trim();
            let v = rest.strip_prefix('"')?.strip_suffix('"')?;
            Some(v.replace("\\\\", "\\"))
        })
        .collect()
}

#[cfg(windows)]
mod win {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS};
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn reg_dword(path: &str, name: &str) -> Option<u32> {
        let (p, n) = (wide(path), wide(name));
        let mut v: u32 = 0;
        let mut size = 4u32;
        // SAFETY: буфер 4 байта под DWORD, строки с нулём в конце.
        let r = unsafe { RegGetValueW(HKEY_CURRENT_USER, p.as_ptr(), n.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), (&mut v as *mut u32).cast(), &mut size) };
        (r == 0).then_some(v)
    }

    pub fn reg_string(path: &str, name: &str) -> Option<String> {
        let (p, n) = (wide(path), wide(name));
        let mut buf = vec![0u16; 1024];
        let mut size = (buf.len() * 2) as u32;
        // SAFETY: размер буфера в байтах передан в size.
        let r = unsafe { RegGetValueW(HKEY_CURRENT_USER, p.as_ptr(), n.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut size) };
        if r != 0 {
            return None;
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }

    /// Имена exe запущенных процессов.
    pub fn process_names() -> Vec<String> {
        let mut out = Vec::new();
        // SAFETY: снимок закрывается в конце; структура с правильным dwSize.
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == INVALID_HANDLE_VALUE {
                return out;
            }
            let mut e: PROCESSENTRY32W = std::mem::zeroed();
            e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut ok = Process32FirstW(snap, &mut e) != 0;
            while ok {
                let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                out.push(String::from_utf16_lossy(&e.szExeFile[..len]));
                ok = Process32NextW(snap, &mut e) != 0;
            }
            CloseHandle(snap);
        }
        out
    }
}

#[cfg(windows)]
fn steam_game() -> Option<String> {
    let id = win::reg_dword("Software\\Valve\\Steam", "RunningAppID").filter(|&id| id != 0)?;
    let steam = win::reg_string("Software\\Valve\\Steam", "SteamPath")?;
    let steam = std::path::PathBuf::from(steam);
    let mut libs = vec![steam.clone()];
    if let Ok(vdf) = std::fs::read_to_string(steam.join("steamapps").join("libraryfolders.vdf")) {
        libs.extend(vdf_values(&vdf, "path").into_iter().map(std::path::PathBuf::from));
    }
    libs.iter().find_map(|lib| {
        let acf = std::fs::read_to_string(lib.join("steamapps").join(format!("appmanifest_{id}.acf"))).ok()?;
        vdf_values(&acf, "name").into_iter().next().filter(|n| !n.is_empty())
    })
}

#[cfg(windows)]
fn process_game(known: &HashMap<String, String>) -> Option<String> {
    pick_known(win::process_names().into_iter(), known).cloned()
}

#[cfg(not(windows))]
fn steam_game() -> Option<String> {
    None
}

#[cfg(not(windows))]
fn process_game(_: &HashMap<String, String>) -> Option<String> {
    None
}

/// «уже 1 ч 5 мин» / «уже 12 мин» / «только что».
pub fn elapsed_text(since: i64, now: i64) -> String {
    let m = (now - since).max(0) / 60;
    match m {
        0 => "только что".into(),
        m if m < 60 => format!("уже {m} мин"),
        m if m % 60 == 0 => format!("уже {} ч", m / 60),
        m => format!("уже {} ч {} мин", m / 60, m % 60),
    }
}

/// Строка под ником: свой статус важнее игры.
pub fn line(a: &proto::Activity) -> Option<(bool, String)> {
    if let Some(s) = a.status.as_deref().filter(|s| !s.is_empty()) {
        return Some((false, s.to_string()));
    }
    a.game.as_deref().map(|g| (true, format!("играет в {g}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn games_list_parses() {
        let k = known_games();
        assert_eq!(k.get("dota2.exe").map(String::as_str), Some("Dota 2"));
        assert!(k.len() > 50);
    }

    #[test]
    fn picks_known_case_insensitive() {
        let k = known_games();
        let names = ["explorer.exe", "CS2.exe", "chrome.exe"].map(String::from);
        assert_eq!(pick_known(names.into_iter(), &k).map(String::as_str), Some("Counter-Strike 2"));
        assert_eq!(pick_known(["notepad.exe".to_string()].into_iter(), &k), None);
    }

    #[test]
    fn vdf() {
        let acf = "\"AppState\"\n{\n\t\"appid\"\t\t\"570\"\n\t\"name\"\t\t\"Dota 2\"\n}";
        assert_eq!(vdf_values(acf, "name"), vec!["Dota 2"]);
        let lib = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"D:\\\\SteamLibrary\"\n\t}\n}";
        assert_eq!(vdf_values(lib, "path"), vec!["D:\\SteamLibrary"]);
    }

    #[test]
    fn elapsed() {
        assert_eq!(elapsed_text(0, 30), "только что");
        assert_eq!(elapsed_text(0, 12 * 60), "уже 12 мин");
        assert_eq!(elapsed_text(0, 3600), "уже 1 ч");
        assert_eq!(elapsed_text(0, 3900), "уже 1 ч 5 мин");
    }

    #[test]
    fn status_beats_game() {
        let a = proto::Activity { game: Some("Dota 2".into()), since: None, status: Some("чиню сервер".into()) };
        assert_eq!(line(&a), Some((false, "чиню сервер".into())));
        let a = proto::Activity { status: None, ..a };
        assert_eq!(line(&a), Some((true, "играет в Dota 2".into())));
    }
}
