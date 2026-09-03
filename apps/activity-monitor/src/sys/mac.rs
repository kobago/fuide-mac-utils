//! The macOS source. Everything here runs on the sampler thread.
//!
//! What a plain user can read directly: Mach host statistics (CPU ticks, VM), sysctl (swap,
//! pressure, load, hardware), IOKit (disk, GPU, power), and libproc for *its own* processes.
//! libproc refuses other users' processes (`EPERM` — Activity Monitor and `top` are setuid), so
//! the base list comes from `ps` (setuid root, ~40 ms) and libproc overlays exact interval CPU,
//! footprint, threads, wakeups and disk bytes where it may. Per-process network bytes have no
//! public API at all; `nettop -n -L 1` (~10 ms) prints them for every process.

use std::collections::HashMap;
use std::ffi::CStr;
use std::mem;
use std::process::Command;
use std::time::Instant;

use super::{
    energy_estimate, iokit, Access, Core, CoreKind, Cpu, Disk, Host, KillOutcome, Mem, Net, Power,
    Pressure, Proc, Snapshot, Source, State,
};

const HOST_VM_INFO64: libc::c_int = 4;
const PROCESSOR_CPU_LOAD_INFO: libc::c_int = 2;
const CPU_STATE_USER: usize = 0;
const CPU_STATE_SYSTEM: usize = 1;
const CPU_STATE_IDLE: usize = 2;
const CPU_STATE_NICE: usize = 3;
const RTM_IFINFO2: u8 = 0x12;

extern "C" {
    // not in libc (deprecated there) nor mach2; the Mach trap that names the host port
    fn mach_host_self() -> libc::mach_port_t;
}
const IFT_LOOP: u8 = 0x18;

struct PrevProc {
    started: i64,
    /// user + system, mach ticks
    cpu_ticks: u64,
    wakeups: u64,
    disk_read: u64,
    disk_write: u64,
}

/// One `ps` row.
struct PsRow {
    pid: i32,
    ppid: i32,
    uid: u32,
    cpu: f32,
    rss: u64,
    vsz: u64,
    nice: i32,
    state: State,
    /// Seconds since start.
    elapsed: i64,
    /// Accumulated CPU seconds (`ps time`).
    cpu_time: f64,
    comm: String,
}

pub struct MacSource {
    /// Nanoseconds per mach tick.
    tick_ns: f64,
    page: u64,
    host: Host,
    last: Option<Instant>,
    prev_ticks: Vec<[u32; 4]>,
    prev_procs: HashMap<i32, PrevProc>,
    prev_net_procs: HashMap<i32, (u64, u64)>,
    prev_disk: Option<(u64, u64, u64, u64)>,
    prev_net: Option<(u64, u64, u64, u64)>,
    users: HashMap<u32, String>,
    /// pid -> (start time, command line), so sysctl runs once per process
    cmdlines: HashMap<i32, (i64, Option<String>)>,
    argmax: usize,
}

impl Default for MacSource {
    fn default() -> Self {
        Self::new()
    }
}

impl MacSource {
    pub fn new() -> Self {
        let mut tb = mach2::mach_time::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: plain out-parameter call.
        unsafe { mach2::mach_time::mach_timebase_info(&mut tb) };
        let tick_ns = if tb.denom == 0 {
            1.0
        } else {
            tb.numer as f64 / tb.denom as f64
        };
        let page = sysctl_u64("hw.pagesize").unwrap_or(16384);
        let host = read_host();
        Self {
            tick_ns,
            page,
            host,
            last: None,
            prev_ticks: Vec::new(),
            prev_procs: HashMap::new(),
            prev_net_procs: HashMap::new(),
            prev_disk: None,
            prev_net: None,
            users: HashMap::new(),
            cmdlines: HashMap::new(),
            argmax: sysctl_u64("kern.argmax").unwrap_or(262_144) as usize,
        }
    }

