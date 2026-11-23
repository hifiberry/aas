//! Runs the AES67 PTP listener alone and prints what it sees once a second.
//! It needs CAP_NET_RAW: run it with sudo, or after
//! `setcap cap_net_raw+ep` on the binary.
//!
//! cargo run --example ptp-listen -- [interface] [domain]

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use aas::aes67::listen;
use aas::aes67::ptp::clock_id_text;

fn main() {
    let mut args = std::env::args().skip(1);
    let interface = args.next().unwrap_or_else(|| "eth0".into());
    let domain = args
        .next()
        .map_or(0, |d| d.parse().expect("domain 0..=255"));
    let stop = Arc::new(AtomicBool::new(false));
    let (clock, _thread) = match listen::start(&interface, domain, stop) {
        Ok(started) => started,
        Err(e) => {
            eprintln!("no PTP listener: {e} (needs CAP_NET_RAW)");
            std::process::exit(1);
        }
    };
    let started = Instant::now();
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let t = started.elapsed().as_secs();
        match clock.now() {
            Some(now) => println!(
                "{t:>4} s  gm {}  domain {}  ptp {}.{:09}  rate {:+.2} ppm  generation {}",
                clock_id_text(&now.gm),
                now.domain,
                now.ptp_ns.div_euclid(1_000_000_000),
                now.ptp_ns.rem_euclid(1_000_000_000),
                (now.rate - 1.0) * 1e6,
                clock.generation(),
            ),
            None => println!("{t:>4} s  no PTP time  generation {}", clock.generation()),
        }
    }
}
