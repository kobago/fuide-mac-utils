//! Playback engine: macOS AVFoundation (`AVPlayer`) through `objc2`, driven from the UI thread.
//!
//! Nothing here runs a callback: the app calls [`Engine::poll`] every frame, which reads the
//! player's state (status, time, buffering), pulls the newest video frame into an egui
//! texture, and picks up track / metadata information once AVFoundation has loaded it. That
//! keeps every AVFoundation object on the main thread (they are not `Send`) and avoids KVO.
//!
//! What plays is whatever macOS can decode: MP4 / M4V / MOV with H.264 / HEVC / ProRes, and
//! MP3 / AAC / ALAC / FLAC / WAV / AIFF audio — from local files or `http(s)` URLs (including
//! HLS `.m3u8`). MKV / WebM / VP9 / Vorbis are not.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, AnyThread, DefinedClass, MainThreadMarker};
use objc2_av_foundation::{
    AVAsset, AVAsynchronousKeyValueLoading, AVMediaCharacteristicContainsOnlyForcedSubtitles,
    AVMediaCharacteristicLegible, AVMediaSelectionGroup, AVMediaSelectionOption, AVMediaTypeAudio,
    AVMediaTypeVideo, AVMetadataCommonKeyAlbumName, AVMetadataCommonKeyArtist,
    AVMetadataCommonKeyTitle, AVMetadataItem, AVMutableAudioMix, AVMutableAudioMixInputParameters,
    AVPlayer, AVPlayerActionAtItemEnd, AVPlayerItem, AVPlayerItemLegibleOutput,
    AVPlayerItemLegibleOutputPushDelegate, AVPlayerItemOutputPushDelegate, AVPlayerItemStatus,
    AVPlayerItemVideoOutput, AVPlayerTimeControlStatus, AVURLAsset, NSValueAVFoundationExtensions,
};
use objc2_core_media::{
    kCMTimeZero, CMAudioFormatDescriptionGetStreamBasicDescription, CMFormatDescription, CMTime,
    CMVideoFormatDescriptionGetDimensions,
};
use objc2_core_video::{
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferPixelFormatTypeKey,
    kCVPixelFormatType_32BGRA, CVPixelBuffer, CVPixelBufferGetBaseAddress,
    CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetIOSurface,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{
    ns_string, NSArray, NSAttributedString, NSDictionary, NSLocale, NSNumber, NSObject,
    NSObjectProtocol, NSString, NSURL,
};

use objc2_core_foundation::CFRetained;
use objc2_metal::{
    MTLDevice, MTLPixelFormat, MTLStorageMode, MTLTextureDescriptor, MTLTextureType,
    MTLTextureUsage,
};

use crate::audiotap::{AudioShared, AudioTap};

/// Timescale for seeks (600 is AVFoundation's conventional "fits 24/25/30/60 fps" value).
const SEEK_TIMESCALE: i32 = 600;

// ---------------------------------------------------------------------------- source

/// Where a track comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    File(PathBuf),
    /// `http(s)://...` — anything AVFoundation streams (progressive MP4 / MP3, HLS).
    Url(String),
}

impl Source {
    /// Interpret what the user typed or dropped: an `http(s)` URL, a `file://` URL, or a path
    /// (`~` and relative to `cwd` allowed). Local paths must exist and not be directories.
    pub fn parse(input: &str, cwd: &Path) -> Result<Source, String> {
        let input = input.trim();
        if input.is_empty() {
            return Err("nothing to open".into());
        }
        let lower = input.to_ascii_lowercase();
        if lower.starts_with("http://") || lower.starts_with("https://") {
            if input[7..].trim_start_matches('/').is_empty() {
                return Err("URL has no host".into());
            }
            return Ok(Source::Url(input.to_string()));
        }
        if let Some(rest) = lower
            .starts_with("file://")
            .then(|| &input["file://".len()..])
        {
            return Source::parse(&percent_decode(rest), cwd);
        }
        if lower.contains("://") {
            return Err("only http(s) URLs and local paths".into());
        }
        let path = fuide::pathinput::expand(input, cwd);
        let meta = std::fs::metadata(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => "no such file".to_string(),
            _ => e.to_string(),
        })?;
        if meta.is_dir() {
            return Err("that is a directory".into());
        }
        Ok(Source::File(path))
    }

    /// Short name for lists: the file name, or the last URL segment (host if there is none).
    pub fn label(&self) -> String {
        match self {
            Source::File(p) => p
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string()),
            Source::Url(u) => {
                let no_query = u.split(['?', '#']).next().unwrap_or(u);
                let trimmed = no_query.trim_end_matches('/');
                let last = trimmed.rsplit('/').next().unwrap_or(trimmed);
                let host = trimmed
                    .split("://")
                    .nth(1)
                    .and_then(|r| r.split('/').next())
                    .unwrap_or(trimmed);
                let last = percent_decode(last);
                if last.is_empty() || last.contains("://") || last == host {
                    host.to_string()
                } else {
                    last
                }
            }
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, Source::Url(_))
    }

    /// The full location, for the inspector and the log.
    pub fn display(&self) -> String {
        match self {
            Source::File(p) => p.display().to_string(),
            Source::Url(u) => u.clone(),
        }
    }

    fn ns_url(&self) -> Option<Retained<NSURL>> {
        match self {
            Source::File(p) => Some(NSURL::fileURLWithPath_isDirectory(
                &NSString::from_str(&p.to_string_lossy()),
                false,
            )),
            Source::Url(u) => NSURL::URLWithString(&NSString::from_str(u)),
        }
    }
}

/// `%20` etc. -> characters (UTF-8; invalid sequences are kept as typed).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

// ---------------------------------------------------------------------------- info

#[derive(Clone, Debug, Default, PartialEq)]
pub struct VideoInfo {
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    /// Bits per second (AVFoundation's estimate; 0 when unknown).
    pub bitrate: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioInfo {
    pub codec: String,
    pub sample_rate: f64,
    pub channels: u32,
    pub bitrate: f32,
}

/// A chapter marker (QuickTime chapter track / HLS date-range).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Chapter {
    pub title: String,
    pub start: f64,
    pub end: f64,
}