    fn user_name(&mut self, uid: u32) -> String {
        if let Some(n) = self.users.get(&uid) {
            return n.clone();
        }
        let name = getpwuid(uid).unwrap_or_else(|| uid.to_string());
        self.users.insert(uid, name.clone());
        name
    }

    fn cpu(&mut self, dt: f32) -> Cpu {
        let ticks = cpu_ticks();
        let mut cores = Vec::with_capacity(ticks.len());
        let (mut tu, mut ts, mut ti) = (0u64, 0u64, 0u64);
        for (i, now) in ticks.iter().enumerate() {
            let prev = self.prev_ticks.get(i).copied().unwrap_or([0; 4]);
            let d = |k: usize| now[k].wrapping_sub(prev[k]) as u64;
            let user = d(CPU_STATE_USER) + d(CPU_STATE_NICE);
            let sys = d(CPU_STATE_SYSTEM);
            let idle = d(CPU_STATE_IDLE);
            let total = (user + sys + idle).max(1);
            tu += user;
            ts += sys;
            ti += idle;
            let kind = if self.host.e_cores > 0 && self.host.p_cores > 0 {
                if (i as u32) < self.host.e_cores {
                    CoreKind::Efficiency
                } else {
                    CoreKind::Performance
                }
            } else {
                CoreKind::Standard
            };
            cores.push(Core {
                kind,
                usage: if self.prev_ticks.is_empty() || dt <= 0.0 {
                    0.0
                } else {
                    (user + sys) as f32 / total as f32
                },
            });
        }
        self.prev_ticks = ticks;
        let total = (tu + ts + ti).max(1) as f32;
        let mut load = [0f64; 3];
        // SAFETY: out-array of 3 doubles.
        unsafe { libc::getloadavg(load.as_mut_ptr(), 3) };
        Cpu {
            user: if dt > 0.0 { tu as f32 / total } else { 0.0 },
            system: if dt > 0.0 { ts as f32 / total } else { 0.0 },
            idle: if dt > 0.0 { ti as f32 / total } else { 1.0 },
            cores,
            load: [load[0] as f32, load[1] as f32, load[2] as f32],
            gpu: iokit::gpu_utilisation(),
            processes: 0,
            threads: 0,
        }
    }

