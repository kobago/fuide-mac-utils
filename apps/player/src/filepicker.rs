//! The macOS open-file dialog (`NSOpenPanel`), shown as a non-blocking sheet: the result comes
//! back through a channel that the app polls every frame. The app's MCP agent cannot see into
//! this system dialog, so the in-app URL / path dialog (Cmd+L) stays the agent's way in.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};

use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSModalResponseOK, NSOpenPanel};
use objc2_av_foundation::AVURLAsset;
use objc2_foundation::{ns_string, NSArray, NSURL};
use objc2_uniform_type_identifiers::UTType;

pub struct FilePicker {
    tx: Sender<Vec<PathBuf>>,
    rx: Receiver<Vec<PathBuf>>,
    open: bool,
}

impl Default for FilePicker {
    fn default() -> Self {
        let (tx, rx) = channel();
        Self {
            tx,
            rx,
            open: false,
        }
    }
}

impl FilePicker {
    /// Show the panel (multiple files, no directories). Only one panel at a time; off the main
    /// thread nothing happens and `false` is returned.
    pub fn show(&mut self, start_dir: Option<&std::path::Path>) -> bool {
        if self.open {
            return true;
        }
        let Some(mtm) = MainThreadMarker::new() else {
            return false;
        };
        let panel = NSOpenPanel::openPanel(mtm);
        panel.setAllowsMultipleSelection(true);
        panel.setCanChooseDirectories(false);
        panel.setCanChooseFiles(true);
        panel.setMessage(Some(ns_string!(
            "Open media files (what macOS can play: MP4 / MOV / M4A / MP3 / WAV / AIFF / FLAC)"
        )));
        panel.setPrompt(Some(ns_string!("Open")));
        // only what AVFoundation can open is selectable
        let types = playable_types();
        if !types.is_empty() {
            let refs: Vec<&UTType> = types.iter().map(|t| &**t).collect();
            panel.setAllowedContentTypes(&NSArray::from_slice(&refs));
        }
        if let Some(dir) = start_dir {
            let url = NSURL::fileURLWithPath_isDirectory(
                &objc2_foundation::NSString::from_str(&dir.to_string_lossy()),
                true,
            );
            panel.setDirectoryURL(Some(&url));
        }
        let tx = self.tx.clone();
        let result_panel = panel.clone();
        let handler = block2::RcBlock::new(move |response: objc2_app_kit::NSModalResponse| {
            let paths: Vec<PathBuf> = if response == NSModalResponseOK {
                result_panel
                    .URLs()
                    .iter()
                    .filter_map(|u| u.path().map(|p| PathBuf::from(p.to_string())))
                    .collect()
            } else {
                Vec::new()
            };
            let _ = tx.send(paths);
        });
        // the block is retained by AppKit for the panel session
        panel.beginWithCompletionHandler(&handler);
        self.open = true;
        true
    }

    /// The chosen files once the panel closed (empty = cancelled).
    pub fn poll(&mut self) -> Option<Vec<PathBuf>> {
        let r = self.rx.try_recv().ok();
        if r.is_some() {
            self.open = false;
        }
        r
    }

    pub fn is_open(&self) -> bool {
        self.open
    }
}

/// The uniform types AVFoundation lists as playable (`AVURLAsset.audiovisualContentTypes`),
/// for the panel's filter. Empty on failure (then the panel shows everything).
pub fn playable_types() -> Vec<Retained<UTType>> {
    // SAFETY: class method with no preconditions.
    unsafe { AVURLAsset::audiovisualContentTypes() }
        .iter()
        .collect()
}
