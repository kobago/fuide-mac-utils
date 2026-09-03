//! Audio tap: the PCM that `AVPlayer` is actually rendering, captured with an
//! `MTAudioProcessingTap` on the item's audio mix and kept in a ring buffer for the spectrum
//! analyser. The tap's callbacks run on AVFoundation's audio thread; they only write into a
//! mutex-protected ring (skipping the frame if the UI holds the lock), the UI thread reads.
//!
//! AVFoundation does not apply audio mixes to HTTP Live Streaming items, so HLS plays without
//! a spectrum; progressive files over http(s) and local files are fine.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use objc2_core_audio_types::{
    kAudioFormatFlagIsNonInterleaved, AudioBufferList, AudioStreamBasicDescription,
};
use objc2_core_foundation::CFRetained;
use objc2_media_toolbox::{
    kMTAudioProcessingTapCreationFlag_PostEffects, MTAudioProcessingTap,
    MTAudioProcessingTapCallbacks, MTAudioProcessingTapFlags,
};

/// Mono samples kept for analysis (enough for a 4096-point FFT at any rate).
pub const RING_LEN: usize = 8192;

/// What the audio thread shares with the UI.
pub struct AudioShared {
    ring: Mutex<Ring>,
    sample_rate: AtomicU32,
    channels: AtomicU32,
    non_interleaved: AtomicU32,
    /// Latest per-channel peak (0..1, as f32 bits), left and right.
    peak: [AtomicU32; 2],
}

struct Ring {
    buf: Vec<f32>,
    /// Next write position.
    pos: usize,
    /// Samples written in total (0 = nothing yet).
    total: u64,
}

impl Default for AudioShared {
    fn default() -> Self {
        Self {
            ring: Mutex::new(Ring {
                buf: vec![0.0; RING_LEN],
                pos: 0,
                total: 0,
            }),
            sample_rate: AtomicU32::new(0),
            channels: AtomicU32::new(0),
            non_interleaved: AtomicU32::new(0),
            peak: [AtomicU32::new(0), AtomicU32::new(0)],
        }
    }
}

impl AudioShared {
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate.load(Ordering::Relaxed)
    }

    pub fn set_sample_rate(&self, rate: u32) {
        self.sample_rate.store(rate, Ordering::Relaxed);
    }

    /// Copy the newest `n` mono samples (oldest first) into `out`. `false` when nothing has
    /// been captured yet.
    pub fn latest(&self, n: usize, out: &mut Vec<f32>) -> bool {
        let Ok(ring) = self.ring.lock() else {
            return false;
        };
        if ring.total == 0 {
            return false;
        }
        let n = n.min(RING_LEN);
        out.clear();
        out.reserve(n);
        let start = (ring.pos + RING_LEN - n) % RING_LEN;
        for i in 0..n {
            out.push(ring.buf[(start + i) % RING_LEN]);
        }
        true
    }

    /// Latest peak per channel (left, right), 0..1.
    pub fn peaks(&self) -> (f32, f32) {
        (
            f32::from_bits(self.peak[0].load(Ordering::Relaxed)),
            f32::from_bits(self.peak[1].load(Ordering::Relaxed)),
        )
    }

    /// Samples captured so far (grows while audio flows; stalls when it does not).
    pub fn total(&self) -> u64 {
        self.ring.lock().map(|r| r.total).unwrap_or(0)
    }

    /// Feed samples in (also used by the fake backend). `chans` interleaved channels.
    pub fn push_interleaved(&self, samples: &[f32], chans: usize) {
        let chans = chans.max(1);
        let mut peak = [0.0f32; 2];
        if let Ok(mut ring) = self.ring.try_lock() {
            for frame in samples.chunks_exact(chans) {
                let mut sum = 0.0;
                for (c, &s) in frame.iter().enumerate() {
                    sum += s;
                    let slot = c.min(1);
                    peak[slot] = peak[slot].max(s.abs());
                }
                if chans == 1 {
                    peak[1] = peak[0];
                }
                let pos = ring.pos;
                ring.buf[pos] = sum / chans as f32;
                ring.pos = (pos + 1) % RING_LEN;
                ring.total += 1;
            }
        }
        self.peak[0].store(peak[0].to_bits(), Ordering::Relaxed);
        self.peak[1].store(peak[1].to_bits(), Ordering::Relaxed);
    }

    /// One buffer per channel (`kAudioFormatFlagIsNonInterleaved`).
    fn push_planar(&self, planes: &[&[f32]]) {
        let Some(first) = planes.first() else { return };
        let n = first.len();
        let mut peak = [0.0f32; 2];
        if let Ok(mut ring) = self.ring.try_lock() {
            for i in 0..n {
                let mut sum = 0.0;
                for (c, plane) in planes.iter().enumerate() {
                    let s = plane.get(i).copied().unwrap_or(0.0);
                    sum += s;
                    let slot = c.min(1);
                    peak[slot] = peak[slot].max(s.abs());
                }
                if planes.len() == 1 {
                    peak[1] = peak[0];
                }
                let pos = ring.pos;
                ring.buf[pos] = sum / planes.len() as f32;
                ring.pos = (pos + 1) % RING_LEN;
                ring.total += 1;
            }
        }
        self.peak[0].store(peak[0].to_bits(), Ordering::Relaxed);
        self.peak[1].store(peak[1].to_bits(), Ordering::Relaxed);
    }
}

