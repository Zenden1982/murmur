//! Видео → кадры (Media Foundation, встроено в Windows): исходник для аватара/баннера профиля.
//! Читается по пути потоком, в памяти — один кадр. Звук не выбирается.

use std::path::Path;

use image::RgbaImage;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::core::{HSTRING, Interface};

const MF_VERSION: u32 = 0x0002_0070;
pub const EXTS: &[&str] = &["mp4", "m4v", "mov", "wmv", "webm"];
/// Видео-исходник не больше (читается потоком, не в память).
pub const VIDEO_SOURCE_MAX_BYTES: u64 = 100 * 1024 * 1024;
pub const CANT_OPEN: &str = "Windows не умеет открыть это видео — попробуйте MP4 (H.264).";

pub fn is_video(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| EXTS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// Итог прохода: сколько кадров отдано и были ли кадры после `max_s` (ролик урезан).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Walk {
    pub frames: usize,
    pub cut: bool,
}

/// MF + COM на время работы. COM снимается в конце: поток — из пула tokio, и оставленный MTA ломает
/// последующие диалоги выбора файла (rfd просит STA на том же потоке).
struct Mf {
    com: bool,
}
impl Mf {
    fn start() -> Result<Self, String> {
        let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        if let Err(e) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) } {
            if com {
                unsafe { CoUninitialize() };
            }
            return Err(format!("Media Foundation: {e}"));
        }
        Ok(Mf { com })
    }
}
impl Drop for Mf {
    fn drop(&mut self) {
        let _ = unsafe { MFShutdown() };
        if self.com {
            unsafe { CoUninitialize() };
        }
    }
}

const VIDEO: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

struct Reader {
    r: IMFSourceReader,
    w: u32,
    h: u32,
    stride: i32,
    /// Видимая часть кадра (x, y, w, h): кадр H.264 бывает выровнен (1080 → 1088), лишние строки — мусор.
    view: (u32, u32, u32, u32),
}

fn open(path: &Path) -> Result<Reader, String> {
    let cant = |e: windows::core::Error| {
        log::info!("видео {}: {e}", path.display());
        CANT_OPEN.to_string()
    };
    unsafe {
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 1).map_err(cant)?;
        let attrs = attrs.ok_or(CANT_OPEN)?;
        attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1).map_err(cant)?;
        let r = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), &attrs).map_err(cant)?;
        r.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false).map_err(cant)?;
        r.SetStreamSelection(VIDEO, true).map_err(cant)?;
        let t = MFCreateMediaType().map_err(cant)?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(cant)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32).map_err(cant)?;
        r.SetCurrentMediaType(VIDEO, None, &t).map_err(cant)?;
        let mut rd = Reader { r, w: 0, h: 0, stride: 0, view: (0, 0, 0, 0) };
        rd.read_format().map_err(cant)?;
        Ok(rd)
    }
}

impl Reader {
    /// Размер кадра и шаг строки из текущего формата (после смены формата — заново).
    fn read_format(&mut self) -> windows::core::Result<()> {
        unsafe {
            let t = self.r.GetCurrentMediaType(VIDEO)?;
            let size = t.GetUINT64(&MF_MT_FRAME_SIZE)?;
            self.w = (size >> 32) as u32;
            self.h = size as u32;
            self.stride = t.GetUINT32(&MF_MT_DEFAULT_STRIDE).map(|s| s as i32).unwrap_or(self.w as i32 * 4);
            let native = self.r.GetNativeMediaType(VIDEO, 0).ok();
            let area = aperture(&t).or_else(|| native.as_ref().and_then(aperture));
            self.view = match area {
                Some((x, y, w, h)) if w > 0 && h > 0 && x + w <= self.w && y + h <= self.h => (x, y, w, h),
                _ => (0, 0, self.w, self.h),
            };
        }
        Ok(())
    }