/// What the tracks and the common metadata say. Filled in by [`Engine::poll`] once the item
/// is ready (tracks) and the asset's metadata has loaded (title & co.).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MediaInfo {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub video: Option<VideoInfo>,
    pub audio: Option<AudioInfo>,
    pub chapters: Vec<Chapter>,
    /// Tracks were read (`video` / `audio` are final).
    pub tracks_loaded: bool,
    /// Common metadata, chapters and subtitle options were read.
    pub metadata_loaded: bool,
}

impl MediaInfo {
    /// Index of the chapter containing `time` (the very end counts as the last chapter).
    pub fn chapter_at(&self, time: f64) -> Option<usize> {
        let last = self.chapters.last()?;
        self.chapters
            .iter()
            .position(|c| time >= c.start - 1e-3 && time < c.end)
            .or((time >= last.end - 1e-3).then(|| self.chapters.len() - 1))
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Status {
    /// No item.
    #[default]
    Idle,
    /// Item created, AVFoundation still opening it.
    Loading,
    Ready,
    Failed(String),
}

/// One frame's view of the player.
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub status: Status,
    pub time: f64,
    /// `None` until known, and for live streams.
    pub duration: Option<f64>,
    /// End of the buffered range that contains the playhead (seconds), if any.
    pub buffered: Option<f64>,
    /// The player is advancing (or trying to).
    pub playing: bool,
    /// Wants to play but waits for data (`waitingToPlayAtSpecifiedRate`).
    pub stalled: bool,
    pub rate: f32,
    pub volume: f32,
    pub muted: bool,
    /// Playback reached the end since the last poll (once per item).
    pub ended: bool,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            status: Status::Idle,
            time: 0.0,
            duration: None,
            buffered: None,
            playing: false,
            stalled: false,
            rate: 1.0,
            volume: 1.0,
            muted: false,
            ended: false,
        }
    }
}

// ---------------------------------------------------------------------------- backend

/// What the app drives. [`Engine`] is AVFoundation; tests use `fake::FakeBackend`.
pub trait Backend {
    /// Replace the current item with `source` (paused; call `play`).
    fn open(&mut self, source: &Source) -> Result<(), String>;
    /// Drop the current item.
    fn close(&mut self);
    fn play(&mut self);
    fn pause(&mut self);
    fn toggle(&mut self) {
        if self.is_playing() {
            self.pause();
        } else {
            self.play();
        }
    }
    fn is_playing(&self) -> bool;
    /// Jump to `secs` (clamped). `precise` seeks to the exact frame; otherwise the nearest
    /// keyframe, which is what scrubbing wants.
    fn seek(&mut self, secs: f64, precise: bool);
    fn seek_by(&mut self, delta: f64) {
        let t = self.time() + delta;
        self.seek(t, true);
    }
    fn time(&self) -> f64;
    fn duration(&self) -> Option<f64>;
    fn speed(&self) -> f32;
    /// Playback rate used while playing (0.5 .. 2.0 are sensible).
    fn set_speed(&mut self, speed: f32);
    fn volume(&self) -> f32;
    fn set_volume(&mut self, v: f32);
    fn muted(&self) -> bool;
    fn set_muted(&mut self, muted: bool);
    fn info(&self) -> &MediaInfo;
    /// The newest decoded video frame (egui texture id and pixel size), once one arrived.
    fn frame(&self) -> Option<(egui::TextureId, [usize; 2])>;
    /// Give the backend the wgpu device so frames can be shared with the GPU directly
    /// (Metal / IOSurface) instead of copied through the CPU. Called every frame; cheap.
    fn attach_gpu(&mut self, _render_state: &egui_wgpu::RenderState) {}
    /// How frames reach the screen: `"gpu"` (IOSurface shared with Metal), `"cpu"` (BGRA
    /// copy), `"none"` (no frame yet).
    fn frame_path(&self) -> &'static str {
        "none"
    }
    /// The PCM being rendered (for the spectrum), once the audio tap is attached.
    fn audio(&self) -> Option<&AudioShared>;
    /// Subtitle / caption tracks by display name (empty when the item has none).
    fn subtitles(&self) -> &[String];
    /// The selected subtitle track, if any.
    fn subtitle(&self) -> Option<usize>;
    /// Select a subtitle track (`None` = off).
    fn select_subtitle(&mut self, index: Option<usize>);
    /// The caption text to show right now (from the selected subtitle track).
    fn caption(&self) -> Option<String>;
    /// Once per frame: read the player, pull a video frame, fill in track / metadata info.
    fn poll(&mut self, ctx: &egui::Context) -> Snapshot;
}

// ---------------------------------------------------------------------------- captions

/// Where the legible output's delegate leaves the current caption text.
type CaptionSlot = Arc<Mutex<Option<String>>>;

struct CaptionIvars {
    slot: CaptionSlot,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; the class has no `Drop`.
    #[unsafe(super(NSObject))]
    #[name = "FuidePlayerCaptionDelegate"]
    #[ivars = CaptionIvars]
    struct CaptionDelegate;

    unsafe impl NSObjectProtocol for CaptionDelegate {}

    unsafe impl AVPlayerItemOutputPushDelegate for CaptionDelegate {}

    unsafe impl AVPlayerItemLegibleOutputPushDelegate for CaptionDelegate {
        #[unsafe(method(legibleOutput:didOutputAttributedStrings:nativeSampleBuffers:forItemTime:))]
        unsafe fn did_output(
            &self,
            _output: &AVPlayerItemLegibleOutput,
            strings: &NSArray<NSAttributedString>,
            _native_samples: &NSArray,
            _item_time: CMTime,
        ) {
            let text = strings
                .iter()
                .map(|s| s.string().to_string())
                .collect::<Vec<_>>()
                .join("\n");
            if let Ok(mut slot) = self.ivars().slot.lock() {
                *slot = (!text.trim().is_empty()).then_some(text);
            }
        }
    }
);

impl CaptionDelegate {
    fn new(slot: CaptionSlot) -> Retained<Self> {
        let this = Self::alloc().set_ivars(CaptionIvars { slot });
        // SAFETY: plain NSObject init.
        unsafe { msg_send![super(this), init] }
    }
}

// ---------------------------------------------------------------------------- gpu frames