/// An `MTAudioProcessingTap` writing into an [`AudioShared`]. Attach it to the item's audio
/// mix (`AVMutableAudioMixInputParameters::setAudioTapProcessor`).
pub struct AudioTap {
    pub shared: Arc<AudioShared>,
    tap: CFRetained<MTAudioProcessingTap>,
}

impl AudioTap {
    pub fn new() -> Option<Self> {
        let shared = Arc::new(AudioShared::default());
        // the tap owns one Arc reference (released in `finalize`)
        let client = Arc::into_raw(shared.clone()) as *mut c_void;
        let mut callbacks = MTAudioProcessingTapCallbacks {
            version: 0, // kMTAudioProcessingTapCallbacksVersion_0
            clientInfo: client,
            init: Some(tap_init),
            finalize: Some(tap_finalize),
            prepare: Some(tap_prepare),
            unprepare: None,
            process: Some(tap_process),
        };
        let mut raw: *const MTAudioProcessingTap = std::ptr::null();
        // SAFETY: `callbacks` outlives the call (MediaToolbox copies it); `raw` receives a
        // +1 retained tap on success.
        let status = unsafe {
            MTAudioProcessingTap::create(
                None,
                NonNull::from(&mut callbacks),
                kMTAudioProcessingTapCreationFlag_PostEffects,
                NonNull::from(&mut raw),
            )
        };
        if status != 0 || raw.is_null() {
            // SAFETY: the tap was not created, so `finalize` will never run: drop our +1.
            unsafe { drop(Arc::from_raw(client as *const AudioShared)) };
            return None;
        }
        // SAFETY: `raw` is a valid +1 reference from a Create function.
        let tap = unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw as *mut _)) };
        Some(Self { shared, tap })
    }

    pub fn raw(&self) -> &MTAudioProcessingTap {
        &self.tap
    }
}

unsafe extern "C-unwind" fn tap_init(
    _tap: NonNull<MTAudioProcessingTap>,
    client_info: *mut c_void,
    storage_out: NonNull<*mut c_void>,
) {
    // SAFETY: MediaToolbox hands us the out-pointer to fill.
    unsafe { storage_out.write(client_info) };
}

unsafe extern "C-unwind" fn tap_finalize(tap: NonNull<MTAudioProcessingTap>) {
    // SAFETY: the storage is the Arc pointer stored in `tap_init`; this is its last use.
    unsafe {
        let p = tap.as_ref().storage();
        drop(Arc::from_raw(p.as_ptr() as *const AudioShared));
    }
}

unsafe fn shared_of(tap: NonNull<MTAudioProcessingTap>) -> Option<&'static AudioShared> {
    // SAFETY: the storage is a live Arc<AudioShared> until `tap_finalize`.
    unsafe {
        let p = tap.as_ref().storage();
        Some(&*(p.as_ptr() as *const AudioShared))
    }
}

unsafe extern "C-unwind" fn tap_prepare(
    tap: NonNull<MTAudioProcessingTap>,
    _max_frames: isize,
    format: NonNull<AudioStreamBasicDescription>,
) {
    // SAFETY: `format` is valid for the call.
    let f = unsafe { format.read() };
    if let Some(s) = unsafe { shared_of(tap) } {
        s.sample_rate.store(f.mSampleRate as u32, Ordering::Relaxed);
        s.channels.store(f.mChannelsPerFrame, Ordering::Relaxed);
        s.non_interleaved.store(
            u32::from(f.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0),
            Ordering::Relaxed,
        );
    }
}

