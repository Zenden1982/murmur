//! Murmur — голосовой чат для своих. Десктоп-клиент (Windows).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod activity;
mod anim;
mod anim_img;
mod banner;
mod api;
mod app;
mod autosave;
mod camera;
mod channels_ui;
mod chat;
#[cfg(windows)]
mod clip;
mod config;
mod crash;
mod crop;
mod denoise;
mod dms;
mod emoji;
mod desktop;
mod frame;
mod friends;
#[cfg(debug_assertions)]
mod gallery;
mod gate;
mod kit;
mod layers;
mod legacy;
mod members_ui;
mod link;
mod metrics;
mod mictest;
mod model;
mod people;
mod profile_card;
mod picker;
mod pip;
mod server_settings;
mod servers_ui;
mod hotkeys;
mod ptt;
mod reactions;
mod remote;
mod remote_ui;
mod sounds;
mod stream;
mod theme;
mod tray;
mod gif_picker;
mod groupcalls;
mod notify;
mod unread;
mod update;
mod video;
mod voice;

// Символ STL 14.44 для предкомпилированного libwebrtc (см. crates/msvc-shim).
use msvc_shim as _;

/// Настройки wgpu. Отличие от умолчаний eframe — ошибка валидации поверхности (`Validation`):
/// по умолчанию кадр просто пропускается, поверхность остаётся битой, и egui-wgpu падает паникой
/// «Failed to create staging buffer» (падения 2026-11-23). Здесь поверхность пересоздаётся.
fn wgpu_options() -> eframe::egui_wgpu::WgpuConfiguration {
    use eframe::egui_wgpu::{SurfaceErrorAction, WgpuConfiguration};
    use eframe::wgpu::CurrentSurfaceTexture as S;
    let mut o = WgpuConfiguration::default().with_surface_config(eframe::egui_wgpu::SurfaceConfig::LOW_LATENCY);
    o.on_surface_status = std::sync::Arc::new(|status| match status {
        S::Outdated => SurfaceErrorAction::Reconfigure,
        S::Lost => SurfaceErrorAction::RecreateSurface,
        S::Occluded => SurfaceErrorAction::SkipFrame,
        other => {
            log::warn!("wgpu: кадр потерян ({other:?}), пересоздаю поверхность");
            SurfaceErrorAction::RecreateSurface
        }
    });
    o
}

/// Следим за GPU-устройством wgpu. Потеря устройства (`DeviceLost`) wgpu не показывает вообще: ошибка глотается,
/// «Dropped frame: Validation», а следующий кадр падает паникой в egui-wgpu (падения 2026-11-23 и 2026-11-24 20:38/20:45).
/// Здесь причина пишется в лог, а клиент перезапускается сам вместо падения. Устройство eframe пересоздать не умеет.
pub fn watch_gpu(device: &eframe::wgpu::Device) {
    device.on_uncaptured_error(std::sync::Arc::new(|e: eframe::wgpu::Error| {
        log::error!("wgpu: ошибка: {e}");
    }));
    let started = std::time::Instant::now();
    device.set_device_lost_callback(move |reason, msg| {
        log::error!("wgpu: устройство потеряно ({reason:?}): {msg}");
        // Destroyed — штатное закрытие. Перезапуск не чаще, чем раз в минуту работы (иначе петля при битом драйвере).
        if matches!(reason, eframe::wgpu::DeviceLostReason::Destroyed) || started.elapsed().as_secs() < 60 {
            return;
        }
        if let Ok(exe) = std::env::current_exe()
            && std::process::Command::new(exe)
                .arg(desktop::ARG_AFTER_UPDATE)
                .arg(std::process::id().to_string())
                .spawn()
                .is_ok()
        {
            log::error!("wgpu: перезапускаю Murmur");
            std::process::exit(0);
        }
    });
}

fn main() -> eframe::Result {
    let mut logger = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info,livekit=info,libwebrtc=warn,wgpu=warn,wgpu_core=warn,wgpu_hal=warn,egui_wgpu=warn,eframe=warn"),
    );
    // Лог — в файл (релиз без консоли): по нему разбираем зависания.
    if let Some(p) = config::log_path() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if std::fs::metadata(&p).is_ok_and(|m| m.len() > 2_000_000) {
            let _ = std::fs::rename(&p, p.with_file_name("murmur.old.log"));
        }
        if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
            logger.target(env_logger::Target::Pipe(Box::new(f)));
        }
    }
    logger.init();
    crash::install();
    let args = desktop::Args::parse(std::env::args().skip(1));
    // После обновления: дождаться, пока старая копия отпустит «единственный экземпляр».
    if let Some(pid) = args.after_update {
        desktop::wait_pid_exit(pid, 15_000);
    }
    if !desktop::claim_single_instance() {
        log::info!("Murmur уже запущен — показываю его окно");
        return Ok(());
    }
    if capture_win::diag::on() {
        log::info!("ДИАГ: диагностический режим — подробные логи трансляции и сети раз в секунду");
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("murmur-rt")
        .build()
        .expect("tokio runtime");
    #[allow(unused_mut)]
    let mut viewport = eframe::egui::ViewportBuilder::default();
    // Отладка (тестовые окна на втором мониторе владельца): MURMUR_WINDOW_POS="x,y" — где открыть окно, без фокуса.
    #[cfg(debug_assertions)]
    if let Some((x, y)) = std::env::var("MURMUR_WINDOW_POS").ok().and_then(|v| {
        let (x, y) = v.split_once(',')?;
        Some((x.trim().parse::<f32>().ok()?, y.trim().parse::<f32>().ok()?))
    }) {
        viewport = viewport.with_position([x, y]).with_active(false);
    }
    let options = eframe::NativeOptions {
        viewport: viewport
            .with_title(if capture_win::diag::on() { "Murmur · диагностика" } else { "Murmur" })
            // своя рамка и заголовок — frame.rs
            .with_decorations(false)
            .with_inner_size([1120.0, 700.0])
            .with_min_inner_size([880.0, 560.0])
            .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon-256.png")).unwrap_or_default()),
        wgpu_options: wgpu_options(),
        ..Default::default()
    };
    eframe::run_native("Murmur", options, Box::new(|cc| Ok(Box::new(app::App::new(cc, rt)))))
}