    fn mem(&self) -> Mem {
        let mut vm: libc::vm_statistics64 = unsafe { mem::zeroed() };
        let mut count = (mem::size_of::<libc::vm_statistics64>()
            / mem::size_of::<libc::integer_t>())
            as libc::mach_msg_type_number_t;
        // SAFETY: out-struct with its size in `count`.
        let ok = unsafe {
            libc::host_statistics64(
                mach_host_self(),
                HOST_VM_INFO64,
                &mut vm as *mut _ as libc::host_info64_t,
                &mut count,
            )
        } == libc::KERN_SUCCESS;
        let pg = |n: u64| n * self.page;
        let (app, wired, compressed, cached) = if ok {
            let internal = vm.internal_page_count as u64;
            let purgeable = vm.purgeable_count as u64;
            (
                pg(internal.saturating_sub(purgeable)),
                pg(vm.wire_count as u64),
                pg(vm.compressor_page_count as u64),
                pg(vm.external_page_count as u64 + purgeable),
            )
        } else {
            (0, 0, 0, 0)
        };
        let mut swap: libc::xsw_usage = unsafe { mem::zeroed() };
        let mut len = mem::size_of::<libc::xsw_usage>();
        // SAFETY: out-struct with its size.
        unsafe {
            libc::sysctlbyname(
                c"vm.swapusage".as_ptr(),
                &mut swap as *mut _ as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        let pressure = match sysctl_u64("kern.memorystatus_vm_pressure_level").unwrap_or(1) {
            4 => Pressure::Critical,
            2 => Pressure::Warn,
            _ => Pressure::Normal,
        };
        Mem {
            total: self.host.mem_total,
            used: app + wired + compressed,
            app,
            wired,
            compressed,
            cached,
            swap_used: swap.xsu_used,
            swap_total: swap.xsu_total,
            pressure,
        }
    }

    fn disk(&mut self, dt: f32) -> Disk {
        let now = iokit::disk_totals();
        let mut d = Disk {
            read_total: now.0,
            write_total: now.1,
            ..Default::default()
        };
        if let Some(p) = self.prev_disk {
            if dt > 0.0 {
                let r = |a: u64, b: u64| a.saturating_sub(b) as f64 / dt as f64;
                d.read_bps = r(now.0, p.0);
                d.write_bps = r(now.1, p.1);
                d.read_ops = r(now.2, p.2);
                d.write_ops = r(now.3, p.3);
            }
        }
        self.prev_disk = Some(now);
        d
    }

    fn net(&mut self, dt: f32) -> Net {
        let now = if_totals();
        let mut n = Net {
            in_total: now.0,
            out_total: now.1,
            ..Default::default()
        };
        if let Some(p) = self.prev_net {
            if dt > 0.0 {
                let r = |a: u64, b: u64| a.saturating_sub(b) as f64 / dt as f64;
                n.in_bps = r(now.0, p.0);
                n.out_bps = r(now.1, p.1);
                n.in_pps = r(now.2, p.2);
                n.out_pps = r(now.3, p.3);
            }
        }
        self.prev_net = Some(now);
        n
    }

    fn procs(&mut self, dt: f32, blockers: &[(i32, String)]) -> Vec<Proc> {
        let now_unix = unix_now();
        let rows = ps_rows();
        let net = nettop();
        let mut prev_procs = mem::take(&mut self.prev_procs);
        let mut next_prev = HashMap::with_capacity(rows.len());
        let mut prev_net = mem::take(&mut self.prev_net_procs);
        let mut next_net = HashMap::with_capacity(net.len());
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let pid = row.pid;
            let mut p = Proc {
                pid,
                ppid: row.ppid,
                uid: row.uid,
                user: String::new(),
                name: basename(&row.comm),
                path: Some(row.comm.clone()).filter(|c| c.starts_with('/')),
                cmdline: None,
                access: Access::Limited,
                state: row.state,
                cpu: row.cpu,
                cpu_time: row.cpu_time,
                threads: 0,
                wakeups: 0.0,
                mem: row.rss,
                rss: row.rss,
                vsz: row.vsz,
                nice: row.nice,
                started: Some(now_unix - row.elapsed),
                ..Default::default()
            };
            p.user = self.user_name(row.uid);

            // libproc overlay (own processes)
            if let Some((bsd, task, ru)) = proc_info(pid) {
                p.access = Access::Full;
                p.ppid = bsd.pbi_ppid as i32;
                p.started = Some(bsd.pbi_start_tvsec as i64);
                p.nice = bsd.pbi_nice;
                if let Some(path) = pid_path(pid) {
                    p.name = basename(&path);
                    p.path = Some(path);
                } else {
                    let n = cstr(&bsd.pbi_name);
                    if !n.is_empty() {
                        p.name = n;
                    }
                }
                p.threads = task.pti_threadnum.max(0) as u32;
                p.rss = task.pti_resident_size;
                p.vsz = task.pti_virtual_size;
                p.mem = ru.ri_phys_footprint;
                p.disk_read = ru.ri_diskio_bytesread;
                p.disk_write = ru.ri_diskio_byteswritten;
                let ticks = task.pti_total_user + task.pti_total_system;
                p.cpu_time = ticks as f64 * self.tick_ns / 1e9;
                let wk = ru.ri_pkg_idle_wkups + ru.ri_interrupt_wkups;
                let started = bsd.pbi_start_tvsec as i64;
                match prev_procs.remove(&pid) {
                    Some(prev) if prev.started == started && dt > 0.0 => {
                        let d = ticks.saturating_sub(prev.cpu_ticks) as f64 * self.tick_ns / 1e9;
                        p.cpu = (d / dt as f64 * 100.0) as f32;
                        p.wakeups = wk.saturating_sub(prev.wakeups) as f32 / dt;
                        p.disk_read_bps =
                            p.disk_read.saturating_sub(prev.disk_read) as f64 / dt as f64;
                        p.disk_write_bps =
                            p.disk_write.saturating_sub(prev.disk_write) as f64 / dt as f64;
                    }
                    _ => p.cpu = 0.0,
                }
                next_prev.insert(
                    pid,
                    PrevProc {
                        started,
                        cpu_ticks: ticks,
                        wakeups: wk,
                        disk_read: p.disk_read,
                        disk_write: p.disk_write,
                    },
                );
                p.cmdline = match self.cmdlines.get(&pid) {
                    Some((s, c)) if *s == started => c.clone(),
                    _ => {
                        let c = proc_args(pid, self.argmax);
                        self.cmdlines.insert(pid, (started, c.clone()));
                        c
                    }
                };
            }

            if let Some(&(i, o)) = net.get(&pid) {
                p.net_in = i;
                p.net_out = o;
                if let Some(&(pi, po)) = prev_net.get(&pid) {
                    if dt > 0.0 {
                        p.net_in_bps = i.saturating_sub(pi) as f64 / dt as f64;
                        p.net_out_bps = o.saturating_sub(po) as f64 / dt as f64;
                    }
                }
                next_net.insert(pid, (i, o));
            }
            p.prevents_sleep = blockers.iter().any(|(b, _)| *b == pid);
            p.energy = energy_estimate(
                p.cpu,
                p.wakeups,
                p.disk_read_bps + p.disk_write_bps,
                p.net_in_bps + p.net_out_bps,
            );
            out.push(p);
        }
        prev_net.clear();
        self.prev_procs = next_prev;
        self.prev_net_procs = next_net;
        self.cmdlines
            .retain(|pid, _| self.prev_procs.contains_key(pid));
        out
    }
}

impl Source for MacSource {
    fn sample(&mut self) -> Snapshot {
        let now = Instant::now();
        let dt = self
            .last
            .map(|l| now.duration_since(l).as_secs_f32())
            .unwrap_or(0.0);
        self.last = Some(now);
        self.host.uptime = uptime();

        let mut cpu = self.cpu(dt);
        let mem = self.mem();
        let disk = self.disk(dt);
        let net = self.net(dt);
        let (on_ac, battery) = iokit::power_source();
        let sleep_blockers = iokit::sleep_blockers();
        let procs = self.procs(dt, &sleep_blockers);
        cpu.processes = procs.len() as u32;
        cpu.threads = procs.iter().map(|p| p.threads).sum();
        Snapshot {
            interval: dt,
            now: unix_now(),
            host: self.host.clone(),
            cpu,
            mem,
            disk,
            net,
            power: Power {
                on_ac,
                battery,
                sleep_blockers,
            },
            procs,
        }
    }

