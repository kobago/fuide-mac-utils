//! File-system model: directory entries, background loading, kinds, disk usage.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::SystemTime;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Dir,
    App,
    Image,
    Video,
    Audio,
    Doc,
    Code,
    Archive,
    Text,
    Binary,
    Link,
}

impl Kind {
    /// 4-char tag shown in the list (ASCII only — no missing glyphs).
    pub fn tag(self) -> &'static str {
        match self {
            Kind::Dir => "DIR",
            Kind::App => "APP",
            Kind::Image => "IMG",
            Kind::Video => "VID",
            Kind::Audio => "AUD",
            Kind::Doc => "DOC",
            Kind::Code => "SRC",
            Kind::Archive => "PKG",
            Kind::Text => "TXT",
            Kind::Binary => "BIN",
            Kind::Link => "LNK",
        }
    }

    pub fn from_ext(ext: &str) -> Self {
        match ext.to_ascii_lowercase().as_str() {
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "heic" | "bmp" | "tiff" | "tif"
            | "icns" | "psd" => Kind::Image,
            "mp4" | "mov" | "mkv" | "avi" | "webm" | "m4v" => Kind::Video,
            "mp3" | "wav" | "flac" | "aac" | "m4a" | "ogg" | "aiff" => Kind::Audio,
            "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "key" | "pages"
            | "numbers" | "rtf" => Kind::Doc,
            "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "go" | "c" | "h" | "cpp" | "hpp"
            | "java" | "kt" | "swift" | "rb" | "sh" | "zsh" | "toml" | "yaml" | "yml" | "json"
            | "html" | "css" | "scss" | "lua" | "sql" | "wgsl" | "glsl" => Kind::Code,
            "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "rar" | "dmg" | "pkg" | "iso"
            | "crate" => Kind::Archive,
            "txt" | "md" | "markdown" | "log" | "csv" | "tsv" | "xml" | "ini" | "cfg" | "conf"
            | "lock" | "plist" => Kind::Text,
            _ => Kind::Binary,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub kind: Kind,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub hidden: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub created: Option<SystemTime>,
    pub mode: u32,
}

impl Entry {
    /// Directories (and .app bundles) navigate; everything else opens with the OS.
    pub fn navigates(&self) -> bool {
        self.is_dir && self.kind != Kind::App
    }
}

pub struct DirListing {
    pub generation: u64,
    pub result: Result<Vec<Entry>, String>,
    pub elapsed_ms: f32,
}

pub struct Loader {
    tx: Sender<DirListing>,
    rx: Receiver<DirListing>,
    generation: u64,
}

impl Loader {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        Self {
            tx,
            rx,
            generation: 0,
        }
    }

    /// Kick off a background read. Returns the generation to match against.
    pub fn request(&mut self, path: PathBuf, ctx: egui::Context) -> u64 {
        self.generation += 1;
        let generation = self.generation;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let t0 = std::time::Instant::now();
            let result = read_dir(&path);
            let _ = tx.send(DirListing {
                generation,
                result,
                elapsed_ms: t0.elapsed().as_secs_f32() * 1000.0,
            });
            ctx.request_repaint();
        });
        generation
    }

    pub fn poll(&self) -> Option<DirListing> {
        self.rx.try_recv().ok()
    }

    pub fn current(&self) -> u64 {
        self.generation
    }
}

fn read_dir(path: &Path) -> Result<Vec<Entry>, String> {
    use std::os::unix::fs::PermissionsExt;
    let rd = std::fs::read_dir(path).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for item in rd {
        let Ok(item) = item else { continue };
        let name = item.file_name().to_string_lossy().into_owned();
        let p = item.path();
        let sym = item.file_type().map(|t| t.is_symlink()).unwrap_or(false);
        // follow symlinks for metadata; fall back to lstat if the target is gone
        let meta = std::fs::metadata(&p)
            .or_else(|_| std::fs::symlink_metadata(&p))
            .ok();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let ext = p
            .extension()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_default();
        let kind = if is_dir {
            if ext.eq_ignore_ascii_case("app") {
                Kind::App
            } else {
                Kind::Dir
            }
        } else if sym {
            Kind::Link
        } else if ext.is_empty() {
            Kind::Binary
        } else {
            Kind::from_ext(&ext)
        };
        out.push(Entry {
            hidden: name.starts_with('.'),
            name,
            path: p,
            kind,
            is_dir,
            is_symlink: sym,
            size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
            modified: meta.as_ref().and_then(|m| m.modified().ok()),
            created: meta.as_ref().and_then(|m| m.created().ok()),
            mode: meta.as_ref().map(|m| m.permissions().mode()).unwrap_or(0),
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Disk usage

#[derive(Clone, Copy, Debug)]
pub struct DiskInfo {
    pub total: u64,
    pub free: u64,
}

impl DiskInfo {
    pub fn used_fraction(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            1.0 - self.free as f32 / self.total as f32
        }
    }
}

pub fn disk_info(path: &Path) -> Option<DiskInfo> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `st` is a properly sized out-param.
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut st) };
    if rc != 0 {
        return None;
    }
    let frsize = st.f_frsize as u64;
    Some(DiskInfo {
        total: st.f_blocks as u64 * frsize,
        free: st.f_bavail as u64 * frsize,
    })
}

