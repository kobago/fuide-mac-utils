//! Homebrew access: inventory via `brew info --json=v2`, search, streaming command runner.
//!
//! Read-only queries run with `HOMEBREW_NO_AUTO_UPDATE=1` (otherwise `brew` may spend seconds
//! auto-updating and print hints). Mutating commands (`update`, `upgrade`, `install`,
//! `uninstall`, `pin`) stream their stdout/stderr line by line so the UI can log them live.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::SystemTime;

use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Formula,
    Cask,
}

impl Kind {
    pub fn tag(self) -> &'static str {
        match self {
            Kind::Formula => "FORMULA",
            Kind::Cask => "CASK",
        }
    }
    pub fn flag(self) -> &'static str {
        match self {
            Kind::Formula => "--formula",
            Kind::Cask => "--cask",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Package {
    pub name: String,
    pub kind: Kind,
    pub desc: String,
    pub homepage: String,
    pub tap: String,
    pub license: Option<String>,
    /// Installed version(s); empty when not installed (search results).
    pub installed: Vec<String>,
    pub latest: String,
    pub outdated: bool,
    pub pinned: bool,
    pub on_request: bool,
    pub as_dependency: bool,
    pub deps: Vec<String>,
    pub caveats: Option<String>,
    pub installed_time: Option<SystemTime>,
    pub auto_updates: bool,
    pub deprecated: bool,
}

impl Package {
    pub fn is_installed(&self) -> bool {
        !self.installed.is_empty()
    }
    pub fn installed_version(&self) -> &str {
        self.installed.last().map(String::as_str).unwrap_or("--")
    }
    pub fn status(&self) -> Status {
        if !self.is_installed() {
            Status::Available
        } else if self.pinned {
            Status::Pinned
        } else if self.outdated {
            Status::Outdated
        } else if self.as_dependency && !self.on_request {
            Status::Dependency
        } else {
            Status::Current
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    Outdated,
    Pinned,
    Current,
    Dependency,
    Available,
}

impl Status {
    pub fn tag(self) -> &'static str {
        match self {
            Status::Outdated => "OUTDATED",
            Status::Pinned => "PINNED",
            Status::Current => "CURRENT",
            Status::Dependency => "DEP",
            Status::Available => "AVAILABLE",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct SystemInfo {
    pub version: String,
    pub prefix: PathBuf,
    pub cellar_kb: Option<u64>,
    pub caskroom_kb: Option<u64>,
    pub last_update: Option<SystemTime>,
}

pub enum Msg {
    /// One line of output from a streaming command.
    Line {
        text: String,
        stderr: bool,
    },
    /// A streaming command finished.
    Exit {
        label: String,
        args: Vec<String>,
        ok: bool,
        code: Option<i32>,
        elapsed_ms: f32,
    },
    Inventory(Result<Vec<Package>, String>, f32),
    Search {
        query: String,
        result: Result<Vec<Package>, String>,
    },
    System(SystemInfo),
}

pub struct Brew {
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    /// Label of the streaming command in flight (only one at a time).
    running: Option<String>,
    fetching: bool,
}

/// Locate `brew`. `FUIDE_BREW_BIN` overrides it (tests point it at `fixtures/fake-brew.sh`).
/// Apps launched from Spotlight / Finder get a minimal `PATH` (`/usr/bin:/bin:/usr/sbin:/sbin`),
/// so the well-known prefixes are tried first.
pub fn brew_executable() -> PathBuf {
    if let Some(p) = std::env::var_os("FUIDE_BREW_BIN") {
        return PathBuf::from(p);
    }
    for p in [
        "/opt/homebrew/bin/brew",
        "/usr/local/bin/brew",
        "/home/linuxbrew/.linuxbrew/bin/brew",
    ] {
        if std::path::Path::new(p).is_file() {
            return PathBuf::from(p);
        }
    }
    PathBuf::from("brew") // fall back to PATH lookup
}

/// `PATH` with the brew bin directory prepended, so brew's own helpers (`git`, `curl`) resolve
/// even when the app was started by launchd.
fn augmented_path() -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(dir) = brew_executable().parent() {
        if dir.is_absolute() {
            parts.push(dir.to_string_lossy().into_owned());
        }
    }
    parts.extend(
        std::env::var("PATH")
            .unwrap_or_else(|_| "/usr/bin:/bin:/usr/sbin:/sbin".into())
            .split(':')
            .filter(|s| !s.is_empty())
            .map(String::from),
    );
    parts.dedup();
    parts.join(":")
}

fn base_command(read_only: bool) -> Command {
    let mut c = Command::new(brew_executable());
    c.env("PATH", augmented_path())
        .env("HOMEBREW_NO_COLOR", "1")
        .env("HOMEBREW_NO_EMOJI", "1")
        .env("HOMEBREW_NO_ENV_HINTS", "1")
        .env("NONINTERACTIVE", "1")
        .stdin(Stdio::null());
    if read_only {
        c.env("HOMEBREW_NO_AUTO_UPDATE", "1");
    }
    c
}

impl Brew {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        Self {
            tx,
            rx,
            running: None,
            fetching: false,
        }
    }

    pub fn running(&self) -> Option<&str> {
        self.running.as_deref()
    }

    /// Feed a message as if a worker had produced it (unit tests drive the app without brew).
    #[cfg(test)]
    pub fn inject(&self, msg: Msg) {
        let _ = self.tx.send(msg);
    }

    pub fn fetching(&self) -> bool {
        self.fetching
    }

    pub fn poll(&mut self) -> Vec<Msg> {
        let mut out = Vec::new();
        while let Ok(m) = self.rx.try_recv() {
            match &m {
                Msg::Exit { .. } => self.running = None,
                Msg::Inventory(..) => self.fetching = false,
                _ => {}
            }
            out.push(m);
        }
        out
    }

    /// `brew info --json=v2 --installed` → packages.
    pub fn fetch_inventory(&mut self, ctx: egui::Context) {
        if self.fetching {
            return;
        }
        self.fetching = true;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let t0 = std::time::Instant::now();
            let result = base_command(true)
                .args(["info", "--json=v2", "--installed"])
                .output()
                .map_err(|e| e.to_string())
                .and_then(|o| {
                    if o.status.success() {
                        parse_info_json(&o.stdout)
                    } else {
                        Err(String::from_utf8_lossy(&o.stderr).trim().to_string())
                    }
                });
            let _ = tx.send(Msg::Inventory(result, t0.elapsed().as_secs_f32() * 1000.0));
            ctx.request_repaint();
        });
    }

    /// `brew --version`, `brew --prefix`, Cellar/Caskroom size, last `brew update` time.
    pub fn fetch_system(&self, ctx: egui::Context) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let mut info = SystemInfo::default();
            if let Ok(o) = base_command(true).arg("--version").output() {
                info.version = String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .trim_start_matches("Homebrew ")
                    .to_string();
            }
            if let Ok(o) = base_command(true).arg("--prefix").output() {
                info.prefix = PathBuf::from(String::from_utf8_lossy(&o.stdout).trim());
            }
            if let Ok(o) = base_command(true).arg("--repository").output() {
                let repo = PathBuf::from(String::from_utf8_lossy(&o.stdout).trim());
                info.last_update = std::fs::metadata(repo.join(".git/FETCH_HEAD"))
                    .and_then(|m| m.modified())
                    .ok();
            }
            info.cellar_kb = du_kb(&info.prefix.join("Cellar"));
            info.caskroom_kb = du_kb(&info.prefix.join("Caskroom"));
            let _ = tx.send(Msg::System(info));
            ctx.request_repaint();
        });
    }

    /// `brew search` for formulae and casks, then `brew info` on the first hits for details.
    pub fn search(&self, query: String, ctx: egui::Context) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = (|| -> Result<Vec<Package>, String> {
                let mut pkgs = Vec::new();
                for kind in [Kind::Formula, Kind::Cask] {
                    let o = base_command(true)
                        .args(["search", kind.flag(), &query])
                        .output()
                        .map_err(|e| e.to_string())?;
                    let names: Vec<String> = String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .map(str::trim)
                        .filter(|l| !l.is_empty() && !l.starts_with("==>"))
                        .take(25)
                        .map(String::from)
                        .collect();
                    if names.is_empty() {
                        continue;
                    }
                    let o = base_command(true)
                        .args(["info", "--json=v2", kind.flag()])
                        .args(&names)
                        .output()
                        .map_err(|e| e.to_string())?;
                    if o.status.success() {
                        pkgs.extend(parse_info_json(&o.stdout)?);
                    } else {
                        // some names may be unavailable; fall back to bare names
                        pkgs.extend(names.into_iter().map(|n| bare_package(n, kind)));
                    }
                }
                Ok(pkgs)
            })();
            let _ = tx.send(Msg::Search { query, result });
            ctx.request_repaint();
        });
    }

    /// Run a mutating brew command, streaming output. Returns false if one is already running.
    pub fn run(&mut self, label: String, args: Vec<String>, ctx: egui::Context) -> bool {
        if self.running.is_some() {
            return false;
        }
        self.running = Some(label.clone());
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let t0 = std::time::Instant::now();
            let child = base_command(false)
                .args(&args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn();
            let mut child = match child {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(Msg::Line {
                        text: format!("cannot start brew: {e}"),
                        stderr: true,
                    });
                    let _ = tx.send(Msg::Exit {
                        label,
                        args,
                        ok: false,
                        code: None,
                        elapsed_ms: 0.0,
                    });
                    ctx.request_repaint();
                    return;
                }
            };
            let mut readers = Vec::new();
            for (stream, is_err) in [
                (
                    child
                        .stdout
                        .take()
                        .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
                    false,
                ),
                (
                    child
                        .stderr
                        .take()
                        .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
                    true,
                ),
            ] {
                let Some(stream) = stream else { continue };
                let tx = tx.clone();
                let ctx = ctx.clone();
                readers.push(std::thread::spawn(move || {
                    for line in BufReader::new(stream).lines().map_while(Result::ok) {
                        let text = line.trim_end().to_string();
                        if text.is_empty() {
                            continue;
                        }
                        let _ = tx.send(Msg::Line {
                            text,
                            stderr: is_err,
                        });
                        ctx.request_repaint();
                    }
                }));
            }
            let status = child.wait();
            for r in readers {
                let _ = r.join();
            }
            let (ok, code) = match status {
                Ok(s) => (s.success(), s.code()),
                Err(_) => (false, None),
            };
            let _ = tx.send(Msg::Exit {
                label,
                args,
                ok,
                code,
                elapsed_ms: t0.elapsed().as_secs_f32() * 1000.0,
            });
            ctx.request_repaint();
        });
        true
    }
}