/// Zero-copy video frames: the `CVPixelBuffer`'s IOSurface becomes a Metal texture, wrapped
/// into wgpu and registered with egui's renderer as a native texture (updated in place).
struct GpuFrames {
    render_state: egui_wgpu::RenderState,
    tex_id: Option<egui::TextureId>,
    size: Option<[usize; 2]>,
    /// The last few frames' buffers and textures, kept alive while the GPU may still read them.
    keep: std::collections::VecDeque<(CFRetained<CVPixelBuffer>, wgpu::Texture)>,
    /// The Metal path did not work here (non-Metal adapter, no IOSurface): use the CPU copy.
    failed: bool,
}

impl GpuFrames {
    /// Wrap `pb` for the GPU and point egui's texture at it. `false` = fall back to the copy.
    fn show(&mut self, pb: &CVPixelBuffer, w: usize, h: usize) -> bool {
        if self.failed {
            return false;
        }
        let Some(surface) = CVPixelBufferGetIOSurface(Some(pb)) else {
            self.failed = true;
            return false;
        };
        let device = &self.render_state.device;
        // SAFETY: the Metal backend is the only one eframe uses on macOS; the raw MTLDevice is
        // used for one texture creation while the guard is alive.
        let raw = unsafe {
            device.as_hal::<wgpu::hal::api::Metal>().and_then(|hal| {
                let desc =
                    MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                        MTLPixelFormat::BGRA8Unorm_sRGB,
                        w,
                        h,
                        false,
                    );
                desc.setUsage(MTLTextureUsage::ShaderRead);
                desc.setStorageMode(MTLStorageMode::Shared);
                hal.raw_device()
                    .newTextureWithDescriptor_iosurface_plane(&desc, &surface, 0)
            })
        };
        let Some(raw) = raw else {
            self.failed = true;
            return false;
        };
        // SAFETY: `raw` is a 2D BGRA8 sRGB texture of exactly this size; wgpu takes ownership.
        let texture = unsafe {
            let hal_tex = wgpu::hal::metal::Device::texture_from_raw(
                raw,
                wgpu::TextureFormat::Bgra8UnormSrgb,
                MTLTextureType::Type2D,
                1,
                1,
                wgpu::hal::CopyExtent {
                    width: w as u32,
                    height: h as u32,
                    depth: 1,
                },
                None,
            );
            device.create_texture_from_hal::<wgpu::hal::api::Metal>(
                hal_tex,
                &wgpu::TextureDescriptor {
                    label: Some("video-frame-iosurface"),
                    size: wgpu::Extent3d {
                        width: w as u32,
                        height: h as u32,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Bgra8UnormSrgb,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                wgpu::TextureUses::RESOURCE,
            )
        };
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        {
            let mut renderer = self.render_state.renderer.write();
            match self.tex_id {
                Some(id) => renderer.update_egui_texture_from_wgpu_texture(
                    device,
                    &view,
                    wgpu::FilterMode::Linear,
                    id,
                ),
                None => {
                    self.tex_id = Some(renderer.register_native_texture(
                        device,
                        &view,
                        wgpu::FilterMode::Linear,
                    ));
                }
            }
        }
        self.size = Some([w, h]);
        // SAFETY: `pb` is a valid, retained pixel buffer; we take our own reference.
        let owned = unsafe { CFRetained::retain(std::ptr::NonNull::from(pb)) };
        self.keep.push_back((owned, texture));
        while self.keep.len() > 3 {
            self.keep.pop_front();
        }
        true
    }

    fn clear(&mut self) {
        self.keep.clear();
        self.size = None;
    }
}

// ---------------------------------------------------------------------------- engine

pub struct Engine {
    mtm: MainThreadMarker,
    player: Retained<AVPlayer>,
    item: Option<Retained<AVPlayerItem>>,
    output: Option<Retained<AVPlayerItemVideoOutput>>,
    /// Set by the asynchronous metadata load of the current asset.
    metadata_ready: Arc<AtomicBool>,
    /// CPU path: an egui-managed texture updated from a BGRA copy.
    texture: Option<egui::TextureHandle>,
    rgba: Vec<u8>,
    /// GPU path: the frame's IOSurface wrapped as a Metal texture and registered with egui.
    gpu: Option<GpuFrames>,
    /// Audio tap on the current item (not for HLS, where audio mixes are ignored).
    tap: Option<AudioTap>,
    tap_wanted: bool,
    /// Subtitles: the legible output feeding `caption`, the selection group and its options.
    legible: Option<Retained<AVPlayerItemLegibleOutput>>,
    caption_delegate: Option<Retained<CaptionDelegate>>,
    caption: CaptionSlot,
    subtitle_group: Option<Retained<AVMediaSelectionGroup>>,
    subtitle_options: Vec<Retained<AVMediaSelectionOption>>,
    subtitle_names: Vec<String>,
    subtitle_index: Option<usize>,
    info: MediaInfo,
    /// Desired playback rate while playing (1.0 = normal).
    speed: f32,
    was_playing: bool,
    ended_reported: bool,
}

impl Engine {
    /// `None` off the main thread (AVFoundation objects live there).
    pub fn new() -> Option<Self> {
        Some(Self::new_on(MainThreadMarker::new()?))
    }

    fn new_on(mtm: MainThreadMarker) -> Self {
        // SAFETY: plain constructor.
        let player = unsafe { AVPlayer::playerWithPlayerItem(None, mtm) };
        Self {
            mtm,
            player,
            item: None,
            output: None,
            metadata_ready: Arc::new(AtomicBool::new(false)),
            texture: None,
            rgba: Vec::new(),
            gpu: None,
            tap: None,
            tap_wanted: true,
            legible: None,
            caption_delegate: None,
            caption: Arc::new(Mutex::new(None)),
            subtitle_group: None,
            subtitle_options: Vec::new(),
            subtitle_names: Vec::new(),
            subtitle_index: None,
            info: MediaInfo::default(),
            speed: 1.0,
            was_playing: false,
            ended_reported: false,
        }
    }