    fn kill(&mut self, pid: i32, force: bool) -> KillOutcome {
        let sig = if force { libc::SIGKILL } else { libc::SIGTERM };
        // SAFETY: kill(2) with a pid the user chose; errors come back as errno.
        if unsafe { libc::kill(pid, sig) } == 0 {
            return KillOutcome::Sent;
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::EPERM) => KillOutcome::NotPermitted,
            Some(libc::ESRCH) => KillOutcome::NoSuchProcess,
            _ => KillOutcome::Failed(std::io::Error::last_os_error().to_string()),
        }
    }
}

// ---- host ---------------------------------------------------------------------------------

fn read_host() -> Host {
    let p_name = sysctl_string("hw.perflevel0.name").unwrap_or_default();
    let l0 = sysctl_u64("hw.perflevel0.logicalcpu").unwrap_or(0) as u32;
    let l1 = sysctl_u64("hw.perflevel1.logicalcpu").unwrap_or(0) as u32;
    let (p_cores, e_cores) = if p_name == "Performance" {
        (l0, l1)
    } else {
        (l1, l0)
    };
    let mut name = [0u8; 256];
    // SAFETY: buffer with its length.
    unsafe { libc::gethostname(name.as_mut_ptr() as *mut libc::c_char, name.len()) };
    let hostname = cstr_bytes(&name);
    Host {
        model: sysctl_string("hw.model").unwrap_or_default(),
        os: sysctl_string("kern.osproductversion").unwrap_or_default(),
        hostname: hostname.trim_end_matches(".local").to_string(),
        uptime: uptime(),
        cores: sysctl_u64("hw.ncpu").unwrap_or(0) as u32,
        p_cores,
        e_cores,
        mem_total: sysctl_u64("hw.memsize").unwrap_or(0),
        // SAFETY: no preconditions.
        uid: unsafe { libc::getuid() },
    }
}

