//! System telemetry: what the monitor shows, where it comes from, and the thread that samples it.
//!
//! The UI never talks to the OS directly. A [`Source`] produces [`Snapshot`]s; the real one
//! (`mac`) reads libproc / Mach / sysctl / IOKit and the two setuid tools (`ps`, `nettop`) that
//! cover what a plain user cannot read, and the fake one (`fake`) feeds tests and screenshots.
//! [`Sampler`] runs a source on its own thread at the configured interval.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub mod fake;
#[cfg(target_os = "macos")]
pub mod iokit;
#[cfg(target_os = "macos")]
pub mod mac;

/// One reading of the whole machine. Rates are per second over `interval`.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// Seconds since the previous snapshot (0 for the first one: no rates yet).
    pub interval: f32,
    /// Unix time of the sample (process ages are measured against it).
    pub now: i64,
    pub host: Host,
    pub cpu: Cpu,
    pub mem: Mem,
    pub disk: Disk,
    pub net: Net,
    pub power: Power,
    pub procs: Vec<Proc>,
}

#[derive(Clone, Debug, Default)]
pub struct Host {
    pub model: String,
    pub os: String,
    pub hostname: String,
    /// Seconds since boot.
    pub uptime: f64,
    pub cores: u32,
    pub p_cores: u32,
    pub e_cores: u32,
    pub mem_total: u64,
    /// The user running the monitor (whose processes are fully readable).
    pub uid: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CoreKind {
    #[default]
    Standard,
    Performance,
    Efficiency,
}

#[derive(Clone, Debug, Default)]
pub struct Core {
    pub kind: CoreKind,
    /// 0..1, user + system.
    pub usage: f32,
}

#[derive(Clone, Debug, Default)]
pub struct Cpu {
    /// Fractions 0..1 of all cores.
    pub user: f32,
    pub system: f32,
    pub idle: f32,
    pub cores: Vec<Core>,
    pub load: [f32; 3],
    /// GPU device utilisation 0..1 when the driver reports it.
    pub gpu: Option<f32>,
    pub processes: u32,
    /// Threads of the processes we could read (the rest are not visible without root).
    pub threads: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Pressure {
    #[default]
    Normal,
    Warn,
    Critical,
}

#[derive(Clone, Debug, Default)]
pub struct Mem {
    pub total: u64,
    /// app + wired + compressed (what Activity Monitor calls "Memory Used").
    pub used: u64,
    pub app: u64,
    pub wired: u64,
    pub compressed: u64,
    pub cached: u64,
    pub swap_used: u64,
    pub swap_total: u64,
    pub pressure: Pressure,
}

#[derive(Clone, Debug, Default)]
pub struct Disk {
    pub read_bps: f64,
    pub write_bps: f64,
    pub read_ops: f64,
    pub write_ops: f64,
    pub read_total: u64,
    pub write_total: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Net {
    pub in_bps: f64,
    pub out_bps: f64,
    pub in_pps: f64,
    pub out_pps: f64,
    pub in_total: u64,
    pub out_total: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Battery {
    /// 0..100.
    pub percent: f32,
    pub charging: bool,
    /// Minutes to empty (on battery) or to full (charging), when the OS knows.
    pub minutes: Option<u32>,
}

#[derive(Clone, Debug, Default)]
pub struct Power {
    pub on_ac: bool,
    /// None on a desktop.
    pub battery: Option<Battery>,
    /// Processes holding a sleep-preventing power assertion: (pid, assertion type).
    pub sleep_blockers: Vec<(i32, String)>,
}

/// How much of a process the monitor could read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Access {
    /// libproc answered: exact interval CPU, footprint, threads, wakeups, disk bytes.
    #[default]
    Full,
    /// Another user's process: only what setuid `ps` reports (kernel-averaged CPU, RSS).
    Limited,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum State {
    Running,
    #[default]
    Sleeping,
    Idle,
    Stopped,
    Zombie,
    Unknown,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Running => "RUN",
            State::Sleeping => "SLEEP",
            State::Idle => "IDLE",
            State::Stopped => "STOP",
            State::Zombie => "ZOMBIE",
            State::Unknown => "--",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub uid: u32,
    pub user: String,
    pub name: String,
    pub path: Option<String>,
    /// Full command line when readable (own processes).
    pub cmdline: Option<String>,
    pub access: Access,
    pub state: State,
    /// Percent of one core over the interval (Full) or the kernel's decaying average (Limited).
    pub cpu: f32,
    /// Seconds of CPU since start (libproc for own processes, `ps time` for the rest).
    pub cpu_time: f64,
    pub threads: u32,
    /// Idle wakeups per second (Full only).
    pub wakeups: f32,
    /// Physical footprint (Full) or RSS (Limited): the "memory" column.
    pub mem: u64,
    pub rss: u64,
    pub vsz: u64,
    pub nice: i32,
    /// Unix time the process started, when known.
    pub started: Option<i64>,
    pub disk_read: u64,
    pub disk_write: u64,
    pub disk_read_bps: f64,
    pub disk_write_bps: f64,
    pub net_in: u64,
    pub net_out: u64,
    pub net_in_bps: f64,
    pub net_out_bps: f64,
    /// Estimated energy impact (unitless, see `energy_estimate`). Not Apple's number.
    pub energy: f32,
    pub prevents_sleep: bool,
}

/// Energy impact estimate. Apple's formula is private; this weighs the things it is known to
/// weigh (CPU time, idle wakeups, disk and network traffic) so the ranking is useful even if
/// the scale is not Activity Monitor's.
pub fn energy_estimate(cpu_pct: f32, wakeups_per_s: f32, disk_bps: f64, net_bps: f64) -> f32 {
    let disk_mb = (disk_bps / 1_048_576.0) as f32;
    let net_mb = (net_bps / 1_048_576.0) as f32;
    cpu_pct + wakeups_per_s * 0.05 + disk_mb * 1.5 + net_mb * 1.0
}

/// Outcome of a kill request, for the log and the error card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KillOutcome {
    Sent,
    NotPermitted,
    NoSuchProcess,
    Failed(String),
}

/// Where snapshots come from. Implementations run on the sampler thread.
pub trait Source: Send {
    fn sample(&mut self) -> Snapshot;
    /// SIGTERM (`force` = false) or SIGKILL. The fake source records the request.
    fn kill(&mut self, pid: i32, force: bool) -> KillOutcome;
}

/// Runs a [`Source`] on a thread; the UI drains `recv` each frame.
pub struct Sampler {
    rx: Receiver<Snapshot>,
    interval: Arc<Mutex<Duration>>,
    poke: Sender<Cmd>,
}

enum Cmd {
    Kill {
        pid: i32,
        force: bool,
        reply: Sender<KillOutcome>,
    },
    Now,
}

impl Sampler {
    /// `wake` is called after each snapshot (the UI passes `ctx.request_repaint`).
    pub fn spawn(
        mut source: Box<dyn Source>,
        interval: Duration,
        wake: impl Fn() + Send + 'static,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let (poke, cmds) = mpsc::channel::<Cmd>();
        let shared = Arc::new(Mutex::new(interval));
        let period = Arc::clone(&shared);
        thread::Builder::new()
            .name("sampler".into())
            .spawn(move || {
                // first snapshot right away (it has no rates), then one per interval
                let mut next = Instant::now();
                loop {
                    let now = Instant::now();
                    if now >= next {
                        let snap = source.sample();
                        if tx.send(snap).is_err() {
                            return;
                        }
                        wake();
                        let dt = *period.lock().unwrap();
                        next = now + dt;
                    }
                    let wait = next.saturating_duration_since(Instant::now());
                    match cmds.recv_timeout(wait) {
                        Ok(Cmd::Kill { pid, force, reply }) => {
                            let _ = reply.send(source.kill(pid, force));
                            next = Instant::now(); // show the effect without waiting a period
                        }
                        Ok(Cmd::Now) => next = Instant::now(),
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
            })
            .expect("sampler thread");
        Self {
            rx,
            interval: shared,
            poke,
        }
    }

    /// Latest snapshot if one arrived since the last call (older ones are dropped).
    pub fn poll(&self) -> Option<Snapshot> {
        let mut last = None;
        while let Ok(s) = self.rx.try_recv() {
            last = Some(s);
        }
        last
    }

    pub fn set_interval(&self, interval: Duration) {
        *self.interval.lock().unwrap() = interval;
        let _ = self.poke.send(Cmd::Now);
    }

    /// Blocks until the sampler thread has signalled the process (fast: one syscall).
    pub fn kill(&self, pid: i32, force: bool) -> KillOutcome {
        let (reply, done) = mpsc::channel();
        if self.poke.send(Cmd::Kill { pid, force, reply }).is_err() {
            return KillOutcome::Failed("sampler stopped".into());
        }
        done.recv_timeout(Duration::from_secs(2))
            .unwrap_or_else(|_| KillOutcome::Failed("no answer from sampler".into()))
    }
}

/// The real source for this platform.
#[cfg(target_os = "macos")]
pub fn native() -> Box<dyn Source> {
    Box::new(mac::MacSource::new())
}

#[cfg(not(target_os = "macos"))]
pub fn native() -> Box<dyn Source> {
    Box::new(fake::FakeSource::new())
}