    pub fn has_item(&self) -> bool {
        self.item.is_some()
    }
}

impl Backend for Engine {
    /// Replace the current item with `source` (paused; call `play`).
    fn open(&mut self, source: &Source) -> Result<(), String> {
        let Some(url) = source.ns_url() else {
            return Err("malformed URL".into());
        };
        self.close();
        self.tap_wanted = match source {
            Source::Url(u) => !u.to_ascii_lowercase().contains(".m3u8"),
            Source::File(_) => true,
        };
        // SAFETY: all objects created here are used on the main thread only; the dictionary
        // types match what `initWithPixelBufferAttributes:` documents.
        unsafe {
            let asset = AVURLAsset::URLAssetWithURL_options(&url, None);
            // metadata may need the network: load it asynchronously, poll the flag
            let flag = self.metadata_ready.clone();
            let block = block2::RcBlock::new(move || flag.store(true, Ordering::Release));
            let keys = NSArray::from_slice(&[
                ns_string!("commonMetadata"),
                ns_string!("duration"),
                ns_string!("tracks"),
                ns_string!("availableChapterLocales"),
                ns_string!("availableMediaCharacteristicsWithMediaSelectionOptions"),
            ]);
            asset.loadValuesAsynchronouslyForKeys_completionHandler(&keys, Some(&block));

            let item = AVPlayerItem::playerItemWithAsset(&asset, self.mtm);
            let key: &NSString = kCVPixelBufferPixelFormatTypeKey.as_ref();
            let value = NSNumber::new_u32(kCVPixelFormatType_32BGRA);
            // IOSurface-backed buffers can be wrapped as Metal textures without a copy
            let surface_key: &NSString = kCVPixelBufferIOSurfacePropertiesKey.as_ref();
            let surface_props: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::new();
            let attrs: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::from_slices(
                &[key, surface_key],
                &[&*value as &AnyObject, &*surface_props as &AnyObject],
            );
            let output = AVPlayerItemVideoOutput::initWithPixelBufferAttributes(
                AVPlayerItemVideoOutput::alloc(),
                Some(&attrs),
            );
            item.addOutput(&output);
            // captions: attributed strings pushed to a delegate on the main queue
            let legible = AVPlayerItemLegibleOutput::initWithMediaSubtypesForNativeRepresentation(
                AVPlayerItemLegibleOutput::alloc(),
                &NSArray::new(),
            );
            let delegate = CaptionDelegate::new(self.caption.clone());
            legible.setDelegate_queue(
                Some(ProtocolObject::from_ref(&*delegate)),
                Some(dispatch2::DispatchQueue::main()),
            );
            item.addOutput(&legible);
            self.legible = Some(legible);
            self.caption_delegate = Some(delegate);
            self.player
                .setActionAtItemEnd(AVPlayerActionAtItemEnd::Pause);
            self.player.replaceCurrentItemWithPlayerItem(Some(&item));
            self.item = Some(item);
            self.output = Some(output);
        }
        Ok(())
    }

    /// Drop the current item (the texture keeps the last frame until the next one).
    fn close(&mut self) {
        // SAFETY: main thread.
        unsafe {
            self.player.pause();
            self.player.replaceCurrentItemWithPlayerItem(None);
        }
        self.item = None;
        self.output = None;
        if let Some(gpu) = &mut self.gpu {
            gpu.clear();
        }
        self.tap = None;
        self.legible = None;
        self.caption_delegate = None;
        if let Ok(mut c) = self.caption.lock() {
            *c = None;
        }
        self.subtitle_group = None;
        self.subtitle_options.clear();
        self.subtitle_names.clear();
        self.subtitle_index = None;
        self.metadata_ready = Arc::new(AtomicBool::new(false));
        self.info = MediaInfo::default();
        self.was_playing = false;
        self.ended_reported = false;
    }

    fn play(&mut self) {
        if self.item.is_none() {
            return;
        }
        // SAFETY: main thread.
        unsafe {
            // restart from the top when the item ended
            if let Some(d) = self.duration() {
                if self.time() >= d - 0.05 {
                    self.seek(0.0, true);
                }
            }
            self.player.setRate(self.speed);
        }
        self.ended_reported = false;
    }

    fn pause(&mut self) {
        // SAFETY: main thread.
        unsafe { self.player.pause() };
    }

    fn is_playing(&self) -> bool {
        // SAFETY: main thread.
        unsafe { self.player.timeControlStatus() != AVPlayerTimeControlStatus::Paused }
    }

    /// Jump to `secs` (clamped). `precise` seeks to the exact frame; otherwise the nearest
    /// keyframe, which is what scrubbing wants.
    fn seek(&mut self, secs: f64, precise: bool) {
        if self.item.is_none() {
            return;
        }
        let max = self.duration().unwrap_or(f64::MAX);
        let secs = secs.clamp(0.0, max);
        // SAFETY: main thread; CMTime values are plain structs.
        unsafe {
            let t = CMTime::with_seconds(secs, SEEK_TIMESCALE);
            if precise {
                self.player
                    .seekToTime_toleranceBefore_toleranceAfter(t, kCMTimeZero, kCMTimeZero);
            } else {
                self.player.seekToTime(t);
            }
        }
        self.ended_reported = false;
    }

    fn time(&self) -> f64 {
        // SAFETY: main thread.
        let t = unsafe { self.player.currentTime().seconds() };
        if t.is_finite() {
            t.max(0.0)
        } else {
            0.0
        }
    }

    fn duration(&self) -> Option<f64> {
        let item = self.item.as_ref()?;
        // SAFETY: main thread.
        let d = unsafe { item.duration().seconds() };
        (d.is_finite() && d > 0.0).then_some(d)
    }

    fn speed(&self) -> f32 {
        self.speed
    }

    /// Playback rate used while playing (0.5 .. 2.0 are sensible).
    fn set_speed(&mut self, speed: f32) {
        self.speed = speed;
        if self.is_playing() {
            // SAFETY: main thread.
            unsafe { self.player.setRate(speed) };
        }
    }

    fn volume(&self) -> f32 {
        // SAFETY: main thread.
        unsafe { self.player.volume() }
    }

    fn set_volume(&mut self, v: f32) {
        // SAFETY: main thread.
        unsafe { self.player.setVolume(v.clamp(0.0, 1.0)) };
    }

    fn muted(&self) -> bool {
        // SAFETY: main thread.
        unsafe { self.player.isMuted() }
    }

    fn set_muted(&mut self, muted: bool) {
        // SAFETY: main thread.
        unsafe { self.player.setMuted(muted) };
    }

    fn info(&self) -> &MediaInfo {
        &self.info
    }

    /// The newest decoded video frame, if the item has video and a frame arrived.
    fn frame(&self) -> Option<(egui::TextureId, [usize; 2])> {
        if let Some(gpu) = &self.gpu {
            if let (Some(id), Some(size)) = (gpu.tex_id, gpu.size) {
                return Some((id, size));
            }
        }
        self.texture.as_ref().map(|t| (t.id(), t.size()))
    }

