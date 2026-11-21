//! Аппаратный путь трансляции: WGC (свой free-threaded захват на D3D11-устройстве с VIDEO_SUPPORT) →
//! BGRA→NV12 и масштабирование на GPU (ID3D11VideoProcessor) → аппаратный H.264-энкодер Media Foundation
//! (NVENC / AMF / QSV как асинхронный MFT) → готовые H.264 access units (Annex B) в приёмник.
//!
//! Кадр не читается в память CPU. Размер видео фиксируется при старте (по пресету); если окно меняет размер,
//! картинка вписывается в кадр с полями (энкодер не пересоздаётся).

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4};
use windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess;
use windows::core::{GUID, IInspectable, Interface, PWSTR};
use windows_capture::graphics_capture_api::GraphicsCaptureApi;
use windows_capture::settings::GraphicsCaptureItemType;

use crate::capture::{CaptureConfig, Event, OCCLUSION_CHECK, STALL, target_size};
use crate::sources::SourceId;

const MF_E_TRANSFORM_NEED_MORE_INPUT: i32 = 0xC00D6D72u32 as i32;
const MF_E_TRANSFORM_STREAM_CHANGE: i32 = 0xC00D6D61u32 as i32;
const MF_VERSION: u32 = 0x0002_0070;
const RING: usize = 4;
/// Как часто повторять последний кадр, когда картинка стоит. Повтор нужен не только «для живости»: запрос ключевого кадра
/// (PLI после потери) доходит до нас только вместе с очередным кадром через WebRTC, а приёмник замечает потерю хвоста
/// кадра только по следующему пакету — поэтому редкий повтор (раньше 1 с) затягивал восстановление после потерь.
/// Опыты: `AMADEUS_REPEAT_MS=N`.
fn repeat_every() -> Duration {
    static V: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        let ms = std::env::var("AMADEUS_REPEAT_MS").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(250);
        Duration::from_millis(ms.clamp(50, 5000))
    })
}
/// Сколько размер окна должен не меняться, прежде чем пересоздавать энкодер (перетаскивание края — десятки размеров).
const RESIZE_SETTLE: Duration = Duration::from_millis(600);
const POLL: Duration = Duration::from_millis(50);
/// Нижняя граница битрейта по запросам GCC (ниже картинка разваливается).
const MIN_BITRATE: u32 = 300_000;
const START_BITRATE: u32 = 2_500_000;

/// Получатель готового H.264. Вызывается из потоков энкодера/захвата.
pub trait EncodedSink: Send + Sync + 'static {
    /// Один access unit H.264 (Annex B, с SPS/PPS на ключевых кадрах).
    fn encoded(&self, data: &[u8], keyframe: bool, timestamp_us: i64, width: u32, height: u32);
    /// Зритель просит ключевой кадр (PLI/FIR, новый подписчик).
    fn take_keyframe_request(&self) -> bool;
    /// Новый целевой битрейт от контроля перегрузки WebRTC, бит/с.
    fn take_bitrate_request(&self) -> Option<u32>;
    fn event(&self, ev: Event);
}

/// COM-объекты D3D/MF, которые мы передаём между потоками. D3D11-устройство — с SetMultithreadProtected,
/// видеоконтекст и MFT используются под нашими мьютексами.
struct Sx<T>(T);
unsafe impl<T> Send for Sx<T> {}
unsafe impl<T> Sync for Sx<T> {}

type R<T> = Result<T, String>;

fn e<E: std::fmt::Display>(what: &'static str) -> impl FnOnce(E) -> String {
    move |err| format!("{what}: {err}")
}

fn now_us() -> i64 {
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_micros() as i64
}

// ------------------------------------------------------------------ D3D11

fn create_device() -> R<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut ctx = None;
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut ctx),
        )
        .map_err(e("D3D11CreateDevice"))?;
    }
    let device = device.ok_or("нет D3D11-устройства")?;
    let ctx = ctx.ok_or("нет D3D11-контекста")?;
    if let Ok(mt) = ctx.cast::<ID3D11Multithread>() {
        let _ = unsafe { mt.SetMultithreadProtected(true) };
    }
    Ok((device, ctx))
}

fn adapter_luid(device: &ID3D11Device) -> Option<u64> {
    let dxgi: IDXGIDevice = device.cast().ok()?;
    let adapter = unsafe { dxgi.GetAdapter() }.ok()?;
    let desc = unsafe { adapter.GetDesc() }.ok()?;
    Some(((desc.AdapterLuid.HighPart as u32 as u64) << 32) | desc.AdapterLuid.LowPart as u64)
}

/// GPU-конвертер BGRA → NV12 с масштабированием; кольцо из RING выходных текстур.
struct Converter {
    device: ID3D11Device,
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    fps: u32,
    out_w: u32,
    out_h: u32,
    in_size: (u32, u32),
    vp: Option<(ID3D11VideoProcessorEnumerator, ID3D11VideoProcessor)>,
    outputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
    /// Однотонная BGRA-текстура для заглушки (создаётся по требованию).
    solid: Option<ID3D11Texture2D>,
}

impl Converter {
    fn new(device: &ID3D11Device, ctx: &ID3D11DeviceContext, out_w: u32, out_h: u32, fps: u32) -> R<Converter> {
        let vdev: ID3D11VideoDevice = device.cast().map_err(e("ID3D11VideoDevice"))?;
        let vctx: ID3D11VideoContext = ctx.cast().map_err(e("ID3D11VideoContext"))?;
        let mut c = Converter {
            device: device.clone(),
            vdev,
            vctx,
            fps,
            out_w,
            out_h,
            in_size: (0, 0),
            vp: None,
            outputs: Vec::new(),
            next: 0,
            solid: None,
        };
        // Процессор под выходной размер (вход уточним по первому кадру) — нужен, чтобы создать output views.
        c.make_vp(out_w, out_h)?;
        for _ in 0..RING {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: out_w,
                Height: out_h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_VIDEO_ENCODER.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut tex = None;
            unsafe { c.device.CreateTexture2D(&desc, None, Some(&mut tex)) }.map_err(e("NV12-текстура"))?;
            let tex = tex.ok_or("NV12-текстура не создана")?;
            let view = c.output_view(&tex)?;
            c.outputs.push((tex, view));
        }
        Ok(c)
    }

