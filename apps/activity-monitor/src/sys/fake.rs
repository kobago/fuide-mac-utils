//! Deterministic source for tests and screenshots: a fixed machine with a handful of processes
//! whose numbers drift a little each sample so graphs have a shape.

use super::{
    energy_estimate, Access, Battery, Core, CoreKind, Cpu, Disk, Host, KillOutcome, Mem, Net,
    Power, Pressure, Proc, Snapshot, Source, State,
};

const GB: u64 = 1 << 30;
/// The fake machine booted at this unix time; processes start a little after.
pub const FAKE_EPOCH: i64 = 1_784_174_625;
const MB: u64 = 1 << 20;

pub struct FakeSource {
    tick: u32,
    /// Every kill request in order: (pid, force).
    pub killed: Vec<(i32, bool)>,
    /// Pids that answer `EPERM` to kill (someone else's).
    pub protected: Vec<i32>,
    /// Pids removed from the list after a kill.
    gone: Vec<i32>,
}

impl Default for FakeSource {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeSource {
    pub fn new() -> Self {
        Self {
            tick: 0,
            killed: Vec::new(),
            protected: vec![1, 88],
            gone: Vec::new(),
        }
    }

    fn wave(&self, phase: f32, amp: f32) -> f32 {
        ((self.tick as f32 * 0.7 + phase).sin() * 0.5 + 0.5) * amp
    }
}

struct Seed {
    pid: i32,
    ppid: i32,
    uid: u32,
    name: &'static str,
    path: &'static str,
    cpu: f32,
    mem: u64,
    threads: u32,
    disk_bps: f64,
    net_bps: f64,
    wakeups: f32,
    sleep: bool,
}

const SEEDS: &[Seed] = &[
    Seed {
        pid: 1,
        ppid: 0,
        uid: 0,
        name: "launchd",
        path: "/sbin/launchd",
        cpu: 0.1,
        mem: 24 * MB,
        threads: 0,
        disk_bps: 0.0,
        net_bps: 0.0,
        wakeups: 0.0,
        sleep: false,
    },
    Seed {
        pid: 88,
        ppid: 1,
        uid: 0,
        name: "kernel_task",
        path: "",
        cpu: 6.0,
        mem: 900 * MB,
        threads: 0,
        disk_bps: 0.0,
        net_bps: 0.0,
        wakeups: 0.0,
        sleep: false,
    },
    Seed {
        pid: 402,
        ppid: 1,
        uid: 501,
        name: "WindowServer",
        path: "/System/Library/PrivateFrameworks/SkyLight.framework/Resources/WindowServer",
        cpu: 9.0,
        mem: 620 * MB,
        threads: 22,
        disk_bps: 0.0,
        net_bps: 0.0,
        wakeups: 120.0,
        sleep: false,
    },
    Seed {
        pid: 1201,
        ppid: 1,
        uid: 501,
        name: "Safari",
        path: "/Applications/Safari.app/Contents/MacOS/Safari",
        cpu: 14.0,
        mem: 1200 * MB,
        threads: 41,
        disk_bps: 2.0e6,
        net_bps: 3.2e6,
        wakeups: 40.0,
        sleep: false,
    },
    Seed {
        pid: 1340,
        ppid: 1,
        uid: 501,
        name: "Music",
        path: "/System/Applications/Music.app/Contents/MacOS/Music",
        cpu: 3.0,
        mem: 380 * MB,
        threads: 18,
        disk_bps: 0.4e6,
        net_bps: 0.3e6,
        wakeups: 15.0,
        sleep: true,
    },
    Seed {
        pid: 2022,
        ppid: 1,
        uid: 501,
        name: "cargo",
        path: "/Users/dev/.cargo/bin/cargo",
        cpu: 78.0,
        mem: 2200 * MB,
        threads: 12,
        disk_bps: 42.0e6,
        net_bps: 0.0,
        wakeups: 3.0,
        sleep: false,
    },
    Seed {
        pid: 2023,
        ppid: 2022,
        uid: 501,
        name: "rustc",
        path: "/Users/dev/.rustup/toolchains/stable/bin/rustc",
        cpu: 190.0,
        mem: 3100 * MB,
        threads: 10,
        disk_bps: 8.0e6,
        net_bps: 0.0,
        wakeups: 1.0,
        sleep: false,
    },
    Seed {
        pid: 2310,
        ppid: 1,
        uid: 501,
        name: "Terminal",
        path: "/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal",
        cpu: 1.2,
        mem: 140 * MB,
        threads: 9,
        disk_bps: 0.0,
        net_bps: 0.0,
        wakeups: 8.0,
        sleep: false,
    },
    Seed {
        pid: 2400,
        ppid: 2310,
        uid: 501,
        name: "zsh",
        path: "/bin/zsh",
        cpu: 0.0,
        mem: 6 * MB,
        threads: 1,
        disk_bps: 0.0,
        net_bps: 0.0,
        wakeups: 0.0,
        sleep: false,
    },
    Seed {
        pid: 3001,
        ppid: 1,
        uid: 501,
        name: "FUIDE Activity Monitor",
        path: "/Applications/FUIDE Activity Monitor.app/Contents/MacOS/fuide-activity-monitor",
        cpu: 2.4,
        mem: 96 * MB,
        threads: 7,
        disk_bps: 0.0,
        net_bps: 0.0,
        wakeups: 20.0,
        sleep: false,
    },
];

impl Source for FakeSource {
    fn sample(&mut self) -> Snapshot {
        self.tick += 1;
        let interval = if self.tick == 1 { 0.0 } else { 1.0 };
        let cores: Vec<Core> = (0..10)
            .map(|i| Core {
                kind: if i < 4 {
                    CoreKind::Efficiency
                } else {
                    CoreKind::Performance
                },
                usage: (0.08 + self.wave(i as f32 * 0.9, 0.7) * if i < 4 { 0.5 } else { 1.0 })
                    .min(1.0),
            })
            .collect();
        let busy = cores.iter().map(|c| c.usage).sum::<f32>() / cores.len() as f32;
        let procs: Vec<Proc> = SEEDS
            .iter()
            .filter(|s| !self.gone.contains(&s.pid))
            .map(|s| {
                let own = s.uid == 501;
                let jitter = self.wave(s.pid as f32, 0.3) + 0.85;
                let cpu = s.cpu * jitter;
                let wakeups = s.wakeups * jitter;
                let disk = s.disk_bps * jitter as f64;
                let net = s.net_bps * jitter as f64;
                Proc {
                    pid: s.pid,
                    ppid: s.ppid,
                    uid: s.uid,
                    user: if own { "dev".into() } else { "root".into() },
                    name: s.name.into(),
                    path: (!s.path.is_empty()).then(|| s.path.to_string()),
                    cmdline: own.then(|| format!("{} --flag", s.path)),
                    access: if own { Access::Full } else { Access::Limited },
                    state: if cpu > 50.0 {
                        State::Running
                    } else {
                        State::Sleeping
                    },
                    cpu,
                    cpu_time: if own {
                        s.cpu as f64 * 60.0 + self.tick as f64
                    } else {
                        0.0
                    },
                    threads: s.threads,
                    wakeups,
                    mem: s.mem,
                    rss: s.mem,
                    vsz: s.mem * 30,
                    nice: 0,
                    started: Some(FAKE_EPOCH + s.pid as i64),
                    disk_read: (disk * 0.7) as u64 * self.tick as u64,
                    disk_write: (disk * 0.3) as u64 * self.tick as u64,
                    disk_read_bps: disk * 0.7,
                    disk_write_bps: disk * 0.3,
                    net_in: (net * 0.8) as u64 * self.tick as u64,
                    net_out: (net * 0.2) as u64 * self.tick as u64,
                    net_in_bps: net * 0.8,
                    net_out_bps: net * 0.2,
                    energy: energy_estimate(cpu, wakeups, disk, net),
                    prevents_sleep: s.sleep,
                }
            })
            .collect();
        let disk_r = procs.iter().map(|p| p.disk_read_bps).sum::<f64>();
        let disk_w = procs.iter().map(|p| p.disk_write_bps).sum::<f64>();
        let net_i = procs.iter().map(|p| p.net_in_bps).sum::<f64>();
        let net_o = procs.iter().map(|p| p.net_out_bps).sum::<f64>();
        let used = 18 * GB + (self.wave(0.3, 2.0) * GB as f32) as u64;
        Snapshot {
            interval,
            now: FAKE_EPOCH + 3 * 86400 + 4000 + self.tick as i64,
            host: Host {
                model: "Mac14,12".into(),
                os: "15.6".into(),
                hostname: "orbital".into(),
                uptime: 3.0 * 86400.0 + 4000.0 + self.tick as f64,
                cores: 10,
                p_cores: 6,
                e_cores: 4,
                mem_total: 32 * GB,
                uid: 501,
            },
            cpu: Cpu {
                user: busy * 0.7,
                system: busy * 0.3,
                idle: 1.0 - busy,
                cores,
                load: [3.2, 2.8, 2.1],
                gpu: Some(self.wave(1.7, 0.4)),
                processes: procs.len() as u32,
                threads: procs.iter().map(|p| p.threads).sum(),
            },
            mem: Mem {
                total: 32 * GB,
                used,
                app: used - 6 * GB,
                wired: 4 * GB,
                compressed: 2 * GB,
                cached: 5 * GB,
                swap_used: 800 * MB,
                swap_total: 2 * GB,
                pressure: Pressure::Normal,
            },
            disk: Disk {
                read_bps: disk_r,
                write_bps: disk_w,
                read_ops: disk_r / 65536.0,
                write_ops: disk_w / 32768.0,
                read_total: 2_232_316_817_408 + (disk_r as u64) * self.tick as u64,
                write_total: 1_100_000_000_000 + (disk_w as u64) * self.tick as u64,
            },
            net: Net {
                in_bps: net_i,
                out_bps: net_o,
                in_pps: net_i / 1200.0,
                out_pps: net_o / 900.0,
                in_total: 90_000_000_000 + (net_i as u64) * self.tick as u64,
                out_total: 12_000_000_000 + (net_o as u64) * self.tick as u64,
            },
            power: Power {
                on_ac: true,
                battery: Some(Battery {
                    percent: 84.0,
                    charging: true,
                    minutes: Some(47),
                }),
                sleep_blockers: vec![(1340, "PreventUserIdleSystemSleep".into())],
            },
            procs,
        }
    }

    fn kill(&mut self, pid: i32, force: bool) -> KillOutcome {
        self.killed.push((pid, force));
        if self.protected.contains(&pid) {
            KillOutcome::NotPermitted
        } else if SEEDS.iter().any(|s| s.pid == pid) && !self.gone.contains(&pid) {
            self.gone.push(pid);
            KillOutcome::Sent
        } else {
            KillOutcome::NoSuchProcess
        }
    }
}