fn du_kb(path: &std::path::Path) -> Option<u64> {
    let o = Command::new("du").arg("-sk").arg(path).output().ok()?;
    String::from_utf8_lossy(&o.stdout)
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn bare_package(name: String, kind: Kind) -> Package {
    Package {
        name,
        kind,
        desc: String::new(),
        homepage: String::new(),
        tap: String::new(),
        license: None,
        installed: Vec::new(),
        latest: String::new(),
        outdated: false,
        pinned: false,
        on_request: false,
        as_dependency: false,
        deps: Vec::new(),
        caveats: None,
        installed_time: None,
        auto_updates: false,
        deprecated: false,
    }
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

/// Parse `brew info --json=v2` output (both `formulae` and `casks` arrays).
pub fn parse_info_json(bytes: &[u8]) -> Result<Vec<Package>, String> {
    let root: Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for f in root["formulae"].as_array().into_iter().flatten() {
        let inst = f["installed"].as_array().cloned().unwrap_or_default();
        let last = inst.last().cloned().unwrap_or(Value::Null);
        out.push(Package {
            name: s(&f["name"]),
            kind: Kind::Formula,
            desc: s(&f["desc"]),
            homepage: s(&f["homepage"]),
            tap: s(&f["tap"]),
            license: f["license"].as_str().map(String::from),
            installed: inst.iter().map(|i| s(&i["version"])).collect(),
            latest: s(&f["versions"]["stable"]),
            outdated: f["outdated"].as_bool().unwrap_or(false),
            pinned: f["pinned"].as_bool().unwrap_or(false),
            on_request: last["installed_on_request"].as_bool().unwrap_or(false),
            as_dependency: last["installed_as_dependency"].as_bool().unwrap_or(false),
            deps: f["dependencies"]
                .as_array()
                .into_iter()
                .flatten()
                .map(s)
                .collect(),
            caveats: f["caveats"].as_str().map(String::from),
            installed_time: last["time"]
                .as_u64()
                .map(|t| SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(t)),
            auto_updates: false,
            deprecated: f["deprecated"].as_bool().unwrap_or(false),
        });
    }
    for c in root["casks"].as_array().into_iter().flatten() {
        let installed = c["installed"].as_str().map(String::from);
        let name = if let Some(n) = c["name"].as_array().and_then(|a| a.first()) {
            s(n)
        } else {
            String::new()
        };
        out.push(Package {
            name: s(&c["token"]),
            kind: Kind::Cask,
            desc: if s(&c["desc"]).is_empty() {
                name
            } else {
                s(&c["desc"])
            },
            homepage: s(&c["homepage"]),
            tap: s(&c["tap"]),
            license: None,
            installed: installed.into_iter().collect(),
            latest: s(&c["version"]),
            outdated: c["outdated"].as_bool().unwrap_or(false),
            pinned: c["pinned"].as_bool().unwrap_or(false),
            on_request: true,
            as_dependency: false,
            deps: Vec::new(),
            caveats: c["caveats"].as_str().map(String::from),
            installed_time: c["installed_time"]
                .as_u64()
                .map(|t| SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(t)),
            auto_updates: c["auto_updates"].as_bool().unwrap_or(false),
            deprecated: c["deprecated"].as_bool().unwrap_or(false),
        });
    }
    Ok(out)
}

/// Fixed-width size from kilobytes: `  1.3 GB`.
pub fn fmt_kb(kb: u64) -> String {
    let mut v = kb as f64;
    let units = ["KB", "MB", "GB", "TB"];
    let mut u = 0;
    while v >= 1000.0 && u < units.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    format!("{:>6.1} {}", v, units[u])
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_formula_and_cask() {
        let json = br#"{"formulae":[{"name":"ripgrep","desc":"Search tool","homepage":"https://x","tap":"homebrew/core","license":"MIT","outdated":true,"pinned":false,"installed":[{"version":"14.0.0","installed_on_request":true,"installed_as_dependency":false,"time":1700000000}],"versions":{"stable":"14.1.0"},"dependencies":["pcre2"]}],"casks":[{"token":"iterm2","name":["iTerm2"],"desc":"Terminal","homepage":"https://y","tap":"homebrew/cask","version":"3.5","installed":"3.4","outdated":true,"auto_updates":true}]}"#;
        let p = parse_info_json(json).unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].name, "ripgrep");
        assert_eq!(p[0].installed_version(), "14.0.0");
        assert_eq!(p[0].latest, "14.1.0");
        assert_eq!(p[0].status(), Status::Outdated);
        assert_eq!(p[0].deps, vec!["pcre2"]);
        assert_eq!(p[1].kind, Kind::Cask);
        assert_eq!(p[1].installed_version(), "3.4");
        assert!(p[1].auto_updates);
    }

    #[test]
    fn status_precedence() {
        let mut p = bare_package("x".into(), Kind::Formula);
        assert_eq!(p.status(), Status::Available);
        p.installed = vec!["1".into()];
        assert_eq!(p.status(), Status::Current);
        p.as_dependency = true;
        assert_eq!(p.status(), Status::Dependency);
        p.outdated = true;
        assert_eq!(p.status(), Status::Outdated);
        p.pinned = true;
        assert_eq!(p.status(), Status::Pinned);
    }

    #[test]
    fn sizes() {
        assert_eq!(fmt_kb(512), " 512.0 KB");
        assert_eq!(fmt_kb(1352252), "   1.3 GB");
    }
}