    fn make_vp(&mut self, in_w: u32, in_h: u32) -> R<()> {
        let rate = DXGI_RATIONAL { Numerator: self.fps, Denominator: 1 };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: in_w,
            InputHeight: in_h,
            OutputFrameRate: rate,
            OutputWidth: self.out_w,
            OutputHeight: self.out_h,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        let en = unsafe { self.vdev.CreateVideoProcessorEnumerator(&desc) }.map_err(e("VideoProcessorEnumerator"))?;
        let vp = unsafe { self.vdev.CreateVideoProcessor(&en, 0) }.map_err(e("VideoProcessor"))?;
        unsafe {
            // Вход — RGB полного диапазона, выход — BT.601 16–235 (как libyuv в программном пути).
            let input = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
            let output = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0x10 };
            self.vctx.VideoProcessorSetStreamColorSpace(&vp, 0, &input);
            self.vctx.VideoProcessorSetOutputColorSpace(&vp, &output);
            let bg = D3D11_VIDEO_COLOR { Anonymous: D3D11_VIDEO_COLOR_0 { RGBA: D3D11_VIDEO_COLOR_RGBA { R: 0.086, G: 0.071, B: 0.059, A: 1.0 } } };
            self.vctx.VideoProcessorSetOutputBackgroundColor(&vp, false, &bg);
            self.vctx.VideoProcessorSetStreamFrameFormat(&vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            self.vctx.VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);
        }
        self.vp = Some((en, vp));
        self.in_size = (in_w, in_h);
        Ok(())
    }

    fn output_view(&self, tex: &ID3D11Texture2D) -> R<ID3D11VideoProcessorOutputView> {
        let (en, _) = self.vp.as_ref().ok_or("нет процессора")?;
        let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
        };
        let mut view = None;
        unsafe { self.vdev.CreateVideoProcessorOutputView(tex, en, &desc, Some(&mut view)) }.map_err(e("OutputView"))?;
        view.ok_or_else(|| "OutputView не создан".into())
    }

    /// Кадр WGC (`src`, видимая часть `w`×`h`) → следующая NV12-текстура кольца.
    fn convert(&mut self, src: &ID3D11Texture2D, w: u32, h: u32) -> R<ID3D11Texture2D> {
        if self.in_size != (w, h) {
            self.make_vp(w, h)?;
            // Output views привязаны к enumerator'у — пересоздаём.
            for i in 0..self.outputs.len() {
                let tex = self.outputs[i].0.clone();
                self.outputs[i].1 = self.output_view(&tex)?;
            }
        }
        let (en, vp) = self.vp.as_ref().ok_or("нет процессора")?;
        let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
        };
        let mut in_view = None;
        unsafe { self.vdev.CreateVideoProcessorInputView(src, en, &in_desc, Some(&mut in_view)) }.map_err(e("InputView"))?;
        let (dw, dh) = crate::sources::fit(w, h, self.out_w, self.out_h);
        let (dw, dh) = (dw.min(self.out_w), dh.min(self.out_h));
        let dx = (self.out_w - dw) / 2;
        let dy = (self.out_h - dh) / 2;
        let src_rect = RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 };
        let dst_rect = RECT { left: dx as i32, top: dy as i32, right: (dx + dw) as i32, bottom: (dy + dh) as i32 };
        let (tex, out_view) = self.outputs[self.next].clone();
        self.next = (self.next + 1) % self.outputs.len();
        unsafe {
            self.vctx.VideoProcessorSetStreamSourceRect(vp, 0, true, Some(&src_rect));
            self.vctx.VideoProcessorSetStreamDestRect(vp, 0, true, Some(&dst_rect));
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(in_view),
                ..Default::default()
            };
            let r = self.vctx.VideoProcessorBlt(vp, &out_view, 0, std::slice::from_ref(&stream));
            ManuallyDrop::drop(&mut stream.pInputSurface);
            r.map_err(e("VideoProcessorBlt"))?;
        }
        Ok(tex)
    }

    /// Заглушка (тёмный кадр) — окно свёрнуто ещё до первого кадра. Через тот же видеопроцессор, что и живые кадры:
    /// ClearView по NV12-виду красит каналы как есть, и зритель видел ярко-зелёный кадр.
    fn placeholder(&mut self) -> R<ID3D11Texture2D> {
        const S: u32 = 64;
        if self.solid.is_none() {
            let px = vec![0xFF_16_12_0Fu32; (S * S) as usize]; // BGRA тёмный фон
            let desc = D3D11_TEXTURE2D_DESC {
                Width: S,
                Height: S,
                MipLevels: 1,
                ArraySize: 1,
                Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let init = D3D11_SUBRESOURCE_DATA { pSysMem: px.as_ptr().cast(), SysMemPitch: S * 4, SysMemSlicePitch: 0 };
            let mut tex = None;
            unsafe { self.device.CreateTexture2D(&desc, Some(&init), Some(&mut tex)) }.map_err(e("текстура заглушки"))?;
            self.solid = tex;
        }
        let solid = self.solid.clone().ok_or("нет текстуры заглушки")?;
        self.convert(&solid, S, S)
    }
}

// ------------------------------------------------------------------ Media Foundation

fn variant_u32(v: u32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_UI4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { ulVal: v },
            }),
        },
    }
}

fn variant_bool(v: bool) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_BOOL,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { boolVal: windows::Win32::Foundation::VARIANT_BOOL(if v { -1 } else { 0 }) },
            }),
        },
    }
}

fn mft_name(a: &IMFActivate) -> String {
    let mut p = PWSTR::null();
    let mut len = 0u32;
    unsafe {
        if a.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut p, &mut len).is_ok() {
            let s = p.to_string().unwrap_or_default();
            CoTaskMemFree(Some(p.0 as *const _));
            return s;
        }
    }
    "H.264 MFT".into()
}

/// Аппаратные H.264-энкодеры (NV12 → H.264); сначала — на том же адаптере, что и устройство.
fn find_encoders(luid: Option<u64>) -> Vec<(IMFActivate, String)> {
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
    let mut ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut n = 0u32;
    let flags = MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0);
    if unsafe { MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, flags, Some(&input), Some(&output), &mut ptr, &mut n) }.is_err()
        || ptr.is_null()
    {
        return Vec::new();
    }
    let mut all: Vec<(IMFActivate, String, bool)> = Vec::new();
    unsafe {
        for slot in std::slice::from_raw_parts_mut(ptr, n as usize) {
            if let Some(a) = slot.take() {
                let same = match luid {
                    Some(l) => a.GetUINT64(&MFT_ENUM_ADAPTER_LUID).map(|v| v == l).unwrap_or(false),
                    None => false,
                };
                let name = mft_name(&a);
                all.push((a, name, same));
            }
        }
        CoTaskMemFree(Some(ptr as *const _));
    }
    all.sort_by_key(|(_, _, same)| !*same);
    all.into_iter().map(|(a, n, _)| (a, n)).collect()
}