    /// Следующий кадр: (время в секундах, кадр) или None в конце.
    fn next(&mut self) -> Result<Option<(f64, RgbaImage)>, String> {
        loop {
            let (mut flags, mut ts) = (0u32, 0i64);
            let mut sample: Option<IMFSample> = None;
            unsafe { self.r.ReadSample(VIDEO, 0, None, Some(&mut flags), Some(&mut ts), Some(&mut sample)) }.map_err(|e| {
                log::info!("видео ReadSample: {e}");
                CANT_OPEN.to_string()
            })?;
            if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
                return Err(CANT_OPEN.into());
            }
            if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0 {
                self.read_format().map_err(|_| CANT_OPEN.to_string())?;
            }
            if let Some(s) = sample {
                return Ok(Some((ts as f64 / 1e7, self.convert(&s)?)));
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                return Ok(None);
            }
        }
    }

    /// RGB32 (BGRX) → RGBA (только видимая часть), альфа всегда 255. Шаг строки — из 2D-буфера, если он есть (учитывает «снизу вверх»).
    fn convert(&self, s: &IMFSample) -> Result<RgbaImage, String> {
        let (w, h) = (self.w as usize, self.h as usize);
        let (vx, vy, vw, vh) = (self.view.0 as usize, self.view.1 as usize, self.view.2 as usize, self.view.3 as usize);
        if w == 0 || h == 0 || vw == 0 || vh == 0 {
            return Err(CANT_OPEN.into());
        }
        let bad = |_| CANT_OPEN.to_string();
        let buf = unsafe { s.ConvertToContiguousBuffer() }.map_err(bad)?;
        let mut out = RgbaImage::new(vw as u32, vh as u32);
        let mut copy = |base: *const u8, pitch: isize, avail: Option<usize>| -> Result<(), String> {
            if pitch.unsigned_abs() < w * 4 {
                return Err(CANT_OPEN.into());
            }
            if let Some(len) = avail
                && len < pitch.unsigned_abs() * (h - 1) + w * 4
            {
                return Err(CANT_OPEN.into());
            }
            let dst = out.as_mut();
            for y in 0..vh {
                let row = unsafe { std::slice::from_raw_parts(base.offset(pitch * (vy + y) as isize).add(vx * 4), vw * 4) };
                let d = &mut dst[y * vw * 4..(y + 1) * vw * 4];
                for (o, i) in d.as_chunks_mut::<4>().0.iter_mut().zip(row.as_chunks::<4>().0) {
                    *o = [i[2], i[1], i[0], 255];
                }
            }
            Ok(())
        };
        if let Ok(b2) = buf.cast::<IMF2DBuffer>() {
            let (mut p, mut pitch) = (std::ptr::null_mut(), 0i32);
            if unsafe { b2.Lock2D(&mut p, &mut pitch) }.is_ok() {
                let r = copy(p, pitch as isize, None);
                let _ = unsafe { b2.Unlock2D() };
                return r.map(|_| out);
            }
        }
        let (mut p, mut len) = (std::ptr::null_mut(), 0u32);
        unsafe { buf.Lock(&mut p, None, Some(&mut len)) }.map_err(bad)?;
        let need = self.stride.unsigned_abs() as usize * (h - 1) + w * 4;
        let r = if (len as usize) < need {
            Err(CANT_OPEN.to_string())
        } else {
            // отрицательный шаг: первая строка картинки — в конце буфера
            let first = if self.stride < 0 { unsafe { p.add(self.stride.unsigned_abs() as usize * (h - 1)) } } else { p };
            copy(first, self.stride as isize, None)
        };
        let _ = unsafe { buf.Unlock() };
        r.map(|_| out)
    }
}

/// MF_MT_MINIMUM_DISPLAY_APERTURE (или геометрическая) → (x, y, w, h).
fn aperture(t: &IMFMediaType) -> Option<(u32, u32, u32, u32)> {
    for key in [&MF_MT_MINIMUM_DISPLAY_APERTURE, &MF_MT_GEOMETRIC_APERTURE] {
        let mut a = MFVideoArea::default();
        let mut got = 0u32;
        let size = std::mem::size_of::<MFVideoArea>() as u32;
        let ok = unsafe { t.GetBlob(key, std::slice::from_raw_parts_mut(&mut a as *mut _ as *mut u8, size as usize), Some(&mut got)) };
        if ok.is_ok() && got == size && a.OffsetX.value >= 0 && a.OffsetY.value >= 0 && a.Area.cx > 0 && a.Area.cy > 0 {
            return Some((a.OffsetX.value as u32, a.OffsetY.value as u32, a.Area.cx as u32, a.Area.cy as u32));
        }
    }
    None
}