fn uptime() -> f64 {
    let mut tv: libc::timeval = unsafe { mem::zeroed() };
    let mut len = mem::size_of::<libc::timeval>();
    // SAFETY: out-struct with its size.
    let ok = unsafe {
        libc::sysctlbyname(
            c"kern.boottime".as_ptr(),
            &mut tv as *mut _ as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } == 0;
    if !ok {
        return 0.0;
    }
    let boot = tv.tv_sec as f64 + tv.tv_usec as f64 / 1e6;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(boot);
    (now - boot).max(0.0)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn sysctl_u64(name: &str) -> Option<u64> {
    let cname = std::ffi::CString::new(name).ok()?;
    let mut buf = [0u8; 8];
    let mut len = buf.len();
    // SAFETY: buffer with its length; the kernel writes 4 or 8 bytes.
    let ok = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } == 0;
    if !ok {
        return None;
    }
    Some(match len {
        4 => u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]) as u64,
        8 => u64::from_ne_bytes(buf),
        _ => return None,
    })
}

fn sysctl_string(name: &str) -> Option<String> {
    let cname = std::ffi::CString::new(name).ok()?;
    let mut buf = [0u8; 256];
    let mut len = buf.len();
    // SAFETY: buffer with its length.
    let ok = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } == 0;
    ok.then(|| cstr_bytes(&buf))
}

fn getpwuid(uid: u32) -> Option<String> {
    let mut pwd: libc::passwd = unsafe { mem::zeroed() };
    let mut buf = vec![0u8; 1024];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: reentrant lookup into our buffer.
    let rc = unsafe {
        libc::getpwuid_r(
            uid,
            &mut pwd,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() {
        return None;
    }
    // SAFETY: pw_name points into `buf`, NUL-terminated.
    Some(
        unsafe { CStr::from_ptr(pwd.pw_name) }
            .to_string_lossy()
            .into_owned(),
    )
}

// ---- cpu ----------------------------------------------------------------------------------

fn cpu_ticks() -> Vec<[u32; 4]> {
    let mut count: libc::natural_t = 0;
    let mut info: libc::processor_info_array_t = std::ptr::null_mut();
    let mut info_count: libc::mach_msg_type_number_t = 0;
    // SAFETY: out-parameters; the array is vm_deallocated below.
    let ok = unsafe {
        libc::host_processor_info(
            mach_host_self(),
            PROCESSOR_CPU_LOAD_INFO,
            &mut count,
            &mut info,
            &mut info_count,
        )
    } == libc::KERN_SUCCESS;
    if !ok || info.is_null() {
        return Vec::new();
    }
    let n = count as usize;
    let mut out = Vec::with_capacity(n);
    // SAFETY: the kernel returned `count` entries of 4 ticks each.
    unsafe {
        let ticks = std::slice::from_raw_parts(info as *const u32, n * 4);
        for c in ticks.chunks_exact(4) {
            out.push([c[0], c[1], c[2], c[3]]);
        }
        libc::vm_deallocate(
            mach2::traps::mach_task_self(),
            info as libc::vm_address_t,
            info_count as libc::vm_size_t * mem::size_of::<libc::integer_t>(),
        );
    }
    out
}

// ---- network ------------------------------------------------------------------------------

/// Cumulative (bytes in, bytes out, packets in, packets out) over non-loopback interfaces.
fn if_totals() -> (u64, u64, u64, u64) {
    let mut mib = [libc::CTL_NET, libc::PF_ROUTE, 0, 0, libc::NET_RT_IFLIST2, 0];
    let mut len = 0usize;
    // SAFETY: size query, then a read into a buffer of that size.
    unsafe {
        if libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        ) != 0
        {
            return (0, 0, 0, 0);
        }
        let mut buf = vec![0u8; len];
        if libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        ) != 0
        {
            return (0, 0, 0, 0);
        }
        let mut t = (0u64, 0u64, 0u64, 0u64);
        let mut off = 0usize;
        while off + mem::size_of::<libc::if_msghdr>() <= len {
            let hdr = &*(buf.as_ptr().add(off) as *const libc::if_msghdr);
            let msglen = hdr.ifm_msglen as usize;
            if msglen == 0 {
                break;
            }
            if hdr.ifm_type == RTM_IFINFO2 && off + mem::size_of::<libc::if_msghdr2>() <= len {
                let m = &*(buf.as_ptr().add(off) as *const libc::if_msghdr2);
                let d = &m.ifm_data;
                if d.ifi_type != IFT_LOOP {
                    t.0 += d.ifi_ibytes;
                    t.1 += d.ifi_obytes;
                    t.2 += d.ifi_ipackets;
                    t.3 += d.ifi_opackets;
                }
            }
            off += msglen;
        }
        t
    }
}