struct EncState {
    pending: Option<(Sx<ID3D11Texture2D>, i64)>,
    need_input: u32,
}

struct MfEncoder {
    mft: Sx<IMFTransform>,
    codec: Option<Sx<ICodecAPI>>,
    events: Sx<IMFMediaEventGenerator>,
    _manager: Sx<IMFDXGIDeviceManager>,
    state: Mutex<EncState>,
    force_key: AtomicBool,
    /// Когда попросили ключевой кадр (для диагностики: через сколько он вышел и какого размера).
    key_asked: Mutex<Option<Instant>>,
    stop: AtomicBool,
    /// Текущий битрейт (бит/с).
    bitrate: AtomicU32,
    provides_samples: bool,
    out_size: u32,
    seq_header: Vec<u8>,
    width: u32,
    height: u32,
    fps: u32,
    max_bitrate: u32,
    name: String,
    /// Диагностика: что вышло из энкодера (кадры, ключевые, байты) с прошлой строки лога.
    out_frames: AtomicU32,
    out_keys: AtomicU32,
    out_bytes: AtomicU64,
    /// Сумма QP и число кадров, у которых энкодер сообщил QP (`MFSampleExtension_VideoEncodeQP`).
    out_qp_sum: AtomicU64,
    out_qp_n: AtomicU32,
    /// Разбор QP из потока (SPS/PPS между кадрами).
    qp_parser: Mutex<crate::h264qp::QpParser>,
}

impl MfEncoder {
    fn new(device: &ID3D11Device, w: u32, h: u32, fps: u32, bitrate: u32) -> R<MfEncoder> {
        unsafe { MFStartup(MF_VERSION, 0) }.map_err(e("MFStartup"))?;
        let list = find_encoders(adapter_luid(device));
        if list.is_empty() {
            return Err("нет аппаратного H.264-энкодера".into());
        }
        let mut last_err = String::new();
        for (act, name) in list {
            match Self::open(device, &act, &name, w, h, fps, bitrate) {
                Ok(enc) => return Ok(enc),
                Err(err) => {
                    log::warn!("энкодер «{name}»: {err}");
                    let _ = unsafe { act.ShutdownObject() };
                    last_err = err;
                }
            }
        }
        Err(last_err)
    }