    fn frame_path(&self) -> &'static str {
        match (&self.gpu, &self.texture) {
            (Some(g), _) if g.size.is_some() => "gpu",
            (_, Some(_)) => "cpu",
            _ => "none",
        }
    }

    fn attach_gpu(&mut self, render_state: &egui_wgpu::RenderState) {
        if self.gpu.is_none() {
            self.gpu = Some(GpuFrames {
                render_state: render_state.clone(),
                tex_id: None,
                size: None,
                keep: std::collections::VecDeque::new(),
                failed: false,
            });
        }
    }

    fn audio(&self) -> Option<&AudioShared> {
        self.tap.as_ref().map(|t| &*t.shared)
    }

    fn subtitles(&self) -> &[String] {
        &self.subtitle_names
    }

    fn subtitle(&self) -> Option<usize> {
        self.subtitle_index
    }

    fn select_subtitle(&mut self, index: Option<usize>) {
        let (Some(item), Some(group)) = (&self.item, &self.subtitle_group) else {
            return;
        };
        let option = index.and_then(|i| self.subtitle_options.get(i));
        // SAFETY: main thread; the option belongs to this group.
        unsafe {
            if option.is_none() && !group.allowsEmptySelection() {
                return;
            }
            item.selectMediaOption_inMediaSelectionGroup(option.map(|o| &**o), group);
        }
        self.subtitle_index = index.filter(|_| option.is_some());
        if let Ok(mut c) = self.caption.lock() {
            *c = None;
        }
    }

    fn caption(&self) -> Option<String> {
        self.subtitle_index?;
        self.caption.lock().ok().and_then(|c| c.clone())
    }

    /// Once per frame: read the player, pull a video frame, fill in track / metadata info.
    fn poll(&mut self, ctx: &egui::Context) -> Snapshot {
        let Some(item) = self.item.clone() else {
            return Snapshot {
                volume: self.volume(),
                muted: self.muted(),
                rate: self.speed,
                ..Snapshot::default()
            };
        };
        // SAFETY: main thread; every pointer read comes from a live AVFoundation object.
        let (status, control) = unsafe {
            let status = match item.status() {
                AVPlayerItemStatus::ReadyToPlay => Status::Ready,
                AVPlayerItemStatus::Failed => Status::Failed(
                    item.error()
                        .map(|e| e.localizedDescription().to_string())
                        .unwrap_or_else(|| "unknown error".into()),
                ),
                _ => Status::Loading,
            };
            (status, self.player.timeControlStatus())
        };
        let time = self.time();
        let duration = self.duration();
        let buffered = self.buffered(&item, time);
        let playing = control != AVPlayerTimeControlStatus::Paused;
        let stalled = control == AVPlayerTimeControlStatus::WaitingToPlayAtSpecifiedRate;

        if status == Status::Ready {
            if !self.info.tracks_loaded {
                self.read_tracks(&item);
            }
            if !self.info.metadata_loaded && self.metadata_ready.load(Ordering::Acquire) {
                self.read_metadata(&item);
            }
            self.pull_frame(ctx);
        }

        // end of item: the player pauses by itself (`actionAtItemEnd = Pause`)
        let mut ended = false;
        if playing {
            self.was_playing = true;
        } else if self.was_playing && !self.ended_reported {
            if let Some(d) = duration {
                if time >= d - 0.05 {
                    ended = true;
                    self.ended_reported = true;
                    self.was_playing = false;
                }
            }
        }

        Snapshot {
            status,
            time,
            duration,
            buffered,
            playing,
            stalled,
            rate: self.speed,
            volume: self.volume(),
            muted: self.muted(),
            ended,
        }
    }
}

impl Engine {
    fn buffered(&self, item: &AVPlayerItem, time: f64) -> Option<f64> {
        // SAFETY: main thread; `loadedTimeRanges` holds NSValues wrapping CMTimeRange.
        unsafe {
            let ranges = item.loadedTimeRanges();
            let mut best: Option<f64> = None;
            for v in ranges.iter() {
                let r = v.CMTimeRangeValue();
                let start = r.start.seconds();
                let end = r.end().seconds();
                if !start.is_finite() || !end.is_finite() {
                    continue;
                }
                if start - 0.5 <= time && time <= end + 0.5 {
                    best = Some(best.map_or(end, |b: f64| b.max(end)));
                }
            }
            best
        }
    }

    fn read_tracks(&mut self, item: &AVPlayerItem) {
        // SAFETY: main thread; the item is ready, so its tracks and their format
        // descriptions are loaded and reading them does not block.
        unsafe {
            let tracks = item.tracks();
            if tracks.is_empty() {
                return; // not populated yet; try again next frame
            }
            let video_type = AVMediaTypeVideo;
            let audio_type = AVMediaTypeAudio;
            for t in tracks.iter() {
                let Some(track) = t.assetTrack() else {
                    continue;
                };
                let media = track.mediaType();
                let descs = track.formatDescriptions();
                let Some(first) = descs.firstObject() else {
                    continue;
                };
                let desc: &CMFormatDescription =
                    &*(Retained::as_ptr(&first) as *const CMFormatDescription);
                let codec = fourcc(desc.media_sub_type());
                if video_type.is_some_and(|v| *v == *media) {
                    let dims = CMVideoFormatDescriptionGetDimensions(desc);
                    self.info.video = Some(VideoInfo {
                        codec: codec_name(&codec),
                        width: dims.width.max(0) as u32,
                        height: dims.height.max(0) as u32,
                        fps: track.nominalFrameRate(),
                        bitrate: track.estimatedDataRate(),
                    });
                } else if audio_type.is_some_and(|a| *a == *media) {
                    if self.tap_wanted && self.tap.is_none() {
                        if let Some(tap) = AudioTap::new() {
                            let params =
                                AVMutableAudioMixInputParameters::audioMixInputParametersWithTrack(
                                    Some(&track),
                                );
                            params.setAudioTapProcessor(Some(tap.raw()));
                            let mix = AVMutableAudioMix::audioMix();
                            let list = NSArray::from_slice(&[
                                &*params as &objc2_av_foundation::AVAudioMixInputParameters
                            ]);
                            mix.setInputParameters(&list);
                            item.setAudioMix(Some(&mix));
                            self.tap = Some(tap);
                        }
                    }
                    let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(desc);
                    let (sample_rate, channels) = if asbd.is_null() {
                        (0.0, 0)
                    } else {
                        ((*asbd).mSampleRate, (*asbd).mChannelsPerFrame)
                    };
                    self.info.audio = Some(AudioInfo {
                        codec: codec_name(&codec),
                        sample_rate,
                        channels,
                        bitrate: track.estimatedDataRate(),
                    });
                }
            }
            self.info.tracks_loaded = true;
        }
    }

