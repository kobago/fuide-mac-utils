//! Real AVFoundation, on the main thread (`harness = false`), muted. AVFoundation reports
//! item status / duration through the main dispatch queue, so the checks pump the main run
//! loop between polls — the same thing the winit event loop does for the app.

use std::path::Path;
use std::time::{Duration, Instant};

use fuide_player::player::{Backend, Engine, Source, Status};

fn pump() {
    // SAFETY: plain CoreFoundation call on the main thread.
    unsafe {
        objc2_core_foundation::CFRunLoop::run_in_mode(
            objc2_core_foundation::kCFRunLoopDefaultMode,
            0.02,
            false,
        );
    }
}

/// 16-bit mono WAV with a sine tone, `secs` long.
fn write_wav(path: &Path, secs: f64) {
    let rate = 22_050u32;
    let n = (rate as f64 * secs) as usize;
    let mut d = Vec::with_capacity(44 + n * 2);
    let p32 = |d: &mut Vec<u8>, v: u32| d.extend_from_slice(&v.to_le_bytes());
    let p16 = |d: &mut Vec<u8>, v: u16| d.extend_from_slice(&v.to_le_bytes());
    d.extend_from_slice(b"RIFF");
    p32(&mut d, 36 + n as u32 * 2);
    d.extend_from_slice(b"WAVEfmt ");
    p32(&mut d, 16);
    p16(&mut d, 1);
    p16(&mut d, 1);
    p32(&mut d, rate);
    p32(&mut d, rate * 2);
    p16(&mut d, 2);
    p16(&mut d, 16);
    d.extend_from_slice(b"data");
    p32(&mut d, n as u32 * 2);
    for i in 0..n {
        let s = (i as f64 / rate as f64 * 440.0 * std::f64::consts::TAU).sin();
        p16(&mut d, (s * 8000.0) as i16 as u16);
    }
    std::fs::write(path, d).unwrap();
}

fn wait_ready(eng: &mut Engine, ctx: &egui::Context, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let s = eng.poll(ctx);
        if s.status == Status::Ready && eng.info().tracks_loaded {
            return;
        }
        if let Status::Failed(e) = &s.status {
            panic!("{what}: failed: {e}");
        }
        assert!(
            Instant::now() < deadline,
            "{what}: not ready: {:?}",
            s.status
        );
        pump();
    }
}

fn wait_until(
    eng: &mut Engine,
    ctx: &egui::Context,
    what: &str,
    mut f: impl FnMut(&Engine) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let _ = eng.poll(ctx);
        if f(eng) {
            return;
        }
        assert!(Instant::now() < deadline, "{what}: timed out");
        pump();
    }
}