unsafe extern "C-unwind" fn tap_process(
    tap: NonNull<MTAudioProcessingTap>,
    frames: isize,
    _flags: MTAudioProcessingTapFlags,
    buffers: NonNull<AudioBufferList>,
    frames_out: NonNull<isize>,
    flags_out: NonNull<MTAudioProcessingTapFlags>,
) {
    // SAFETY: standard tap process contract — pull the source audio into the provided list.
    let status = unsafe {
        tap.as_ref().source_audio(
            frames,
            buffers,
            flags_out.as_ptr(),
            std::ptr::null_mut(),
            frames_out.as_ptr(),
        )
    };
    if status != 0 {
        return;
    }
    let Some(shared) = (unsafe { shared_of(tap) }) else {
        return;
    };
    // SAFETY: the buffer list holds `mNumberBuffers` AudioBuffers of 32-bit floats (the tap's
    // processing format is always Float32 PCM), each `mDataByteSize` bytes long.
    unsafe {
        let abl = buffers.as_ref();
        let n = abl.mNumberBuffers as usize;
        let bufs = std::slice::from_raw_parts(abl.mBuffers.as_ptr(), n);
        let as_f32 = |b: &objc2_core_audio_types::AudioBuffer| -> &[f32] {
            if b.mData.is_null() {
                &[]
            } else {
                std::slice::from_raw_parts(b.mData as *const f32, b.mDataByteSize as usize / 4)
            }
        };
        if shared.non_interleaved.load(Ordering::Relaxed) != 0 || n > 1 {
            let planes: Vec<&[f32]> = bufs.iter().map(as_f32).collect();
            shared.push_planar(&planes);
        } else if let Some(b) = bufs.first() {
            shared.push_interleaved(as_f32(b), b.mNumberChannels.max(1) as usize);
        }
    }
}

// ---------------------------------------------------------------------------- analysis

/// Log-spaced spectrum bands with smoothing and peak hold, fed from an [`AudioShared`] (or
/// any mono sample slice) once per UI frame.
pub struct Spectrum {
    pub bands: Vec<f32>,
    pub peaks: Vec<f32>,
    peak_age: Vec<f32>,
    window: Vec<f32>,
    scratch: Vec<f32>,
    re: Vec<f32>,
    im: Vec<f32>,
}

pub const FFT_LEN: usize = 2048;
pub const BAND_COUNT: usize = 48;
const F_LO: f32 = 40.0;
const F_HI: f32 = 16_000.0;

impl Default for Spectrum {
    fn default() -> Self {
        let window = (0..FFT_LEN)
            .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / (FFT_LEN - 1) as f32).cos())
            .collect();
        Self {
            bands: vec![0.0; BAND_COUNT],
            peaks: vec![0.0; BAND_COUNT],
            peak_age: vec![0.0; BAND_COUNT],
            window,
            scratch: Vec::with_capacity(FFT_LEN),
            re: vec![0.0; FFT_LEN],
            im: vec![0.0; FFT_LEN],
        }
    }
}

impl Spectrum {
    /// Analyse the newest samples: `bands` (0..1, dB-scaled) and `peaks` decay over `dt`.
    pub fn update(&mut self, samples: &[f32], sample_rate: f32, dt: f32) {
        if samples.len() < FFT_LEN || sample_rate <= 0.0 {
            self.decay(dt);
            return;
        }
        let start = samples.len() - FFT_LEN;
        for i in 0..FFT_LEN {
            self.re[i] = samples[start + i] * self.window[i];
            self.im[i] = 0.0;
        }
        fft(&mut self.re, &mut self.im);
        let bin_hz = sample_rate / FFT_LEN as f32;
        let ratio = (F_HI / F_LO).ln();
        for b in 0..BAND_COUNT {
            let f0 = F_LO * (ratio * b as f32 / BAND_COUNT as f32).exp();
            let f1 = F_LO * (ratio * (b + 1) as f32 / BAND_COUNT as f32).exp();
            let k0 = ((f0 / bin_hz) as usize).clamp(1, FFT_LEN / 2 - 1);
            let k1 = ((f1 / bin_hz) as usize).clamp(k0 + 1, FFT_LEN / 2);
            let mut power = 0.0f32;
            for k in k0..k1 {
                power += self.re[k] * self.re[k] + self.im[k] * self.im[k];
            }
            power /= (k1 - k0) as f32;
            // magnitude relative to full scale, -60 dB .. 0 dB -> 0..1
            let mag = (power.sqrt() * 2.0 / FFT_LEN as f32 * 4.0).max(1e-9);
            let db = 20.0 * mag.log10();
            let v = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
            // fast attack, slow release
            let cur = self.bands[b];
            self.bands[b] = if v > cur {
                cur + (v - cur) * (dt * 30.0).min(1.0)
            } else {
                cur + (v - cur) * (dt * 8.0).min(1.0)
            };
            if self.bands[b] >= self.peaks[b] {
                self.peaks[b] = self.bands[b];
                self.peak_age[b] = 0.0;
            }
        }
        self.decay_peaks(dt);
    }