    fn read_metadata(&mut self, item: &AVPlayerItem) {
        // SAFETY: main thread; the asynchronous load of `commonMetadata` completed (with or
        // without success), so reading it is synchronous and local.
        unsafe {
            let asset: Retained<AVAsset> = item.asset();
            let items = asset.commonMetadata();
            self.info.title = metadata_string(&items, AVMetadataCommonKeyTitle);
            self.info.artist = metadata_string(&items, AVMetadataCommonKeyArtist);
            self.info.album = metadata_string(&items, AVMetadataCommonKeyAlbumName);
            // chapters (QuickTime chapter tracks), best match for the system languages
            #[allow(deprecated)]
            let mut groups = asset.chapterMetadataGroupsBestMatchingPreferredLanguages(
                &NSLocale::preferredLanguages(),
            );
            if groups.is_empty() {
                // no chapter locale matches the system languages: take the first one there is
                if let Some(locale) = asset.availableChapterLocales().firstObject() {
                    #[allow(deprecated)]
                    let by_locale = asset
                        .chapterMetadataGroupsWithTitleLocale_containingItemsWithCommonKeys(
                            &locale, None,
                        );
                    groups = by_locale;
                }
            }
            self.info.chapters = groups
                .iter()
                .map(|g| {
                    let range = g.timeRange();
                    let start = range.start.seconds();
                    let end = range.end().seconds();
                    let items = g.items();
                    let title = metadata_string(&items, AVMetadataCommonKeyTitle)
                        .or_else(|| {
                            items
                                .iter()
                                .find_map(|it| it.stringValue().map(|s| s.to_string()))
                        })
                        .unwrap_or_default();
                    Chapter {
                        title,
                        start: if start.is_finite() { start } else { 0.0 },
                        end: if end.is_finite() { end } else { start },
                    }
                })
                .filter(|c| !c.title.is_empty() || c.end > c.start)
                .collect();
            // subtitle / caption tracks
            if let Some(legible) = AVMediaCharacteristicLegible {
                #[allow(deprecated)]
                let group = asset.mediaSelectionGroupForMediaCharacteristic(legible);
                if let Some(group) = group {
                    // forced-only tracks (shown automatically for foreign dialogue) are not
                    // something to pick by hand
                    let forced = AVMediaCharacteristicContainsOnlyForcedSubtitles;
                    self.subtitle_options = group
                        .options()
                        .iter()
                        .filter(|o| !forced.is_some_and(|f| o.hasMediaCharacteristic(f)))
                        .collect();
                    self.subtitle_names = self
                        .subtitle_options
                        .iter()
                        .map(|o| o.displayName().to_string())
                        .collect();
                    let current = item
                        .currentMediaSelection()
                        .selectedMediaOptionInMediaSelectionGroup(&group);
                    self.subtitle_index = current
                        .and_then(|cur| self.subtitle_options.iter().position(|o| **o == *cur));
                    self.subtitle_group = Some(group);
                }
            }
            self.info.metadata_loaded = true;
        }
    }

    /// Copy the newest video frame (BGRA) into the egui texture (RGBA).
    fn pull_frame(&mut self, ctx: &egui::Context) {
        let Some(output) = self.output.clone() else {
            return;
        };
        // SAFETY: main thread; the pixel buffer is locked read-only for the duration of the
        // copy, and `bytes_per_row * height` bytes are readable from its base address.
        unsafe {
            let t = self.player.currentTime();
            if !output.hasNewPixelBufferForItemTime(t) {
                return;
            }
            let Some(pb) =
                output.copyPixelBufferForItemTime_itemTimeForDisplay(t, std::ptr::null_mut())
            else {
                return;
            };
            if CVPixelBufferGetPixelFormatType(&pb) != kCVPixelFormatType_32BGRA {
                return;
            }
            let w = CVPixelBufferGetWidth(&pb);
            let h = CVPixelBufferGetHeight(&pb);
            if w == 0 || h == 0 {
                return;
            }
            if let Some(gpu) = &mut self.gpu {
                if gpu.show(&pb, w, h) {
                    return;
                }
            }
            if CVPixelBufferLockBaseAddress(&pb, CVPixelBufferLockFlags::ReadOnly) != 0 {
                return;
            }
            let bpr = CVPixelBufferGetBytesPerRow(&pb);
            let base = CVPixelBufferGetBaseAddress(&pb) as *const u8;
            if !base.is_null() {
                self.rgba.resize(w * h * 4, 0);
                for y in 0..h {
                    let row = std::slice::from_raw_parts(base.add(y * bpr), w * 4);
                    let out = &mut self.rgba[y * w * 4..(y + 1) * w * 4];
                    for (src, dst) in row.chunks_exact(4).zip(out.chunks_exact_mut(4)) {
                        dst[0] = src[2];
                        dst[1] = src[1];
                        dst[2] = src[0];
                        dst[3] = 255;
                    }
                }
                let image = egui::ColorImage::from_rgba_unmultiplied([w, h], &self.rgba);
                let opts = egui::TextureOptions::LINEAR;
                match &mut self.texture {
                    Some(tex) => tex.set(image, opts),
                    None => self.texture = Some(ctx.load_texture("video-frame", image, opts)),
                }
            }
            CVPixelBufferUnlockBaseAddress(&pb, CVPixelBufferLockFlags::ReadOnly);
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.close();
    }
}

/// First string value of the metadata item with `key`.
unsafe fn metadata_string(
    items: &NSArray<AVMetadataItem>,
    key: Option<&'static objc2_av_foundation::AVMetadataKey>,
) -> Option<String> {
    let key = key?;
    for it in items.iter() {
        if it.commonKey().is_some_and(|k| *k == *key) {
            if let Some(s) = it.stringValue() {
                let s = s.to_string();
                if !s.trim().is_empty() {
                    return Some(s);
                }
            }
        }
    }
    None
}

/// Four-character code as text (`avc1`, `mp4a`, ...).
pub fn fourcc(code: u32) -> String {
    code.to_be_bytes()
        .iter()
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '?'
            }
        })
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// Human name for the codecs macOS commonly plays; unknown codes are shown as-is.
pub fn codec_name(fourcc: &str) -> String {
    match fourcc {
        "avc1" | "avc3" => "H.264",
        "hvc1" | "hev1" => "HEVC",
        "av01" => "AV1",
        "mp4v" => "MPEG-4",
        "apch" | "apcn" | "apcs" | "apco" | "ap4h" | "ap4x" => "ProRes",
        "jpeg" | "mjpa" | "mjpb" => "MJPEG",
        "mp4a" | "aac" => "AAC",
        ".mp3" | "mp3" => "MP3",
        "alac" => "ALAC",
        "flac" => "FLAC",
        "lpcm" | "sowt" | "twos" | "in24" | "in32" | "fl32" | "fl64" => "PCM",
        "ac-3" => "AC-3",
        "ec-3" => "E-AC-3",
        "opus" => "Opus",
        other => return other.to_uppercase(),
    }
    .to_string()
}