/// pid -> (bytes in, bytes out) from `nettop` (private NetworkStatistics framework; the tool
/// runs as a plain user). `-n` skips name resolution, without it a run takes seconds.
fn nettop() -> HashMap<i32, (u64, u64)> {
    let mut out = HashMap::new();
    let Ok(res) = Command::new("/usr/bin/nettop")
        .args(["-n", "-P", "-L", "1", "-x", "-J", "bytes_in,bytes_out"])
        .output()
    else {
        return out;
    };
    for line in String::from_utf8_lossy(&res.stdout).lines() {
        let mut f = line.split(',');
        let (Some(name), Some(i), Some(o)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        let Some(pid) = name.rsplit('.').next().and_then(|p| p.parse::<i32>().ok()) else {
            continue;
        };
        let (Ok(i), Ok(o)) = (i.parse::<u64>(), o.parse::<u64>()) else {
            continue;
        };
        out.insert(pid, (i, o));
    }
    out
}

// ---- processes ----------------------------------------------------------------------------

/// The base list for every process on the machine (setuid `ps`).
fn ps_rows() -> Vec<PsRow> {
    let Ok(res) = Command::new("/bin/ps")
        .args([
            "-axo",
            "pid=,ppid=,uid=,pcpu=,rss=,vsz=,nice=,stat=,etime=,time=,comm=",
        ])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&res.stdout)
        .lines()
        .filter_map(parse_ps_row)
        .collect()
}

fn parse_ps_row(line: &str) -> Option<PsRow> {
    // ten fixed tokens, then `comm` (which may contain spaces) as the rest of the line
    let mut rest = line.trim_start();
    let mut tok = || -> Option<&str> {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let (t, r) = rest.split_at(end);
        rest = r.trim_start();
        (!t.is_empty()).then_some(t)
    };
    let pid = tok()?.parse().ok()?;
    let ppid = tok()?.parse().ok()?;
    let uid = tok()?.parse().ok()?;
    let cpu = tok()?.parse().ok()?;
    let rss: u64 = tok()?.parse().ok()?;
    let vsz: u64 = tok()?.parse().ok()?;
    let nice = tok()?.parse().unwrap_or(0);
    let stat = tok()?;
    let etime = tok()?;
    let time = tok()?;
    let comm = rest.trim().to_string();
    Some(PsRow {
        pid,
        ppid,
        uid,
        cpu,
        rss: rss * 1024,
        vsz: vsz * 1024,
        nice,
        state: match stat.chars().next() {
            Some('R') => State::Running,
            Some('S') => State::Sleeping,
            Some('I') => State::Idle,
            Some('T') => State::Stopped,
            Some('Z') => State::Zombie,
            Some('U') => State::Sleeping,
            _ => State::Unknown,
        },
        elapsed: parse_etime(etime),
        cpu_time: parse_cpu_time(time),
        comm,
    })
}

