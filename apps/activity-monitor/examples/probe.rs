//! `cargo run -p fuide-activity-monitor --example probe`: two live samples, the busiest
//! processes and the machine totals, to eyeball the source against Activity Monitor.

use std::time::Duration;

use fuide_activity_monitor::sys::{self, Access};

fn main() {
    let mut src = sys::native();
    let _ = src.sample();
    std::thread::sleep(Duration::from_secs(2));
    let t0 = std::time::Instant::now();
    let s = src.sample();
    println!("sample() took {:.1} ms", t0.elapsed().as_secs_f64() * 1e3);
    for _ in 0..3 {
        std::thread::sleep(Duration::from_millis(500));
        let t0 = std::time::Instant::now();
        let _ = src.sample();
        println!("sample() took {:.1} ms", t0.elapsed().as_secs_f64() * 1e3);
    }
    println!(
        "{} {} up {:.0}s :: {} cores ({}P/{}E) :: interval {:.2}s",
        s.host.model,
        s.host.os,
        s.host.uptime,
        s.host.cores,
        s.host.p_cores,
        s.host.e_cores,
        s.interval
    );
    println!(
        "cpu user {:.1}% sys {:.1}% idle {:.1}% load {:?} gpu {:?} procs {} threads {}",
        s.cpu.user * 100.0,
        s.cpu.system * 100.0,
        s.cpu.idle * 100.0,
        s.cpu.load,
        s.cpu.gpu,
        s.cpu.processes,
        s.cpu.threads
    );
    let cores: Vec<String> = s
        .cpu
        .cores
        .iter()
        .map(|c| format!("{:.0}", c.usage * 100.0))
        .collect();
    println!("cores {}", cores.join(" "));
    let gb = |b: u64| b as f64 / 1e9;
    println!(
        "mem used {:.2} app {:.2} wired {:.2} comp {:.2} cached {:.2} swap {:.2}/{:.2} GB pressure {:?}",
        gb(s.mem.used), gb(s.mem.app), gb(s.mem.wired), gb(s.mem.compressed), gb(s.mem.cached),
        gb(s.mem.swap_used), gb(s.mem.swap_total), s.mem.pressure
    );
    println!(
        "disk r {:.1} KB/s w {:.1} KB/s ({:.0}/{:.0} ops) totals {:.1}/{:.1} GB",
        s.disk.read_bps / 1e3,
        s.disk.write_bps / 1e3,
        s.disk.read_ops,
        s.disk.write_ops,
        gb(s.disk.read_total),
        gb(s.disk.write_total)
    );
    println!(
        "net in {:.1} KB/s out {:.1} KB/s ({:.0}/{:.0} pps)",
        s.net.in_bps / 1e3,
        s.net.out_bps / 1e3,
        s.net.in_pps,
        s.net.out_pps
    );
    println!(
        "power ac {} battery {:?} blockers {:?}",
        s.power.on_ac, s.power.battery, s.power.sleep_blockers
    );
    let mut procs = s.procs.clone();
    procs.sort_by(|a, b| b.cpu.partial_cmp(&a.cpu).unwrap());
    let full = procs.iter().filter(|p| p.access == Access::Full).count();
    println!("{} processes, {} readable via libproc", procs.len(), full);
    for p in procs.iter().take(12) {
        println!(
            "{:>6} {:<28} {:<8} {:>6.1}% {:>8.1} MB thr {:>3} wk {:>6.1} d {:>8.1} KB/s n {:>8.1} KB/s e {:>6.1} {:?} {}",
            p.pid, p.name.chars().take(28).collect::<String>(), p.user, p.cpu, p.mem as f64 / 1e6,
            p.threads, p.wakeups, (p.disk_read_bps + p.disk_write_bps) / 1e3,
            (p.net_in_bps + p.net_out_bps) / 1e3, p.energy, p.access,
            if p.prevents_sleep { "NOSLEEP" } else { "" }
        );
    }
    let mut by_net: Vec<_> = s
        .procs
        .iter()
        .filter(|p| p.net_in_bps + p.net_out_bps > 0.0)
        .collect();
    by_net.sort_by(|a, b| {
        (b.net_in_bps + b.net_out_bps)
            .partial_cmp(&(a.net_in_bps + a.net_out_bps))
            .unwrap()
    });
    for p in by_net.iter().take(5) {
        println!(
            "net {:>6} {:<28} in {:>8.1} out {:>8.1} KB/s",
            p.pid,
            p.name,
            p.net_in_bps / 1e3,
            p.net_out_bps / 1e3
        );
    }
}
