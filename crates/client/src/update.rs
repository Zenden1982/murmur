//! Автообновление: раз в 6 часов спрашиваем у бэкенда `/version`; если там новее — в заголовке окна загорается
//! «Обновление», по кнопке скачиваем новый exe (`/download/…`, gzip если есть), сверяем размер и SHA-256,
//! подменяем свой exe (запущенный файл Windows разрешает переименовать) и перезапускаемся.
//! Старый exe остаётся рядом как `<имя>.exe.old` и удаляется при следующем запуске.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use eframe::egui;
use proto::ReleaseInfo;
use sha2::{Digest, Sha256};

use crate::api::Api;
use crate::desktop;

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
const FIRST_CHECK: Duration = Duration::from_secs(4);
const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);
const RETRY_AFTER_ERROR: Duration = Duration::from_secs(15 * 60);

enum Ev {
    Checked(Result<Option<ReleaseInfo>, String>),
    Installed(Result<(), String>),
}

pub struct Updater {
    rt: tokio::runtime::Handle,
    ctx: egui::Context,
    api: Api,
    tx: std_mpsc::Sender<Ev>,
    rx: std_mpsc::Receiver<Ev>,
    /// Проверять самим (в release; в debug — только с AMADEUS_UPDATE_CHECK=1).
    auto: bool,
    next_check: Instant,
    pub checking: bool,
    /// Ручная проверка уже отвечала (для надписи «у вас последняя версия»).
    pub checked_once: bool,
    /// Доступная более новая версия.
    pub available: Option<ReleaseInfo>,
    pub error: Option<String>,
    pub installing: bool,
    progress: Arc<AtomicU64>,
    /// Новый exe на месте — осталось перезапуститься.
    pub ready: bool,
}

impl Updater {
    pub fn new(rt: tokio::runtime::Handle, ctx: egui::Context, api: Api) -> Self {
        cleanup_old();
        let (tx, rx) = std_mpsc::channel();
        let auto = !cfg!(debug_assertions) || std::env::var_os("AMADEUS_UPDATE_CHECK").is_some();
        Self {
            rt,
            ctx,
            api,
            tx,
            rx,
            auto,
            next_check: Instant::now() + FIRST_CHECK,
            checking: false,
            checked_once: false,
            available: None,
            error: None,
            installing: false,
            progress: Arc::new(AtomicU64::new(0)),
            ready: false,
        }
    }

    pub fn set_api(&mut self, api: Api) {
        self.api = api;
    }

    /// Раз в кадр: результаты фоновых задач и плановая проверка.
    pub fn poll(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Ev::Checked(r) => {
                    self.checking = false;
                    self.checked_once = true;
                    match r {
                        Ok(info) => {
                            self.error = None;
                            self.available = info.filter(|i| proto::is_newer(&i.version, CURRENT)).filter(|i| {
                                let ok = i.verify();
                                if !ok {
                                    log::warn!("обновление {} без верной подписи издателя — пропущено", i.version);
                                }
                                ok
                            });
                            self.next_check = Instant::now() + CHECK_EVERY;
                        }
                        Err(e) => {
                            log::warn!("проверка обновлений: {e}");
                            self.error = Some(e);
                            self.next_check = Instant::now() + RETRY_AFTER_ERROR;
                        }
                    }
                }
                Ev::Installed(r) => {
                    self.installing = false;
                    match r {
                        Ok(()) => self.ready = true,
                        Err(e) => {
                            log::error!("обновление: {e}");
                            self.error = Some(e);
                        }
                    }
                }
            }
        }
        if self.auto && !self.checking && !self.installing && !self.ready {
            let now = Instant::now();
            if now >= self.next_check {
                self.check();
            } else {
                self.ctx.request_repaint_after(self.next_check - now);
            }
        }
    }

    pub fn check(&mut self) {
        if self.checking {
            return;
        }
        self.checking = true;
        let (api, tx, ctx) = (self.api.clone(), self.tx.clone(), self.ctx.clone());
        self.rt.spawn(async move {
            let r = api.latest_release().await.map_err(|e| e.message);
            let _ = tx.send(Ev::Checked(r));
            ctx.request_repaint();
        });
    }

    /// Скачать и подменить exe (доступная версия должна быть).
    pub fn install(&mut self) {
        let Some(info) = self.available.clone() else { return };
        if self.installing || self.ready {
            return;
        }
        self.installing = true;
        self.error = None;
        self.progress.store(0, Ordering::Relaxed);
        let (base, tx, ctx, progress) = (self.api.base().to_string(), self.tx.clone(), self.ctx.clone(), self.progress.clone());
        self.rt.spawn(async move {
            let r = download_and_swap(&base, &info, &progress, &ctx).await;
            let _ = tx.send(Ev::Installed(r));
            ctx.request_repaint();
        });
    }

    /// Доля скачанного 0..1.
    pub fn progress(&self) -> f32 {
        match &self.available {
            Some(i) if i.size > 0 => (self.progress.load(Ordering::Relaxed) as f64 / i.size as f64).min(1.0) as f32,
            _ => 0.0,
        }
    }

    /// Ссылка на установщик (если самообновление не удалось — скачать руками).
    pub fn installer_url(&self) -> String {
        format!("{}/get", self.api.base())
    }

    /// Запустить новый exe; вызывающий после этого закрывает приложение.
    pub fn restart(&self) -> Result<(), String> {
        let exe = desktop::current_exe().ok_or("не найден путь к программе")?;
        std::process::Command::new(&exe)
            .arg(desktop::ARG_AFTER_UPDATE)
            .arg(std::process::id().to_string())
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("не удалось запустить новую версию: {e}"))
    }
}