    #[allow(clippy::too_many_arguments)]
    fn open(device: &ID3D11Device, act: &IMFActivate, name: &str, w: u32, h: u32, fps: u32, bitrate: u32) -> R<MfEncoder> {
        // Стартуем скромно: WebRTC сам поднимет битрейт запросами, когда оценит канал.
        let start_bitrate = START_BITRATE.min(bitrate);
        unsafe {
            let mft: IMFTransform = act.ActivateObject().map_err(e("ActivateObject"))?;
            let attrs = mft.GetAttributes().map_err(e("GetAttributes"))?;
            let is_async = attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0;
            if !is_async {
                return Err("энкодер синхронный (ожидался аппаратный асинхронный MFT)".into());
            }
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).map_err(e("ASYNC_UNLOCK"))?;
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);

            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager).map_err(e("DXGIDeviceManager"))?;
            let manager = manager.ok_or("DXGIDeviceManager не создан")?;
            manager.ResetDevice(device, token).map_err(e("ResetDevice"))?;
            mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize).map_err(e("SET_D3D_MANAGER"))?;

            let codec: Option<ICodecAPI> = mft.cast().ok();
            if let Some(c) = &codec {
                let set = |g: &GUID, v: VARIANT, what: &str| {
                    if let Err(err) = c.SetValue(g, &v) {
                        log::debug!("{name}: {what} не поддерживается ({err})");
                    }
                };
                set(&CODECAPI_AVLowLatencyMode, variant_bool(true), "LowLatencyMode");
                // Опыты (AMADEUS_ENC_RC=cbr|pcvbr|uvbr|ldvbr, AMADEUS_ENC_MAXQP=N): режим контроля битрейта и потолок QP.
                // CBR: только он следует цели WebRTC на ходу. VBR (LowDelay/PeakConstrained/Unconstrained) у NVIDIA MFT смену
                // MeanBitRate после старта не применяет: при цели 12/4/2 Мбит/с одинаково ~1 Мбит/с и QP 46–50 в динамичных
                // сценах (замер rcprobe 2026-11-21). Для опытов: AMADEUS_ENC_RC=ldvbr|pcvbr|uvbr, AMADEUS_ENC_MAXQP=N.
                let rc = match std::env::var("AMADEUS_ENC_RC").as_deref() {
                    Ok("ldvbr") => eAVEncCommonRateControlMode_LowDelayVBR,
                    Ok("pcvbr") => eAVEncCommonRateControlMode_PeakConstrainedVBR,
                    Ok("uvbr") => eAVEncCommonRateControlMode_UnconstrainedVBR,
                    _ => eAVEncCommonRateControlMode_CBR,
                };
                set(&CODECAPI_AVEncCommonRateControlMode, variant_u32(rc.0 as u32), "RateControlMode");
                if let Some(q) = std::env::var("AMADEUS_ENC_MAXQP").ok().and_then(|v| v.parse::<u32>().ok()) {
                    log::info!("{name}: MaxQP из AMADEUS_ENC_MAXQP: {q}");
                    set(&CODECAPI_AVEncVideoMaxQP, variant_u32(q), "MaxQP");
                }
                set(&CODECAPI_AVEncCommonMaxBitRate, variant_u32(start_bitrate.saturating_mul(3) / 2), "MaxBitRate");
                set(&CODECAPI_AVEncCommonMeanBitRate, variant_u32(start_bitrate), "MeanBitRate");
                set(&CODECAPI_AVEncCommonBufferSize, variant_u32(start_bitrate / 2), "BufferSize");
                set(&CODECAPI_AVEncMPVGOPSize, variant_u32(fps * 10), "GOPSize");
                set(&CODECAPI_AVEncMPVDefaultBPictureCount, variant_u32(0), "BPictureCount");
                if crate::diag::on() {
                    let sup = |g: &GUID| if c.IsSupported(g).is_ok() { "да" } else { "нет" };
                    log::info!(
                        "ДИАГ энкодер {name}: поддерживает MaxQP {}, MinQP {}, QualityVsSpeed {}",
                        sup(&CODECAPI_AVEncVideoMaxQP),
                        sup(&CODECAPI_AVEncVideoMinQP),
                        sup(&CODECAPI_AVEncCommonQualityVsSpeed)
                    );
                }
            }

            // Тип выхода (до входа — так требуют энкодеры).
            let mut set_out = None;
            for profile in [eAVEncH264VProfile_ConstrainedBase.0 as u32, 66, 77] {
                let out = MFCreateMediaType().map_err(e("MediaType"))?;
                out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(e("out major"))?;
                out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264).map_err(e("out subtype"))?;
                out.SetUINT32(&MF_MT_AVG_BITRATE, start_bitrate).map_err(e("bitrate"))?;
                out.SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | h as u64).map_err(e("size"))?;
                out.SetUINT64(&MF_MT_FRAME_RATE, ((fps as u64) << 32) | 1).map_err(e("fps"))?;
                out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1).map_err(e("par"))?;
                out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).map_err(e("interlace"))?;
                out.SetUINT32(&MF_MT_MPEG2_PROFILE, profile).map_err(e("profile"))?;
                match mft.SetOutputType(0, &out, 0) {
                    Ok(()) => {
                        set_out = Some(profile);
                        break;
                    }
                    Err(err) => log::debug!("{name}: профиль {profile}: {err}"),
                }
            }
            let profile = set_out.ok_or("энкодер не принял тип H.264")?;

            let inp = MFCreateMediaType().map_err(e("MediaType"))?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(e("in major"))?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).map_err(e("in subtype"))?;
            inp.SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | h as u64).map_err(e("size"))?;
            inp.SetUINT64(&MF_MT_FRAME_RATE, ((fps as u64) << 32) | 1).map_err(e("fps"))?;
            inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).map_err(e("interlace"))?;
            mft.SetInputType(0, &inp, 0).map_err(e("SetInputType NV12"))?;

            let info = mft.GetOutputStreamInfo(0).map_err(e("OutputStreamInfo"))?;
            let provides_samples = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
            let seq_header = mft
                .GetOutputCurrentType(0)
                .ok()
                .and_then(|t| {
                    let n = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER).ok()?;
                    let mut buf = vec![0u8; n as usize];
                    t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buf, None).ok()?;
                    Some(buf)
                })
                .unwrap_or_default();

            let events: IMFMediaEventGenerator = mft.cast().map_err(e("IMFMediaEventGenerator"))?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).map_err(e("BEGIN_STREAMING"))?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).map_err(e("START_OF_STREAM"))?;
            log::info!(
                "аппаратный энкодер «{name}»: {w}×{h} {fps} к/с, старт {} кбит/с (потолок {}), профиль {profile}",
                start_bitrate / 1000,
                bitrate / 1000
            );
            Ok(MfEncoder {
                mft: Sx(mft),
                codec: codec.map(Sx),
                events: Sx(events),
                _manager: Sx(manager),
                state: Mutex::new(EncState { pending: None, need_input: 0 }),
                force_key: AtomicBool::new(true),
                key_asked: Mutex::new(None),
                stop: AtomicBool::new(false),
                bitrate: AtomicU32::new(start_bitrate),
                provides_samples,
                out_size: info.cbSize.max(w * h),
                seq_header,
                width: w,
                height: h,
                fps,
                max_bitrate: bitrate,
                name: name.to_string(),
                out_frames: AtomicU32::new(0),
                out_keys: AtomicU32::new(0),
                out_bytes: AtomicU64::new(0),
                out_qp_sum: AtomicU64::new(0),
                out_qp_n: AtomicU32::new(0),
                qp_parser: Mutex::new(Default::default()),
            })
        }
    }

    /// Новый кадр (NV12-текстура). Если энкодер ждёт вход — сразу отдаём, иначе кадр ждёт (заменяя старый).
    fn push(&self, tex: ID3D11Texture2D, ts_us: i64, sink: &dyn EncodedSink) {
        let mut st = self.state.lock().unwrap();
        st.pending = Some((Sx(tex), ts_us));
        self.try_feed(&mut st, sink);
    }

    fn try_feed(&self, st: &mut EncState, sink: &dyn EncodedSink) {
        if st.need_input == 0 || self.stop.load(Ordering::Relaxed) {
            return;
        }
        let Some((tex, ts)) = st.pending.take() else { return };
        if let Some(bps) = sink.take_bitrate_request() {
            self.set_bitrate(bps);
        }
        if sink.take_keyframe_request() {
            self.force_key.store(true, Ordering::Relaxed);
        }
        let r = unsafe {
            (|| -> windows::core::Result<()> {
                let buf = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &tex.0, 0, false)?;
                let sample = MFCreateSample()?;
                sample.AddBuffer(&buf)?;
                sample.SetSampleTime(ts * 10)?;
                sample.SetSampleDuration(10_000_000 / self.fps as i64)?;
                if self.force_key.swap(false, Ordering::Relaxed)
                    && let Some(c) = &self.codec {
                        let _ = c.0.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &variant_u32(1));
                        self.key_asked.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Instant::now);
                    }
                self.mft.0.ProcessInput(0, &sample, 0)
            })()
        };
        st.need_input -= 1;
        if let Err(err) = r {
            log::warn!("энкодер: ProcessInput: {err}");
        }
    }

    /// Битрейт от контроля перегрузки WebRTC: средний = цель, пик и буфер VBV — от неё же,
    /// чтобы ни всплески, ни крупные ключевые кадры не переполняли канал.
    fn set_bitrate(&self, bps: u32) {
        let bps = bps.clamp(MIN_BITRATE, self.max_bitrate);
        let prev = self.bitrate.swap(bps, Ordering::Relaxed);
        if prev == bps {
            return;
        }
        if let Some(c) = &self.codec {
            unsafe {
                let mean = c.0.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &variant_u32(bps));
                let _ = c.0.SetValue(&CODECAPI_AVEncCommonMaxBitRate, &variant_u32(bps.saturating_mul(3) / 2));
                let _ = c.0.SetValue(&CODECAPI_AVEncCommonBufferSize, &variant_u32(bps / 2));
                if let Err(err) = mean {
                    log::warn!("энкодер: смена битрейта: {err}");
                }
            }
        }
        if (prev as i64 - bps as i64).unsigned_abs() > prev as u64 / 5 {
            log::info!("энкодер: битрейт {} → {} кбит/с", prev / 1000, bps / 1000);
        }
    }

    fn process_output(&self, sink: &dyn EncodedSink) -> R<()> {
        unsafe {
            let mut buf = MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, ..Default::default() };
            if !self.provides_samples {
                let s = MFCreateSample().map_err(e("sample"))?;
                let b = MFCreateMemoryBuffer(self.out_size).map_err(e("buffer"))?;
                s.AddBuffer(&b).map_err(e("AddBuffer"))?;
                buf.pSample = ManuallyDrop::new(Some(s));
            }
            let mut status = 0u32;
            let r = self.mft.0.ProcessOutput(0, std::slice::from_mut(&mut buf), &mut status);
            let sample = ManuallyDrop::take(&mut buf.pSample);
            let _events = ManuallyDrop::take(&mut buf.pEvents);
            if let Err(err) = r {
                let code = err.code().0;
                if code == MF_E_TRANSFORM_STREAM_CHANGE {
                    // Энкодер сменил формат выхода — принимаем его вариант.
                    if let Ok(t) = self.mft.0.GetOutputAvailableType(0, 0) {
                        let _ = self.mft.0.SetOutputType(0, &t, 0);
                    }
                    return Ok(());
                }
                if code == MF_E_TRANSFORM_NEED_MORE_INPUT {
                    return Ok(());
                }
                return Err(format!("ProcessOutput: {err}"));
            }
            let Some(sample) = sample else { return Ok(()) };
            let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            // QP кадра — мерило качества при недоборе битрейта: из атрибута образца, а если энкодер его не даёт (NVENC) —
            // из заголовка среза (только в диагностике: разбор стоит копейки, но незачем).
            let attr_qp = sample.GetUINT64(&MFSampleExtension_VideoEncodeQP).ok().map(|q| (q & 0xFFFF) as i32);
            let ts = sample.GetSampleTime().unwrap_or(0) / 10;
            let mb = sample.ConvertToContiguousBuffer().map_err(e("ContiguousBuffer"))?;
            let mut p: *mut u8 = std::ptr::null_mut();
            let mut len = 0u32;
            mb.Lock(&mut p, None, Some(&mut len)).map_err(e("Lock"))?;
            let data = std::slice::from_raw_parts(p, len as usize);
            let qp = attr_qp.or_else(|| {
                if !crate::diag::on() {
                    return None;
                }
                let mut parser = self.qp_parser.lock().unwrap_or_else(|e| e.into_inner());
                // энкодер может не повторять SPS/PPS в кадрах — берём их из заголовка последовательности
                if !has_nal(data, 7) {
                    let _ = parser.frame_qp(&self.seq_header);
                }
                parser.frame_qp(data)
            });
            if let Some(q) = qp {
                self.out_qp_sum.fetch_add(q.max(0) as u64, Ordering::Relaxed);
                self.out_qp_n.fetch_add(1, Ordering::Relaxed);
            }
            self.out_frames.fetch_add(1, Ordering::Relaxed);
            self.out_bytes.fetch_add(len as u64, Ordering::Relaxed);
            if key {
                self.out_keys.fetch_add(1, Ordering::Relaxed);
                let asked = self.key_asked.lock().unwrap_or_else(|e| e.into_inner()).take();
                if crate::diag::on() {
                    log::info!(
                        "ДИАГ ключевой кадр «{}»: {:.0} КБ ({:.2} с при цели {} кбит/с){}",
                        self.name,
                        len as f64 / 1024.0,
                        len as f64 * 8.0 / self.bitrate.load(Ordering::Relaxed).max(1) as f64,
                        self.bitrate.load(Ordering::Relaxed) / 1000,
                        asked.map(|t| format!(", через {} мс после запроса", t.elapsed().as_millis())).unwrap_or_default()
                    );
                }
            }
            if key && !has_nal(data, 7) && !self.seq_header.is_empty() {
                let mut v = Vec::with_capacity(self.seq_header.len() + data.len());
                v.extend_from_slice(&self.seq_header);
                v.extend_from_slice(data);
                sink.encoded(&v, key, ts, self.width, self.height);
            } else {
                sink.encoded(data, key, ts, self.width, self.height);
            }
            let _ = mb.Unlock();
        }
        Ok(())
    }

    /// Цикл событий асинхронного MFT (отдельный поток).
    fn run_events(self: Arc<Self>, sink: Arc<dyn EncodedSink>) {
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let mut outputs = 0u64;
        loop {
            let ev = match unsafe { self.events.0.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0)) } {
                Ok(ev) => ev,
                Err(err) => {
                    if !self.stop.load(Ordering::Relaxed) {
                        log::warn!("энкодер: GetEvent: {err}");
                        sink.event(Event::Error(format!("аппаратный энкодер: {err}")));
                    }
                    break;
                }
            };
            let ty = unsafe { ev.GetType() }.unwrap_or(0) as i32;
            if ty == METransformNeedInput.0 {
                let mut st = self.state.lock().unwrap();
                st.need_input += 1;
                self.try_feed(&mut st, sink.as_ref());
            } else if ty == METransformHaveOutput.0 {
                if let Err(err) = self.process_output(sink.as_ref()) {
                    log::warn!("энкодер: {err}");
                }
                outputs += 1;
                if outputs == 1 {
                    sink.event(Event::Started { width: self.width, height: self.height });
                }
            } else if ty == METransformDrainComplete.0 {
                break;
            }
            if self.stop.load(Ordering::Relaxed) && ty != METransformHaveOutput.0 {
                break;
            }
        }
        log::debug!("энкодер: поток событий завершён");
    }

    fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        unsafe {
            let _ = self.mft.0.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.0.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
        }
    }
}