// ---------------------------------------------------------------------------
// Formatting helpers

/// Fixed-width size: `  12.4 MB`. Always 9 chars so the column never jitters.
pub fn fmt_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B ", "KB", "MB", "GB", "TB", "PB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{:>6} {}", bytes, UNITS[0])
    } else {
        format!("{:>6.1} {}", v, UNITS[u])
    }
}

pub fn fmt_time(t: Option<SystemTime>) -> String {
    match t {
        Some(t) => {
            let dt: chrono::DateTime<chrono::Local> = t.into();
            dt.format("%Y-%m-%d %H:%M").to_string()
        }
        None => "----------- --:--".into(),
    }
}

pub fn fmt_mode(mode: u32) -> String {
    let mut s = String::with_capacity(9);
    for shift in [6u32, 3, 0] {
        let bits = (mode >> shift) & 0o7;
        s.push(if bits & 4 != 0 { 'r' } else { '-' });
        s.push(if bits & 2 != 0 { 'w' } else { '-' });
        s.push(if bits & 1 != 0 { 'x' } else { '-' });
    }
    s
}

/// Well-known macOS locations for the sidebar.
pub fn places() -> Vec<(String, PathBuf)> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut v = Vec::new();
    if let Some(h) = &home {
        v.push(("Home".to_string(), h.clone()));
        for sub in ["Desktop", "Documents", "Downloads", "Pictures", "Projects"] {
            let p = h.join(sub);
            if p.is_dir() {
                v.push((sub.to_string(), p));
            }
        }
    }
    v.push(("Applications".into(), PathBuf::from("/Applications")));
    v.push(("Root".into(), PathBuf::from("/")));
    v
}

pub fn volumes() -> Vec<(String, PathBuf)> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/Volumes") {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                v.push((e.file_name().to_string_lossy().into_owned(), p));
            }
        }
    }
    v.sort();
    v
}

// ---------------------------------------------------------------------------
// Mutating operations (rename / trash / delete)

/// Rename `path` in place. Rejects empty names, path separators and existing targets.
pub fn rename(path: &Path, new_name: &str) -> Result<PathBuf, String> {
    let new_name = new_name.trim();
    if new_name.is_empty() {
        return Err("name is empty".into());
    }
    if new_name.contains('/') || new_name == "." || new_name == ".." {
        return Err("name contains '/' or is reserved".into());
    }
    let Some(parent) = path.parent() else {
        return Err("cannot rename the root".into());
    };
    let target = parent.join(new_name);
    if target == path {
        return Err("name unchanged".into());
    }
    if target.exists() || std::fs::symlink_metadata(&target).is_ok() {
        return Err(format!("'{new_name}' already exists"));
    }
    std::fs::rename(path, &target).map_err(|e| e.to_string())?;
    Ok(target)
}

/// Move `src` into the directory `dest_dir`, keeping its name. Falls back to copy + delete
/// when the destination is on another volume (`EXDEV`). Rejects self-moves and collisions.
pub fn move_into(src: &Path, dest_dir: &Path) -> Result<PathBuf, String> {
    let Some(name) = src.file_name() else {
        return Err("cannot move the root".into());
    };
    if src.parent() == Some(dest_dir) {
        return Err("already in this directory".into());
    }
    if dest_dir.starts_with(src) {
        return Err("cannot move a directory into itself".into());
    }
    let target = dest_dir.join(name);
    if target.exists() || std::fs::symlink_metadata(&target).is_ok() {
        return Err(format!("'{}' already exists here", name.to_string_lossy()));
    }
    match std::fs::rename(src, &target) {
        Ok(()) => Ok(target),
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            copy_recursive(src, &target).map_err(|e| e.to_string())?;
            remove(src)?;
            Ok(target)
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Copy a tree preserving symlinks (as links, never followed).
fn copy_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)?;
    } else if meta.is_dir() {
        std::fs::create_dir(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_recursive(&e.path(), &dst.join(e.file_name()))?;
        }
    } else {
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

/// Move to the macOS Trash (`NSFileManager.trashItem`, via the `trash` crate). Undoable from the
/// Finder. No Apple Events / Automation permission needed. Callers run it on a thread.
pub fn trash(path: &Path) -> Result<(), String> {
    use trash::macos::{DeleteMethod, TrashContextExtMacos};
    let mut ctx = trash::TrashContext::new();
    ctx.set_delete_method(DeleteMethod::NsFileManager); // no Finder / Apple Events involved
    ctx.delete(path).map_err(|e| e.to_string())
}

/// Permanent removal. Symlinks are unlinked (never followed); directories are removed recursively.
pub fn remove(path: &Path) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    let r = if meta.file_type().is_symlink() || !meta.is_dir() {
        std::fs::remove_file(path)
    } else {
        std::fs::remove_dir_all(path)
    };
    r.map_err(|e| e.to_string())
}