/// Deterministic stand-in for tests: ready two polls after `open`, time advances a quarter
/// second per poll while playing, ends at the duration. What it "decodes" depends on the
/// source's label: `*.mp4` / `*.mov` / URLs have a 160x120 video track (a solid texture),
/// `*.mp3` / `*.wav` / `*.m4a` are audio only, names containing `missing` fail.
#[cfg(test)]
pub mod fake {
    use super::*;

    #[derive(Default)]
    pub struct FakeBackend {
        source: Option<Source>,
        polls: u32,
        status: Status,
        time: f64,
        duration: Option<f64>,
        playing: bool,
        speed: f32,
        volume: f32,
        muted: bool,
        info: MediaInfo,
        texture: Option<egui::TextureHandle>,
        was_playing: bool,
        ended_reported: bool,
        audio: Arc<AudioShared>,
        subtitle_names: Vec<String>,
        subtitle_index: Option<usize>,
        /// Every `open` in order (for assertions).
        pub opened: Vec<Source>,
        pub seeks: Vec<f64>,
    }

    impl FakeBackend {
        pub fn new() -> Self {
            Self {
                speed: 1.0,
                volume: 1.0,
                ..Default::default()
            }
        }
    }

    impl Backend for FakeBackend {
        fn open(&mut self, source: &Source) -> Result<(), String> {
            self.close();
            self.opened.push(source.clone());
            self.source = Some(source.clone());
            self.status = Status::Loading;
            Ok(())
        }
        fn close(&mut self) {
            self.source = None;
            self.status = Status::Idle;
            self.polls = 0;
            self.time = 0.0;
            self.duration = None;
            self.playing = false;
            self.info = MediaInfo::default();
            self.texture = None;
            self.was_playing = false;
            self.ended_reported = false;
            self.subtitle_names.clear();
            self.subtitle_index = None;
        }
        fn play(&mut self) {
            if self.source.is_none() {
                return;
            }
            if let Some(d) = self.duration {
                if self.time >= d - 0.05 {
                    self.time = 0.0;
                }
            }
            self.playing = true;
            self.ended_reported = false;
        }
        fn pause(&mut self) {
            self.playing = false;
        }
        fn is_playing(&self) -> bool {
            self.playing
        }
        fn seek(&mut self, secs: f64, _precise: bool) {
            if self.source.is_none() {
                return;
            }
            self.time = secs.clamp(0.0, self.duration.unwrap_or(f64::MAX));
            self.seeks.push(self.time);
            self.ended_reported = false;
        }
        fn time(&self) -> f64 {
            self.time
        }
        fn duration(&self) -> Option<f64> {
            self.duration
        }
        fn speed(&self) -> f32 {
            self.speed
        }
        fn set_speed(&mut self, speed: f32) {
            self.speed = speed;
        }
        fn volume(&self) -> f32 {
            self.volume
        }
        fn set_volume(&mut self, v: f32) {
            self.volume = v.clamp(0.0, 1.0);
        }
        fn muted(&self) -> bool {
            self.muted
        }
        fn set_muted(&mut self, muted: bool) {
            self.muted = muted;
        }
        fn info(&self) -> &MediaInfo {
            &self.info
        }
        fn frame(&self) -> Option<(egui::TextureId, [usize; 2])> {
            self.texture.as_ref().map(|t| (t.id(), t.size()))
        }
        fn frame_path(&self) -> &'static str {
            if self.texture.is_some() {
                "cpu"
            } else {
                "none"
            }
        }
        fn audio(&self) -> Option<&AudioShared> {
            (self.status == Status::Ready).then_some(&*self.audio)
        }
        fn subtitles(&self) -> &[String] {
            &self.subtitle_names
        }
        fn subtitle(&self) -> Option<usize> {
            self.subtitle_index
        }
        fn select_subtitle(&mut self, index: Option<usize>) {
            self.subtitle_index = index.filter(|&i| i < self.subtitle_names.len());
        }
        fn caption(&self) -> Option<String> {
            let name = self.subtitle_names.get(self.subtitle_index?)?;
            (self.time >= 1.0).then(|| format!("{name} caption at {:.0}s", self.time.floor()))
        }
        fn poll(&mut self, ctx: &egui::Context) -> Snapshot {
            let Some(source) = self.source.clone() else {
                return Snapshot {
                    volume: self.volume,
                    muted: self.muted,
                    rate: self.speed,
                    ..Snapshot::default()
                };
            };
            self.polls += 1;
            if self.status == Status::Loading && self.polls >= 2 {
                let label = source.label().to_lowercase();
                if label.contains("missing") {
                    self.status = Status::Failed("no such file".into());
                } else {
                    self.status = Status::Ready;
                    let video =
                        source.is_remote() || label.ends_with(".mp4") || label.ends_with(".mov");
                    self.duration = Some(if source.is_remote() {
                        30.0
                    } else if video {
                        10.0
                    } else {
                        5.0
                    });
                    if video {
                        self.info.video = Some(VideoInfo {
                            codec: "H.264".into(),
                            width: 160,
                            height: 120,
                            fps: 30.0,
                            bitrate: 1_000_000.0,
                        });
                        let img = egui::ColorImage::new(
                            [160, 120],
                            vec![egui::Color32::from_rgb(20, 60, 70); 160 * 120],
                        );
                        self.texture =
                            Some(ctx.load_texture("fake-frame", img, egui::TextureOptions::LINEAR));
                        let d = self.duration.unwrap_or(10.0);
                        self.info.chapters = vec![
                            Chapter {
                                title: "Intro".into(),
                                start: 0.0,
                                end: d * 0.3,
                            },
                            Chapter {
                                title: "Middle".into(),
                                start: d * 0.3,
                                end: d * 0.7,
                            },
                            Chapter {
                                title: "End".into(),
                                start: d * 0.7,
                                end: d,
                            },
                        ];
                        self.subtitle_names = vec!["English".into(), "日本語".into()];
                    }
                    self.info.audio = Some(AudioInfo {
                        codec: if video { "AAC" } else { "MP3" }.into(),
                        sample_rate: 44_100.0,
                        channels: 2,
                        bitrate: 128_000.0,
                    });
                    self.info.tracks_loaded = true;
                    self.info.title = Some(format!("Title of {}", source.label()));
                    self.info.artist = Some("Fake Artist".into());
                    self.info.metadata_loaded = true;
                }
            }
            let mut ended = false;
            if self.status == Status::Ready && self.playing {
                self.was_playing = true;
                // a quarter second of a two-tone signal per poll (44.1 kHz stereo)
                let rate = 44_100.0f32;
                let n = (rate * 0.25) as usize;
                let t0 = self.time as f32;
                let mut buf = Vec::with_capacity(n * 2);
                for i in 0..n {
                    let t = t0 + i as f32 / rate;
                    let s = 0.4 * (std::f32::consts::TAU * 220.0 * t).sin()
                        + 0.2 * (std::f32::consts::TAU * 3000.0 * t).sin();
                    buf.push(s);
                    buf.push(s * 0.5);
                }
                self.audio.set_sample_rate(rate as u32);
                self.audio.push_interleaved(&buf, 2);
                self.time += 0.25 * self.speed as f64;
                if let Some(d) = self.duration {
                    if self.time >= d {
                        self.time = d;
                        self.playing = false;
                    }
                }
            } else if self.was_playing && !self.playing && !self.ended_reported {
                if let Some(d) = self.duration {
                    if self.time >= d - 0.05 {
                        ended = true;
                        self.ended_reported = true;
                        self.was_playing = false;
                    }
                }
            }
            Snapshot {
                status: self.status.clone(),
                time: self.time,
                duration: self.duration,
                buffered: self.duration.map(|d| (self.time + 5.0).min(d)),
                playing: self.playing,
                stalled: false,
                rate: self.speed,
                volume: self.volume,
                muted: self.muted,
                ended,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_parse_accepts_urls_and_existing_files_only() {
        let dir = std::env::temp_dir().join(format!("fuide-player-src-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("clip one.mp4");
        std::fs::write(&f, "x").unwrap();

        assert_eq!(
            Source::parse("https://example.com/a/b.mp4?x=1", &dir),
            Ok(Source::Url("https://example.com/a/b.mp4?x=1".into()))
        );
        assert_eq!(
            Source::parse("  clip one.mp4 ", &dir),
            Ok(Source::File(f.clone()))
        );
        assert_eq!(
            Source::parse(
                &format!("file://{}", dir.join("clip%20one.mp4").display()),
                &dir
            ),
            Ok(Source::File(f.clone()))
        );
        assert_eq!(Source::parse("", &dir).unwrap_err(), "nothing to open");
        assert_eq!(Source::parse("nope.mp4", &dir).unwrap_err(), "no such file");
        assert_eq!(
            Source::parse(&dir.display().to_string(), &dir).unwrap_err(),
            "that is a directory"
        );
        assert_eq!(
            Source::parse("ftp://x/y", &dir).unwrap_err(),
            "only http(s) URLs and local paths"
        );
        assert_eq!(
            Source::parse("http://", &dir).unwrap_err(),
            "URL has no host"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn source_labels_use_file_names_and_url_segments() {
        assert_eq!(Source::File("/a/b/song.m4a".into()).label(), "song.m4a");
        assert_eq!(
            Source::Url("https://cdn.example.com/media/Big%20Buck.mp4?token=1".into()).label(),
            "Big Buck.mp4"
        );
        assert_eq!(
            Source::Url("https://stream.example.com/".into()).label(),
            "stream.example.com"
        );
        assert_eq!(
            Source::Url("https://stream.example.com".into()).label(),
            "stream.example.com"
        );
    }

    #[test]
    fn fourcc_and_codec_names() {
        assert_eq!(fourcc(0x61766331), "avc1");
        assert_eq!(codec_name("avc1"), "H.264");
        assert_eq!(codec_name("hvc1"), "HEVC");
        assert_eq!(codec_name("mp4a"), "AAC");
        assert_eq!(codec_name(".mp3"), "MP3");
        assert_eq!(codec_name("lpcm"), "PCM");
        assert_eq!(codec_name("zzzz"), "ZZZZ");
    }

    /// 16-bit mono WAV with a sine tone, `secs` long.
    #[allow(dead_code)]
    pub(crate) fn write_wav(path: &Path, secs: f64) {
        let rate = 22_050u32;
        let n = (rate as f64 * secs) as usize;
        let mut data = Vec::with_capacity(44 + n * 2);
        let push32 = |d: &mut Vec<u8>, v: u32| d.extend_from_slice(&v.to_le_bytes());
        let push16 = |d: &mut Vec<u8>, v: u16| d.extend_from_slice(&v.to_le_bytes());
        data.extend_from_slice(b"RIFF");
        push32(&mut data, 36 + n as u32 * 2);
        data.extend_from_slice(b"WAVEfmt ");
        push32(&mut data, 16);
        push16(&mut data, 1); // PCM
        push16(&mut data, 1); // mono
        push32(&mut data, rate);
        push32(&mut data, rate * 2);
        push16(&mut data, 2);
        push16(&mut data, 16);
        data.extend_from_slice(b"data");
        push32(&mut data, n as u32 * 2);
        for i in 0..n {
            let s = (i as f64 / rate as f64 * 440.0 * std::f64::consts::TAU).sin();
            push16(&mut data, (s * 8000.0) as i16 as u16);
        }
        std::fs::write(path, data).unwrap();
    }
}