/// `[[hh:]mm:]ss.cc` (minutes may exceed 59) -> seconds.
fn parse_cpu_time(s: &str) -> f64 {
    let parts: Vec<f64> = s.split(':').map(|p| p.parse().unwrap_or(0.0)).collect();
    match parts.as_slice() {
        [h, m, s] => h * 3600.0 + m * 60.0 + s,
        [m, s] => m * 60.0 + s,
        [s] => *s,
        _ => 0.0,
    }
}

/// `[[dd-]hh:]mm:ss` -> seconds.
fn parse_etime(s: &str) -> i64 {
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<i64>().unwrap_or(0), r),
        None => (0, s),
    };
    let parts: Vec<i64> = rest.split(':').map(|p| p.parse().unwrap_or(0)).collect();
    let secs = match parts.as_slice() {
        [h, m, s] => h * 3600 + m * 60 + s,
        [m, s] => m * 60 + s,
        [s] => *s,
        _ => 0,
    };
    days * 86400 + secs
}

fn proc_info(
    pid: i32,
) -> Option<(
    libc::proc_bsdinfo,
    libc::proc_taskinfo,
    libc::rusage_info_v4,
)> {
    let mut bsd: libc::proc_bsdinfo = unsafe { mem::zeroed() };
    let mut task: libc::proc_taskinfo = unsafe { mem::zeroed() };
    let mut ru: libc::rusage_info_v4 = unsafe { mem::zeroed() };
    // SAFETY: out-structs with their sizes; libproc reports how much it wrote.
    unsafe {
        let n = libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut bsd as *mut _ as *mut libc::c_void,
            mem::size_of::<libc::proc_bsdinfo>() as i32,
        );
        if n != mem::size_of::<libc::proc_bsdinfo>() as i32 {
            return None;
        }
        let n = libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTASKINFO,
            0,
            &mut task as *mut _ as *mut libc::c_void,
            mem::size_of::<libc::proc_taskinfo>() as i32,
        );
        if n != mem::size_of::<libc::proc_taskinfo>() as i32 {
            return None;
        }
        if libc::proc_pid_rusage(
            pid,
            libc::RUSAGE_INFO_V4,
            &mut ru as *mut _ as *mut libc::rusage_info_t,
        ) != 0
        {
            return None;
        }
    }
    Some((bsd, task, ru))
}

fn pid_path(pid: i32) -> Option<String> {
    let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: buffer with its length.
    let n =
        unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32) };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

/// Command line via `KERN_PROCARGS2` (own processes; the kernel refuses the others).
fn proc_args(pid: i32, argmax: usize) -> Option<String> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut buf = vec![0u8; argmax];
    let mut len = buf.len();
    // SAFETY: buffer with its length.
    let ok = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } == 0;
    if !ok || len < 4 {
        return None;
    }
    let argc = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    let data = &buf[4..len];
    // exec path, NUL padding, then argc NUL-terminated strings
    let mut i = data.iter().position(|&b| b == 0)?;
    while i < data.len() && data[i] == 0 {
        i += 1;
    }
    let mut args = Vec::with_capacity(argc);
    let mut rest = &data[i..];
    for _ in 0..argc {
        let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
        args.push(String::from_utf8_lossy(&rest[..end]).into_owned());
        if end >= rest.len() {
            break;
        }
        rest = &rest[end + 1..];
    }
    (!args.is_empty()).then(|| args.join(" "))
}