fn sibling(exe: &Path, suffix: &str) -> PathBuf {
    let mut s = exe.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Убрать `<имя>.exe.old*` и недокачанный `.new` от прошлых обновлений.
fn cleanup_old() {
    let Some(exe) = desktop::current_exe() else { return };
    let (Some(dir), Some(name)) = (exe.parent(), exe.file_name().and_then(|n| n.to_str())) else { return };
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let n = e.file_name();
        let n = n.to_string_lossy();
        if n.starts_with(&format!("{name}.old")) || n == format!("{name}.new") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn human_io(e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        "нет прав на запись в папку программы — скачайте установщик".into()
    } else {
        e.to_string()
    }
}

async fn download_and_swap(base: &str, info: &ReleaseInfo, progress: &AtomicU64, ctx: &egui::Context) -> Result<(), String> {
    if !info.verify() {
        return Err("у обновления нет верной подписи — не устанавливаю".into());
    }
    let exe = desktop::current_exe().ok_or("не найден путь к программе")?;
    let new = sibling(&exe, ".new");
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(60))
        .user_agent(concat!("Murmur/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())?;
    let url = format!("{base}/download/{}", info.file);
    let mut resp = http.get(&url).send().await.map_err(|e| format!("сеть: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("сервер ответил {}", resp.status()));
    }
    let file = std::fs::File::create(&new).map_err(|e| human_io(&e))?;
    let mut out = std::io::BufWriter::with_capacity(1 << 20, file);
    let mut hasher = Sha256::new();
    let mut got: u64 = 0;
    let res: Result<(), String> = async {
        while let Some(chunk) = resp.chunk().await.map_err(|e| format!("обрыв загрузки: {e}"))? {
            got += chunk.len() as u64;
            if got > info.size {
                return Err("файл больше, чем заявлено".into());
            }
            hasher.update(&chunk);
            out.write_all(&chunk).map_err(|e| human_io(&e))?;
            progress.store(got, Ordering::Relaxed);
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        out.flush().map_err(|e| human_io(&e))?;
        if got != info.size {
            return Err(format!("скачано {got} байт из {}", info.size));
        }
        let sum = hex(&hasher.finalize());
        if sum != info.sha256 {
            return Err("контрольная сумма не совпала — файл повреждён".into());
        }
        Ok(())
    }
    .await;
    drop(out);
    if let Err(e) = res {
        let _ = std::fs::remove_file(&new);
        return Err(e);
    }
    swap(&exe, &new).map_err(|e| {
        let _ = std::fs::remove_file(&new);
        human_io(&e)
    })
}

/// `exe` → `exe.old` (или `.old-<pid>`, если старый ещё занят), `new` → `exe`; при сбое — вернуть как было.
fn swap(exe: &Path, new: &Path) -> std::io::Result<()> {
    let mut old = sibling(exe, ".old");
    if old.exists() && std::fs::remove_file(&old).is_err() {
        old = sibling(exe, &format!(".old-{}", std::process::id()));
    }
    std::fs::rename(exe, &old)?;
    if let Err(e) = std::fs::rename(new, exe) {
        let _ = std::fs::rename(&old, exe);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_and_sibling() {
        assert_eq!(hex(&[0x00, 0xab, 0x10]), "00ab10");
        assert_eq!(sibling(Path::new(r"C:\x\murmur.exe"), ".old"), PathBuf::from(r"C:\x\murmur.exe.old"));
    }

    #[test]
    fn swap_replaces_and_keeps_old() {
        let dir = std::env::temp_dir().join(format!("amadeus-swap-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("murmur.exe");
        let new = dir.join("murmur.exe.new");
        std::fs::write(&exe, b"old").unwrap();
        std::fs::write(&new, b"new").unwrap();
        std::fs::write(sibling(&exe, ".old"), b"older").unwrap();
        swap(&exe, &new).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        assert_eq!(std::fs::read(sibling(&exe, ".old")).unwrap(), b"old");
        assert!(!new.exists());
        // нет нового файла — exe остаётся на месте
        assert!(swap(&exe, &new).is_err());
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