/// Есть ли в Annex B потоке NAL указанного типа (7 = SPS, 5 = IDR).
pub fn has_nal(data: &[u8], ty: u8) -> bool {
    let mut i = 0;
    while i + 3 < data.len() {
        let sc3 = data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1;
        if sc3 {
            if data[i + 3] & 0x1F == ty {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

// ------------------------------------------------------------------ захват

/// Желаемый размер энкодера: (размер, с какого момента, уже сообщили).
type WantSize = ((u32, u32), Instant, bool);

/// Второй, «экономный» слой трансляции (свой simulcast): тот же захват, меньше размер и частота, свой энкодер и приёмник.
pub struct LowLayer {
    pub sink: Arc<dyn EncodedSink>,
    pub cfg: CaptureConfig,
    pub bitrate: u32,
}

struct LowEnc {
    enc: Arc<MfEncoder>,
    conv: Mutex<Sx<Converter>>,
    sink: Arc<dyn EncodedSink>,
    /// Кодировать каждый N-й кадр (60 → 30 к/с).
    every: u32,
    n: AtomicU32,
    last: Mutex<Option<Sx<ID3D11Texture2D>>>,
    events: Mutex<Option<JoinHandle<()>>>,
}

struct Shared {
    stop: AtomicBool,
    closed: AtomicBool,
    conv: Mutex<Sx<Converter>>,
    enc: Arc<MfEncoder>,
    sink: Arc<dyn EncodedSink>,
    /// Последняя отправленная NV12-текстура и когда пришёл последний живой кадр.
    last: Mutex<(Option<Sx<ID3D11Texture2D>>, Instant)>,
    minimized: AtomicBool,
    min_interval: Duration,
    frames: AtomicU32,
    /// Диагностика: живые кадры и повторы с прошлой строки лога.
    diag_frames: AtomicU32,
    repeats: AtomicU32,
    /// Размер энкодера и желаемый размер по живым кадрам: (размер, с какого момента, уже сообщили).
    enc_size: (u32, u32),
    want_size: Mutex<Option<WantSize>>,
    low: Option<LowEnc>,
}

pub struct HwCapture {
    source: SourceId,
    shared: Arc<Shared>,
    pool: Sx<Direct3D11CaptureFramePool>,
    session: Sx<GraphicsCaptureSession>,
    _item: Sx<GraphicsCaptureItem>,
    pump: Option<JoinHandle<()>>,
    events: Option<JoinHandle<()>>,
    pub encoder_name: String,
    pub size: (u32, u32),
}

fn capture_item(source: SourceId) -> R<GraphicsCaptureItem> {
    let item: GraphicsCaptureItemType = match source {
        SourceId::Window(h) => windows_capture::window::Window::from_raw_hwnd(h as *mut std::ffi::c_void)
            .try_into()
            .map_err(e("окно для захвата"))?,
        SourceId::Monitor(h) => windows_capture::monitor::Monitor::from_raw_hmonitor(h as *mut std::ffi::c_void)
            .try_into()
            .map_err(e("экран для захвата"))?,
    };
    Ok(match item {
        GraphicsCaptureItemType::Window((i, _)) | GraphicsCaptureItemType::Monitor((i, _)) => i,
        GraphicsCaptureItemType::Unknown((i, _)) => i,
    })
}

impl HwCapture {
    /// Запуск аппаратного пути. Ошибка — значит, аппаратного кодирования нет (используйте программный путь).
    pub fn start(source: SourceId, cfg: CaptureConfig, bitrate: u32, sink: Arc<dyn EncodedSink>) -> R<HwCapture> {
        Self::start_with_low(source, cfg, bitrate, sink, None)
    }

    /// То же, плюс экономный слой. Не поднялся второй энкодер — трансляция идёт без него.
    pub fn start_with_low(source: SourceId, cfg: CaptureConfig, bitrate: u32, sink: Arc<dyn EncodedSink>, low: Option<LowLayer>) -> R<HwCapture> {
        Self::start_sized(source, cfg, bitrate, sink, low, None)
    }

    /// То же, но размер кадров известен заранее (`content` — из события SizeChanged): энкодер создаётся под него.
    /// Без этого пересоздание брало `GraphicsCaptureItem::Size`, а он у некоторых окон (Яндекс Браузер) стабильно отличается
    /// от размера кадров (1920×1054 против 1920×1050) — энкодер пересоздавался по кругу раз в секунду, пока захват не завис.
    pub fn start_sized(
        source: SourceId,
        cfg: CaptureConfig,
        bitrate: u32,
        sink: Arc<dyn EncodedSink>,
        low: Option<LowLayer>,
        content: Option<(u32, u32)>,
    ) -> R<HwCapture> {
        if !GraphicsCaptureApi::is_supported().unwrap_or(false) {
            return Err("Захват экрана не поддерживается этой версией Windows".into());
        }
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let (device, ctx) = create_device()?;
        let item = capture_item(source)?;
        let size = item.Size().map_err(e("размер источника"))?;
        let (src_w, src_h) = content.unwrap_or((size.Width.max(2) as u32, size.Height.max(2) as u32));
        let (w, h) = target_size(src_w.max(2), src_h.max(2), &cfg);
        let fps = cfg.fps.clamp(1, 120);
        let enc = Arc::new(MfEncoder::new(&device, w, h, fps, bitrate)?);
        let conv = Converter::new(&device, &ctx, w, h, fps)?;
        let low = low.and_then(|l| {
            let (lw, lh) = target_size(src_w.max(2), src_h.max(2), &l.cfg);
            let lfps = l.cfg.fps.clamp(1, fps);
            if (lw, lh) == (w, h) && lfps == fps {
                return None; // экономному слою нечего экономить
            }
            let made = MfEncoder::new(&device, lw, lh, lfps, l.bitrate).and_then(|enc| Ok((enc, Converter::new(&device, &ctx, lw, lh, lfps)?)));
            match made {
                Ok((enc, conv)) => {
                    log::info!("экономный слой: {lw}×{lh} {lfps} к/с, до {} кбит/с", l.bitrate / 1000);
                    Some(LowEnc {
                        enc: Arc::new(enc),
                        conv: Mutex::new(Sx(conv)),
                        sink: l.sink,
                        every: (fps / lfps).max(1),
                        n: AtomicU32::new(0),
                        last: Mutex::new(None),
                        events: Mutex::new(None),
                    })
                }
                Err(err) => {
                    log::warn!("экономный слой не поднялся: {err} — только основной");
                    None
                }
            }
        });
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            conv: Mutex::new(Sx(conv)),
            enc: enc.clone(),
            sink: sink.clone(),
            last: Mutex::new((None, Instant::now())),
            minimized: AtomicBool::new(false),
            min_interval: Duration::from_micros(1_000_000 / fps as u64 * 85 / 100),
            frames: AtomicU32::new(0),
            diag_frames: AtomicU32::new(0),
            repeats: AtomicU32::new(0),
            enc_size: (w, h),
            want_size: Mutex::new(None),
            low,
        });
        let events = {
            let (enc, sink) = (enc.clone(), sink.clone());
            std::thread::Builder::new().name("mft-events".into()).spawn(move || enc.run_events(sink)).map_err(e("поток"))?
        };
        if let Some(l) = &shared.low {
            let (enc, sink) = (l.enc.clone(), l.sink.clone());
            let h = std::thread::Builder::new().name("mft-events-low".into()).spawn(move || enc.run_events(sink)).map_err(e("поток"))?;
            *l.events.lock().unwrap() = Some(h);
        }

        let d3d = windows_capture::d3d11::create_direct3d_device(&device).map_err(e("IDirect3DDevice"))?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&d3d, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, size)
            .map_err(e("FramePool"))?;
        let session = pool.CreateCaptureSession(&item).map_err(e("CaptureSession"))?;
        if GraphicsCaptureApi::is_cursor_settings_supported().unwrap_or(false) {
            let _ = session.SetIsCursorCaptureEnabled(true);
        }
        if GraphicsCaptureApi::is_border_settings_supported().unwrap_or(false) {
            let _ = session.SetIsBorderRequired(false);
        }
        if GraphicsCaptureApi::is_minimum_update_interval_supported().unwrap_or(false) {
            let _ = session.SetMinUpdateInterval(TimeSpan { Duration: 10_000_000 / fps as i64 * 9 / 10 });
        }
        {
            let shared = shared.clone();
            item.Closed(&TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
                shared.closed.store(true, Ordering::Relaxed);
                Ok(())
            }))
            .map_err(e("Closed"))?;
        }
        {
            let shared = shared.clone();
            let d3d = Sx(d3d.clone());
            let last_size = Mutex::new((size.Width, size.Height));
            pool.FrameArrived(&TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |pool, _| {
                let Some(pool) = pool.as_ref() else { return Ok(()) };
                let frame = pool.TryGetNextFrame()?;
                if shared.stop.load(Ordering::Relaxed) {
                    return Ok(());
                }
                let cs = frame.ContentSize()?;
                // Свёрнутое окно: WGC шлёт пустые/крошечные кадры — не кодируем их (поток повторяет последний живой кадр).
                if cs.Width < 16 || cs.Height < 16 || (source.is_window() && source.is_minimized()) {
                    return Ok(());
                }
                {
                    let mut ls = last_size.lock().unwrap();
                    if (cs.Width, cs.Height) != *ls {
                        *ls = (cs.Width, cs.Height);
                        drop(frame);
                        let dev = &d3d; // захватить Sx целиком (Send), а не поле
                        pool.Recreate(&dev.0, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, cs)?;
                        return Ok(());
                    }
                }
                {
                    let last = shared.last.lock().unwrap();
                    if last.0.is_some() && last.1.elapsed() < shared.min_interval {
                        return Ok(());
                    }
                }
                let surface = frame.Surface()?;
                let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
                let tex: ID3D11Texture2D = unsafe { access.GetInterface()? };
                // После Recreate ещё приходят кадры из старого пула: текстура меньше ContentSize. Прямоугольник
                // источника больше текстуры — VideoProcessorBlt E_INVALIDARG, а следом падение в d3d11.dll.
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                unsafe { tex.GetDesc(&mut desc) };
                let w = (cs.Width as u32).min(desc.Width) & !1;
                let h = (cs.Height as u32).min(desc.Height) & !1;
                if w < 16 || h < 16 {
                    return Ok(());
                }
                {
                    // Окно изменило размер: запоминаем, насос сообщит, когда размер устоится (не на каждом шаге перетаскивания).
                    let t = target_size(w, h, &cfg);
                    let mut want = shared.want_size.lock().unwrap();
                    if t == shared.enc_size {
                        *want = None;
                    } else if want.as_ref().is_none_or(|(s, _, _)| *s != t) {
                        *want = Some((t, Instant::now(), false));
                    }
                }
                // Контекст устройства один на оба слоя — конвертируем под одним замком.
                let mut conv = shared.conv.lock().unwrap();
                let nv12 = match conv.0.convert(&tex, w, h) {
                    Ok(t) => t,
                    Err(err) => {
                        log::warn!("конвертация кадра: {err}");
                        return Ok(());
                    }
                };
                if let Some(l) = &shared.low
                    && l.n.fetch_add(1, Ordering::Relaxed) % l.every == 0
                {
                    match l.conv.lock().unwrap().0.convert(&tex, w, h) {
                        Ok(t) => {
                            *l.last.lock().unwrap() = Some(Sx(t.clone()));
                            l.enc.push(t, now_us(), l.sink.as_ref());
                        }
                        Err(err) => log::warn!("экономный слой: конвертация кадра: {err}"),
                    }
                }
                drop(conv);
                {
                    let mut last = shared.last.lock().unwrap();
                    let gap = last.1.elapsed();
                    if crate::diag::on() && last.0.is_some() && gap >= Duration::from_millis(400) {
                        log::info!("ДИАГ захват: живых кадров не было {} мс", gap.as_millis());
                    }
                    *last = (Some(Sx(nv12.clone())), Instant::now());
                }
                shared.frames.fetch_add(1, Ordering::Relaxed);
                shared.diag_frames.fetch_add(1, Ordering::Relaxed);
                shared.enc.push(nv12, now_us(), shared.sink.as_ref());
                Ok(())
            }))
            .map_err(e("FrameArrived"))?;
        }
        session.StartCapture().map_err(e("StartCapture"))?;

        let pump = {
            let shared = shared.clone();
            std::thread::Builder::new().name("hw-pump".into()).spawn(move || pump(shared, source)).map_err(e("поток"))?
        };
        log::info!("захват {source:?} (аппаратный путь): {w}×{h} {fps} к/с");
        Ok(HwCapture {
            source,
            shared,
            pool: Sx(pool),
            session: Sx(session),
            _item: Sx(item),
            pump: Some(pump),
            events: Some(events),
            encoder_name: enc.name.clone(),
            size: (w, h),
        })
    }

    pub fn source(&self) -> SourceId {
        self.source
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if self.shared.stop.swap(true, Ordering::Relaxed) {
            return;
        }
        let _ = self.session.0.Close();
        let _ = self.pool.0.Close();
        if let Some(p) = self.pump.take() {
            let _ = p.join();
        }
        self.shared.enc.shutdown();
        if let Some(l) = &self.shared.low {
            l.enc.shutdown();
            if let Some(ev) = l.events.lock().unwrap().take() {
                let t = Instant::now();
                while !ev.is_finished() && t.elapsed() < Duration::from_millis(800) {
                    std::thread::sleep(Duration::from_millis(10));
                }
                if ev.is_finished() {
                    let _ = ev.join();
                }
            }
        }
        if let Some(ev) = self.events.take() {
            // Поток событий выходит по DrainComplete; если драйвер его не прислал — не виснем.
            let t = Instant::now();
            while !ev.is_finished() && t.elapsed() < Duration::from_millis(800) {
                std::thread::sleep(Duration::from_millis(10));
            }
            if ev.is_finished() {
                let _ = ev.join();
            } else {
                log::warn!("энкодер не завершился вовремя — поток оставлен");
            }
        }
        log::info!("захват {:?} (аппаратный путь): стоп", self.source);
    }
}