fn main() {
    let dir = std::env::temp_dir().join(format!("fuide-player-engine-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let ctx = egui::Context::default();
    let mut eng = Engine::new().expect("main thread");
    eng.set_muted(true);

    // ---- audio: a generated WAV ------------------------------------------------------------
    let wav = dir.join("tone.wav");
    write_wav(&wav, 1.0);
    eng.open(&Source::File(wav.clone())).unwrap();
    wait_ready(&mut eng, &ctx, "wav");
    let s = eng.poll(&ctx);
    assert!((s.duration.unwrap() - 1.0).abs() < 0.05, "{:?}", s.duration);
    let audio = eng.info().audio.clone().expect("audio track");
    assert_eq!(audio.codec, "PCM");
    assert_eq!(audio.channels, 1);
    assert_eq!(audio.sample_rate, 22_050.0);
    assert!(eng.info().video.is_none());
    assert!(eng.frame().is_none(), "no video frames for audio");
    println!("wav: ready, {audio:?}");

    eng.seek(0.5, true);
    eng.play();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let s = eng.poll(&ctx);
        if s.ended {
            break;
        }
        assert!(Instant::now() < deadline, "wav: did not end: {s:?}");
        pump();
    }
    assert!(!eng.is_playing());
    assert!(eng.time() >= 0.9, "{}", eng.time());
    println!("wav: played to the end");
    // the audio tap saw the PCM go by
    let audio = eng.audio().expect("tap attached to the audio track");
    assert!(
        audio.total() > 1000,
        "tap captured {} samples",
        audio.total()
    );
    assert_eq!(audio.sample_rate(), 22_050);
    let (l, r) = audio.peaks();
    println!(
        "wav: tap {} samples @ {} Hz, peaks {l:.2}/{r:.2}",
        audio.total(),
        audio.sample_rate()
    );

    // play again restarts from the top
    eng.play();
    wait_until(&mut eng, &ctx, "wav restart", |e| {
        e.time() < 0.4 && e.is_playing()
    });
    eng.pause();
    assert!(!eng.is_playing());

    // ---- video: the H.264 / AAC fixture ----------------------------------------------------
    let clip = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/clip.mp4");
    eng.open(&Source::File(clip)).unwrap();
    wait_ready(&mut eng, &ctx, "mp4");
    let v = eng.info().video.clone().expect("video track");
    assert_eq!((v.codec.as_str(), v.width, v.height), ("H.264", 160, 120));
    assert!((v.fps - 15.0).abs() < 0.5, "{}", v.fps);
    let a = eng.info().audio.clone().expect("audio track");
    assert_eq!(a.codec, "AAC");
    assert_eq!(a.sample_rate, 44_100.0);
    println!("mp4: ready, {v:?} {a:?}");

    // metadata arrives through the asynchronous load
    wait_until(&mut eng, &ctx, "mp4 metadata", |e| e.info().metadata_loaded);
    assert_eq!(eng.info().title.as_deref(), Some("FUIDE Test Clip"));
    assert_eq!(eng.info().artist.as_deref(), Some("fuide"));

    // a frame is decoded into the texture even while paused (after a seek)
    eng.seek(1.0, true);
    wait_until(&mut eng, &ctx, "mp4 frame", |e| e.frame().is_some());
    let size = eng.frame().unwrap().1;
    assert_eq!(size, [160, 120]);
    println!("mp4: frame {size:?}");

    // ---- chapters + subtitles: the fixture with a chapter track and a mov_text track ----
    let clip = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/chapters.mp4");
    eng.open(&Source::File(clip)).unwrap();
    wait_ready(&mut eng, &ctx, "chapters");
    wait_until(&mut eng, &ctx, "chapters metadata", |e| {
        e.info().metadata_loaded
    });
    let chapters = eng.info().chapters.clone();
    let titles: Vec<&str> = chapters.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(titles, ["Intro", "Middle Part", "Finale"], "{chapters:?}");
    assert!((chapters[1].start - 2.0).abs() < 0.05 && (chapters[1].end - 4.0).abs() < 0.05);
    assert_eq!(eng.info().chapter_at(2.5), Some(1));
    println!("chapters: {titles:?}");
    let subs = eng.subtitles().to_vec();
    assert_eq!(subs.len(), 1, "one subtitle track: {subs:?}");
    println!("subtitles: {subs:?} (current {:?})", eng.subtitle());
    eng.select_subtitle(Some(0));
    assert_eq!(eng.subtitle(), Some(0));
    // captions arrive through the legible output while playing (delegate on the main queue)
    eng.seek(0.6, true);
    eng.play();
    wait_until(&mut eng, &ctx, "caption", |e| e.caption().is_some());
    let caption = eng.caption().unwrap();
    println!("caption: {caption:?}");
    assert!(caption.contains("Hello from the intro"), "{caption}");
    eng.pause();
    eng.select_subtitle(None);
    assert_eq!(eng.subtitle(), None);
    assert!(eng.caption().is_none());

    // speed and volume are plain pass-throughs
    eng.set_speed(2.0);
    eng.set_volume(0.5);
    let s = eng.poll(&ctx);
    assert_eq!(s.rate, 2.0);
    assert!((s.volume - 0.5).abs() < 1e-3);
    assert!(s.muted);

    // a missing file fails instead of hanging
    eng.open(&Source::File(dir.join("missing.mp4"))).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let s = eng.poll(&ctx);
        if let Status::Failed(e) = s.status {
            println!("missing: failed as expected: {e}");
            break;
        }
        assert!(Instant::now() < deadline, "missing: no failure reported");
        pump();
    }

    eng.close();
    assert_eq!(eng.poll(&ctx).status, Status::Idle);
    let _ = std::fs::remove_dir_all(&dir);
    println!("engine: ok");
}