/// Первый кадр (для окна обрезки).
pub fn first_frame(path: &Path) -> Result<RgbaImage, String> {
    let _mf = Mf::start()?;
    let mut r = open(path)?;
    r.next()?.map(|(_, f)| f).ok_or_else(|| "В видео нет кадров.".into())
}

/// Пройти по видео с частотой не выше `fps` не дольше `max_s` секунд: `f(кадр, задержка мс)`; `f` вернул false — стоп.
/// Время — от первого кадра (у некоторых MOV/MP4 оно не с нуля). Задержка — настоящий промежуток до следующего
/// взятого кадра: ролик с частотой ниже `fps` или с паузами (запись экрана) не ускоряется.
pub fn walk(path: &Path, fps: f64, max_s: f64, mut f: impl FnMut(RgbaImage, u32) -> Result<bool, String>) -> Result<Walk, String> {
    let _mf = Mf::start()?;
    let mut r = open(path)?;
    let step = 1.0 / fps;
    let ms = |s: f64| ((s * 1000.0).round() as u32).max(10);
    let (mut t0, mut next, mut frames, mut cut) = (None::<f64>, 0.0f64, 0usize, false);
    let mut pending: Option<(RgbaImage, f64)> = None;
    let mut gap = step; // последнему кадру — тот же промежуток, что был перед ним
    while let Some((ts, img)) = r.next()? {
        let t = ts - *t0.get_or_insert(ts);
        if t >= max_s - 1e-6 {
            cut = true;
            break;
        }
        if t + 1e-3 < next {
            continue;
        }
        while next <= t + 1e-3 {
            next += step;
        }
        if let Some((prev, pt)) = pending.take() {
            frames += 1;
            gap = t - pt;
            if !f(prev, ms(gap))? {
                return Ok(Walk { frames, cut: true });
            }
        }
        pending = Some((img, t));
    }
    let Some((last, _)) = pending else {
        return Err("В видео нет кадров.".into());
    };
    frames += 1;
    f(last, ms(gap))?;
    Ok(Walk { frames, cut })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/testdata/clip.mp4")
    }

    #[test]
    fn ext() {
        assert!(is_video(Path::new("a/B.MP4")));
        assert!(is_video(Path::new("x.webm")));
        assert!(!is_video(Path::new("x.gif")));
    }

    #[test]
    fn first_frame_size_and_opaque() {
        let f = first_frame(&clip()).unwrap();
        assert_eq!((f.width(), f.height()), (320, 180));
        assert!(f.pixels().all(|p| p[3] == 255));
        // testsrc: не однотонный кадр
        assert!(f.pixels().any(|p| p[0] > 200) && f.pixels().any(|p| p[0] < 50));
    }

    #[test]
    fn walk_whole_and_cut() {
        let mut n = 0;
        let w = walk(&clip(), 10.0, 10.0, |img, d| {
            assert_eq!((img.width(), img.height(), d), (320, 180, 100));
            n += 1;
            Ok(true)
        })
        .unwrap();
        assert!((39..=41).contains(&w.frames), "{w:?}");
        assert_eq!(n, w.frames);
        assert!(!w.cut);
        let w = walk(&clip(), 10.0, 2.0, |_, _| Ok(true)).unwrap();
        assert!((19..=21).contains(&w.frames), "{w:?}");
        assert!(w.cut);
    }

    #[test]
    fn slower_source_keeps_speed() {
        // ролик 30 кадр/с, просим 60: кадров ~120, задержки ~33 мс (не 17 — иначе ролик ускорится вдвое)
        let mut total = 0;
        let w = walk(&clip(), 60.0, 10.0, |_, d| {
            assert!((32..=35).contains(&d), "{d}");
            total += d;
            Ok(true)
        })
        .unwrap();
        assert!((118..=122).contains(&w.frames), "{w:?}");
        assert!((3900..=4100).contains(&total), "общая длина ~4 с: {total}");
    }

    #[test]
    fn not_a_video() {
        let p = std::env::temp_dir().join("murmur-not-video.mp4");
        std::fs::write(&p, b"hello, not a video").unwrap();
        let e = first_frame(&p).err().unwrap();
        assert!(e.contains("Windows не умеет"), "{e}");
    }
}