impl Drop for HwCapture {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Повтор кадра при статичной картинке, заглушка для свёрнутого окна, закрытие, статистика.
fn pump(shared: Arc<Shared>, source: SourceId) {
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let mut closed_sent = false;
    let mut last_repeat = Instant::now();
    let mut stat_at = Instant::now();
    let mut min_check = Instant::now();
    let mut hidden = false;
    let mut occl_check = Instant::now();
    let mut diag_at = Instant::now();
    let mut screen = crate::diag::ScreenWatch::default();
    while !shared.stop.load(Ordering::Relaxed) {
        std::thread::sleep(POLL);
        if shared.closed.load(Ordering::Relaxed) || (source.is_window() && !source.is_alive()) {
            if !closed_sent {
                closed_sent = true;
                shared.sink.event(Event::Closed);
            }
            continue;
        }
        if source.is_window() && min_check.elapsed() >= Duration::from_millis(250) {
            min_check = Instant::now();
            let m = source.is_minimized();
            if m != shared.minimized.swap(m, Ordering::Relaxed) {
                shared.sink.event(Event::Minimized(m));
                last_repeat = Instant::now() - repeat_every();
            }
        }
        if let Some((size, since, sent)) = shared.want_size.lock().unwrap().as_mut()
            && !*sent
            && since.elapsed() >= RESIZE_SETTLE
        {
            *sent = true;
            log::info!("захват: размер {}×{} → {}×{} — пересоздать энкодер", shared.enc_size.0, shared.enc_size.1, size.0, size.1);
            shared.sink.event(Event::SizeChanged { width: size.0, height: size.1 });
        }
        let minimized = shared.minimized.load(Ordering::Relaxed);
        let idle = shared.last.lock().unwrap().1.elapsed();
        if source.is_window() && occl_check.elapsed() >= OCCLUSION_CHECK {
            occl_check = Instant::now();
            let h = !minimized && idle >= STALL && source.is_occluded();
            if h != hidden {
                hidden = h;
                log::info!("захват: окно {}", if h { "перекрыто — новых кадров нет" } else { "снова присылает кадры" });
                shared.sink.event(Event::Hidden(h));
            }
        }
        let key_wanted = shared.sink.take_keyframe_request();
        if key_wanted {
            shared.enc.force_key.store(true, Ordering::Relaxed);
        }
        let low_key = shared.low.as_ref().is_some_and(|l| l.sink.take_keyframe_request());
        if let Some(l) = shared.low.as_ref().filter(|_| low_key) {
            l.enc.force_key.store(true, Ordering::Relaxed);
        }
        if (idle >= repeat_every() || minimized) && (last_repeat.elapsed() >= repeat_every() || key_wanted) {
            last_repeat = Instant::now();
            // Свёрнутое окно: зритель видит последний живой кадр (заглушка — только если кадров ещё не было).
            let last = shared.last.lock().unwrap().0.as_ref().map(|t| t.0.clone());
            let tex = match last {
                Some(t) => Some(t),
                None if minimized => shared.conv.lock().unwrap().0.placeholder().ok(),
                None => None,
            };
            if let Some(t) = tex {
                shared.enc.push(t, now_us(), shared.sink.as_ref());
                shared.repeats.fetch_add(1, Ordering::Relaxed);
            }
            if let Some(l) = &shared.low
                && let Some(t) = l.last.lock().unwrap().as_ref().map(|t| t.0.clone())
            {
                l.enc.push(t, now_us(), l.sink.as_ref());
            }
        } else if low_key
            && let Some(l) = &shared.low
            && let Some(t) = l.last.lock().unwrap().as_ref().map(|t| t.0.clone())
        {
            // зритель экономного слоя попросил ключевой кадр, а картинка стоит — повторить сейчас
            l.enc.push(t, now_us(), l.sink.as_ref());
        }
        if crate::diag::on() && diag_at.elapsed() >= Duration::from_secs(1) {
            let dt = diag_at.elapsed().as_secs_f64();
            diag_at = Instant::now();
            let live = shared.diag_frames.swap(0, Ordering::Relaxed);
            let rep = shared.repeats.swap(0, Ordering::Relaxed);
            let out = shared.enc.out_frames.swap(0, Ordering::Relaxed);
            let keys = shared.enc.out_keys.swap(0, Ordering::Relaxed);
            let kbps = shared.enc.out_bytes.swap(0, Ordering::Relaxed) as f64 * 8.0 / 1000.0 / dt;
            let qp_n = shared.enc.out_qp_n.swap(0, Ordering::Relaxed);
            let qp_sum = shared.enc.out_qp_sum.swap(0, Ordering::Relaxed);
            let qp = if qp_n > 0 { format!("{:.1}", qp_sum as f64 / qp_n as f64) } else { "?".into() };
            let (occ, chg) = match source {
                SourceId::Window(h) => (if source.is_occluded() { "да" } else { "нет" }, screen.changed(h)),
                SourceId::Monitor(_) => ("—", "—"),
            };
            let low = shared.low.as_ref().map(|l| {
                let kb = l.enc.out_bytes.swap(0, Ordering::Relaxed) as f64 * 8.0 / 1000.0 / dt;
                format!(", экономный {} к/с {kb:.0} кбит/с (цель {})", l.enc.out_frames.swap(0, Ordering::Relaxed), l.enc.bitrate.load(Ordering::Relaxed) / 1000)
            }).unwrap_or_default();
            log::info!(
                "ДИАГ захват: живых {live}, повторов {rep}, из энкодера {out} (ключевых {keys}, {kbps:.0} кбит/с, цель {} кбит/с, QP {qp}), без кадров {} мс, свёрнуто {}, перекрыто {occ}, экран под окном меняется {chg}, в фокусе «{}»{low}",
                shared.enc.bitrate.load(Ordering::Relaxed) / 1000,
                idle.as_millis(),
                if minimized { "да" } else { "нет" },
                crate::diag::foreground_title()
            );
        }
        if stat_at.elapsed() >= Duration::from_secs(10) {
            let n = shared.frames.swap(0, Ordering::Relaxed);
            log::info!("захват (аппаратный путь): {:.1} к/с", n as f64 / stat_at.elapsed().as_secs_f64());
            stat_at = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::has_nal;

    #[test]
    fn finds_nal_types() {
        let idr = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 9];
        assert!(has_nal(&idr, 7));
        assert!(has_nal(&idr, 8));
        assert!(has_nal(&idr, 5));
        let p = [0, 0, 0, 1, 0x41, 1, 2, 3];
        assert!(!has_nal(&p, 7));
        assert!(has_nal(&p, 1));
    }
}