fn basename(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

fn cstr(buf: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn cstr_bytes(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ps_row_parses_comm_with_spaces_and_etime_forms() {
        let r = parse_ps_row(
            "  4242  1  501  12.5  204800  4194304  0 S+  1-02:03:04 1830:09.67 /Applications/Foo Bar.app/Contents/MacOS/Foo Bar",
        )
        .unwrap();
        assert_eq!(r.pid, 4242);
        assert_eq!(r.uid, 501);
        assert_eq!(r.rss, 204800 * 1024);
        assert_eq!(r.state, State::Sleeping);
        assert_eq!(r.elapsed, 86400 + 2 * 3600 + 3 * 60 + 4);
        assert_eq!(r.comm, "/Applications/Foo Bar.app/Contents/MacOS/Foo Bar");
        assert_eq!(basename(&r.comm), "Foo Bar");
        assert_eq!(r.cpu_time, 1830.0 * 60.0 + 9.67);
        assert_eq!(parse_cpu_time("0:01.52"), 1.52);
        assert_eq!(parse_cpu_time("1:02:03.50"), 3723.5);
        assert_eq!(parse_etime("05:07"), 307);
        assert_eq!(parse_etime("12:00:00"), 43200);
    }

    #[test]
    fn live_sample_reads_this_machine() {
        let mut src = MacSource::new();
        let first = src.sample();
        assert!(first.host.cores > 0);
        assert!(first.mem.total > 0);
        assert!(!first.procs.is_empty());
        let me = std::process::id() as i32;
        let mine = first
            .procs
            .iter()
            .find(|p| p.pid == me)
            .expect("own process listed");
        assert_eq!(
            mine.access,
            Access::Full,
            "own process readable via libproc"
        );
        assert!(mine.mem > 0);
        assert!(mine.cmdline.is_some());
        assert!(first
            .procs
            .iter()
            .any(|p| p.pid == 1 && p.access == Access::Limited));
        std::thread::sleep(std::time::Duration::from_millis(300));
        let second = src.sample();
        assert!(second.interval > 0.2);
        assert!(!second.cpu.cores.is_empty());
    }
}

#[cfg(test)]
mod timing {
    use super::*;

    /// `cargo test -p fuide-activity-monitor timing -- --ignored --nocapture`: where a sample's
    /// time goes on this machine.
    #[test]
    #[ignore]
    fn stages() {
        let ms = |t: Instant| t.elapsed().as_secs_f64() * 1e3;
        let t = Instant::now();
        let rows = ps_rows();
        println!("ps_rows: {:.1} ms ({} rows)", ms(t), rows.len());
        let t = Instant::now();
        let n = nettop();
        println!("nettop: {:.1} ms ({} pids)", ms(t), n.len());
        let t = Instant::now();
        let mut full = 0;
        for r in &rows {
            if proc_info(r.pid).is_some() {
                full += 1;
            }
        }
        println!("proc_info x{}: {:.1} ms ({} full)", rows.len(), ms(t), full);
        let t = Instant::now();
        for r in &rows {
            let _ = pid_path(r.pid);
        }
        println!("pid_path x{}: {:.1} ms", rows.len(), ms(t));
        let t = Instant::now();
        let _ = cpu_ticks();
        println!("cpu_ticks: {:.1} ms", ms(t));
        let t = Instant::now();
        let _ = iokit::disk_totals();
        println!("disk_totals: {:.1} ms", ms(t));
        let t = Instant::now();
        let _ = iokit::gpu_utilisation();
        println!("gpu: {:.1} ms", ms(t));
        let t = Instant::now();
        let _ = iokit::power_source();
        let _ = iokit::sleep_blockers();
        println!("power: {:.1} ms", ms(t));
        let t = Instant::now();
        let _ = if_totals();
        println!("if_totals: {:.1} ms", ms(t));
        let t = Instant::now();
        let mut src = MacSource::new();
        println!("MacSource::new: {:.1} ms", ms(t));
        let _ = src.sample();
        let t = Instant::now();
        let _ = src.sample();
        println!("sample: {:.1} ms", ms(t));
    }
}
