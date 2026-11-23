//! Камера в голосе: кадры захвата → видеотрек LiveKit и локальное превью (плитка «я»).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use capture_win::{Event, I420Buffer, Sink};
use livekit::options::{TrackPublishOptions, VideoCodec, VideoEncoding};
use livekit::prelude::*;
use livekit::webrtc::video_frame::{VideoFrame, VideoRotation};
use livekit::webrtc::video_source::native::NativeVideoSource;
use livekit::webrtc::video_source::{RtcVideoSource, VideoResolution};
use tokio::sync::mpsc;

use crate::stream::CaptureEvent;
use crate::video::VideoHub;

/// Номер «поколения» событий камеры в общем канале событий захвата (события экрана ходят с номерами от 0).
pub const CAM_GEN: u64 = u64::MAX;
/// Имя трека камеры.
pub const CAM_TRACK: &str = "camera";
const MAX_BITRATE: u64 = 1_500_000;
const FPS: f64 = 30.0;
/// Превью себе — не чаще, чем раз в столько (экономит процессор).
const PREVIEW_EVERY_US: u64 = 66_000;

/// Ключ текстуры камеры участника в `VideoHub`.
pub fn hub_key(identity: &str) -> String {
    format!("cam:{identity}")
}

struct Ref<'a>(&'a I420Buffer);
impl AsRef<dyn livekit::webrtc::video_frame::VideoBuffer> for Ref<'_> {
    fn as_ref(&self) -> &(dyn livekit::webrtc::video_frame::VideoBuffer + 'static) {
        self.0
    }
}

/// Приёмник кадров камеры: в видеотрек и (редко) в превью.
pub struct CamSink {
    pub source: NativeVideoSource,
    pub events: mpsc::UnboundedSender<CaptureEvent>,
    pub hub: VideoHub,
    pub key: String,
    pub last_preview_us: AtomicU64,
}

impl Sink for CamSink {
    fn frame(&self, frame: &I420Buffer, timestamp_us: i64) {
        let f = VideoFrame { rotation: VideoRotation::VideoRotation0, timestamp_us, frame_metadata: None, buffer: Ref(frame) };
        self.source.capture_frame(&f);
        let now = timestamp_us as u64;
        if now.saturating_sub(self.last_preview_us.load(Ordering::Relaxed)) >= PREVIEW_EVERY_US {
            self.last_preview_us.store(now, Ordering::Relaxed);
            self.hub.put_i420(&self.key, frame);
        }
    }

    fn event(&self, event: Event) {
        let _ = self.events.send(CaptureEvent { generation: CAM_GEN, event });
    }
}

pub fn new_source() -> NativeVideoSource {
    NativeVideoSource::new(VideoResolution { width: capture_win::camera::MAX_W, height: capture_win::camera::MAX_H }, false)
}

pub async fn publish(room: &Room, source: &NativeVideoSource) -> Result<LocalTrackPublication, String> {
    let track = LocalVideoTrack::create_video_track(CAM_TRACK, RtcVideoSource::Native(source.clone()));
    let opts = TrackPublishOptions {
        source: TrackSource::Camera,
        video_codec: VideoCodec::VP8,
        simulcast: false,
        video_encoding: Some(VideoEncoding { max_bitrate: MAX_BITRATE, max_framerate: FPS }),
        ..Default::default()
    };
    match tokio::time::timeout(Duration::from_secs(10), room.local_participant().publish_track(LocalTrack::Video(track), opts)).await {
        Ok(Ok(p)) => Ok(p),
        Ok(Err(e)) => Err(format!("Камера не опубликована: {e}")),
        Err(_) => Err("Камера не опубликована: сервер не отвечает".into()),
    }
}

pub fn sink(source: &NativeVideoSource, events: &mpsc::UnboundedSender<CaptureEvent>, hub: &VideoHub, identity: &str) -> Arc<CamSink> {
    Arc::new(CamSink { source: source.clone(), events: events.clone(), hub: hub.clone(), key: hub_key(identity), last_preview_us: AtomicU64::new(0) })
}

#[cfg(test)]
mod tests {
    #[test]
    fn hub_key_is_prefixed() {
        assert_eq!(super::hub_key("7"), "cam:7");
    }
}
