//! FUIDE Player — audio / video player on AVFoundation with a Sci-Fi FUI look.
//!
//! `app` is the egui application, `player` the AVFoundation engine behind the `Backend`
//! trait (tests swap in `player::fake::FakeBackend`).

pub mod app;
pub mod audiotap;
pub mod filepicker;
pub mod player;