/// Result of a background operation, delivered through [`OpChannel`].
pub struct OpResult {
    pub label: String,
    pub result: Result<(), String>,
}

pub struct OpChannel {
    tx: Sender<OpResult>,
    rx: Receiver<OpResult>,
    in_flight: usize,
}

impl OpChannel {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        Self {
            tx,
            rx,
            in_flight: 0,
        }
    }

    pub fn spawn(
        &mut self,
        label: String,
        ctx: egui::Context,
        op: impl FnOnce() -> Result<(), String> + Send + 'static,
    ) {
        self.in_flight += 1;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = op();
            let _ = tx.send(OpResult { label, result });
            ctx.request_repaint();
        });
    }

    pub fn poll(&mut self) -> Option<OpResult> {
        let r = self.rx.try_recv().ok();
        if r.is_some() {
            self.in_flight -= 1;
        }
        r
    }

    pub fn busy(&self) -> bool {
        self.in_flight > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fuide-file-manager-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn rename_moves_file_and_rejects_bad_names() {
        let dir = scratch("rename");
        let a = dir.join("a.txt");
        std::fs::write(&a, "x").unwrap();
        std::fs::write(dir.join("taken.txt"), "y").unwrap();

        assert!(rename(&a, "").is_err());
        assert!(rename(&a, "x/y").is_err());
        assert!(rename(&a, "..").is_err());
        assert!(rename(&a, "a.txt").is_err(), "unchanged name is rejected");
        assert!(
            rename(&a, "taken.txt").is_err(),
            "existing target is rejected"
        );

        let b = rename(&a, "  b.txt ").unwrap();
        assert_eq!(b, dir.join("b.txt"));
        assert!(!a.exists() && b.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_into_relocates_and_rejects_noops_collisions_and_cycles() {
        let dir = scratch("move");
        let sub = dir.join("sub");
        std::fs::create_dir(&sub).unwrap();
        let a = dir.join("a.txt");
        std::fs::write(&a, "x").unwrap();
        std::fs::write(sub.join("taken.txt"), "y").unwrap();

        assert!(move_into(&a, &dir).is_err(), "same parent is a no-op");
        assert!(
            move_into(&sub, &sub).is_err(),
            "directory into itself is rejected"
        );
        std::fs::write(sub.join("a.txt"), "z").unwrap();
        assert!(move_into(&a, &sub).is_err(), "collision is rejected");
        std::fs::remove_file(sub.join("a.txt")).unwrap();

        let target = move_into(&a, &sub).unwrap();
        assert_eq!(target, sub.join("a.txt"));
        assert!(!a.exists() && target.exists());

        // a directory moves with its contents
        let deep = dir.join("deep");
        std::fs::create_dir(&deep).unwrap();
        std::fs::write(deep.join("k.txt"), "k").unwrap();
        move_into(&deep, &sub).unwrap();
        assert!(sub.join("deep/k.txt").exists() && !deep.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_recursive_preserves_trees_and_symlinks() {
        let dir = scratch("copytree");
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("inner")).unwrap();
        std::fs::write(src.join("inner/f.txt"), "f").unwrap();
        std::os::unix::fs::symlink("inner/f.txt", src.join("ln")).unwrap();

        let dst = dir.join("dst");
        copy_recursive(&src, &dst).unwrap();
        assert_eq!(
            std::fs::read_to_string(dst.join("inner/f.txt")).unwrap(),
            "f"
        );
        let meta = std::fs::symlink_metadata(dst.join("ln")).unwrap();
        assert!(meta.file_type().is_symlink(), "symlink copied as a link");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_unlinks_symlink_without_following_it() {
        let dir = scratch("remove");
        let target = dir.join("target");
        std::fs::create_dir_all(target.join("inner")).unwrap();
        std::fs::write(target.join("inner/keep.txt"), "k").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        remove(&link).unwrap();
        assert!(std::fs::symlink_metadata(&link).is_err(), "link is gone");
        assert!(target.join("inner/keep.txt").exists(), "target untouched");

        remove(&target).unwrap();
        assert!(!target.exists());
        assert!(remove(&target).is_err(), "missing path reports an error");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod trash_tests {
    /// Talks to the real Finder; run explicitly with `cargo test -- --ignored trash`.
    #[test]
    #[ignore]
    fn trash_moves_file_to_finder_trash() {
        let dir =
            std::env::temp_dir().join(format!("fuide-file-manager-trash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("fui \"probe\" 2026.txt");
        std::fs::write(&f, "probe").unwrap();
        super::trash(&f).unwrap();
        assert!(!f.exists(), "file left the source directory");
        // ~/.Trash is TCC-protected; only verify its contents when this process may read it.
        let home = std::env::var("HOME").unwrap();
        if let Ok(rd) = std::fs::read_dir(format!("{home}/.Trash")) {
            let in_trash = rd.flatten().any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("fui \"probe\" 2026")
            });
            assert!(in_trash, "file is in ~/.Trash");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