    fn decay(&mut self, dt: f32) {
        for b in &mut self.bands {
            *b *= (1.0 - dt * 6.0).max(0.0);
        }
        self.decay_peaks(dt);
    }

    fn decay_peaks(&mut self, dt: f32) {
        for i in 0..BAND_COUNT {
            self.peak_age[i] += dt;
            if self.peak_age[i] > 0.6 {
                self.peaks[i] = (self.peaks[i] - dt * 0.6).max(self.bands[i]);
            }
        }
    }

    /// Scratch buffer for callers that pull samples from an [`AudioShared`].
    pub fn scratch(&mut self) -> &mut Vec<f32> {
        &mut self.scratch
    }
}

/// In-place iterative radix-2 FFT (`re.len()` must be a power of two).
pub fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    debug_assert!(n.is_power_of_two() && im.len() == n);
    // bit reversal
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -std::f32::consts::TAU / len as f32;
        let (wr, wi) = (ang.cos(), ang.sin());
        let mut i = 0;
        while i < n {
            let (mut cr, mut ci) = (1.0f32, 0.0f32);
            for k in 0..len / 2 {
                let (ar, ai) = (re[i + k], im[i + k]);
                let (br, bi) = (re[i + k + len / 2], im[i + k + len / 2]);
                let (tr, ti) = (br * cr - bi * ci, br * ci + bi * cr);
                re[i + k] = ar + tr;
                im[i + k] = ai + ti;
                re[i + k + len / 2] = ar - tr;
                im[i + k + len / 2] = ai - ti;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
            i += len;
        }
        len <<= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fft_finds_a_pure_tone() {
        let n = 1024;
        let mut re: Vec<f32> = (0..n)
            .map(|i| (std::f32::consts::TAU * 64.0 * i as f32 / n as f32).sin())
            .collect();
        let mut im = vec![0.0; n];
        fft(&mut re, &mut im);
        let mags: Vec<f32> = (0..n / 2)
            .map(|k| (re[k] * re[k] + im[k] * im[k]).sqrt())
            .collect();
        let peak = mags
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(k, _)| k)
            .unwrap();
        assert_eq!(peak, 64);
        assert!((mags[64] - n as f32 / 2.0).abs() < 1.0);
    }

    #[test]
    fn ring_keeps_the_newest_samples_and_peaks() {
        let shared = AudioShared::default();
        assert!(!shared.latest(16, &mut Vec::new()), "empty at first");
        // 2 channels interleaved: left ramps, right is silent
        let mut data = Vec::new();
        for i in 0..(RING_LEN + 100) {
            data.push((i % 100) as f32 / 100.0);
            data.push(0.0);
        }
        shared.push_interleaved(&data, 2);
        let mut out = Vec::new();
        assert!(shared.latest(4, &mut out));
        // the last frame was i = RING_LEN + 99 = 8291 -> left 0.91 -> mono (0.91 + 0) / 2
        assert!((out[3] - 0.455).abs() < 1e-5, "{out:?}");
        assert_eq!(out.len(), 4);
        let (l, r) = shared.peaks();
        assert!(l > 0.98 && r == 0.0);
        assert_eq!(shared.total(), (RING_LEN + 100) as u64);
    }

    #[test]
    fn spectrum_bands_react_to_a_tone_in_the_right_place() {
        let rate = 48_000.0;
        let samples: Vec<f32> = (0..FFT_LEN)
            .map(|i| 0.5 * (std::f32::consts::TAU * 1000.0 * i as f32 / rate).sin())
            .collect();
        let mut sp = Spectrum::default();
        for _ in 0..30 {
            sp.update(&samples, rate, 1.0 / 60.0);
        }
        // 1 kHz sits at band ln(1000/40)/ln(400) * 48 ~= 25.8
        let loudest = sp
            .bands
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(b, _)| b)
            .unwrap();
        assert!(
            (24..=27).contains(&loudest),
            "loudest band {loudest}: {:?}",
            sp.bands
        );
        assert!(sp.bands[loudest] > 0.5);
        assert!(sp.bands[5] < 0.3, "low bands stay quiet: {}", sp.bands[5]);
        // silence decays
        for _ in 0..120 {
            sp.update(&[], rate, 1.0 / 60.0);
        }
        assert!(sp.bands[loudest] < 0.05);
    }
}
