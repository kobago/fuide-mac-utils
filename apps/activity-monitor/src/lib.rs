//! FUIDE Activity Monitor — CPU / memory / energy / disk / network with a Sci-Fi FUI look.
//!
//! `app` is the egui application, `sys` the telemetry: the [`sys::Source`] trait, its macOS
//! implementation and the fake one the tests run on.

pub mod app;
pub mod sys;
