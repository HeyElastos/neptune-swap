//! Checks the carrier between the solver and a capsule: joins `topic` on the
//! elastOS node in `coords` (the operator approves elastos://peer/* in Home),
//! sends one message and prints everything others send for `secs` seconds.
//!
//!   cargo run --release -p solver --example carrier_probe -- <runtime-coords.json> [topic] [secs]

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use solver::carrier::Carrier;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let coords = args.first().context("runtime coords file")?;
    let topic = args.get(1).map(String::as_str).unwrap_or("neptune-swap-live");
    let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(120);
    let carrier = Carrier::connect(
        coords.as_ref(),
        "neptune-swap",
        "solver",
        "Neptune swaps: carrier check with the capsule",
        Duration::from_secs(280),
    )?;
    carrier.join(topic)?;
    carrier.send(topic, b"hello from the solver")?;
    println!("joined {topic}, sent one message; listening {secs} s");
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        for m in carrier.recv(topic)? {
            println!("received: {}", String::from_utf8_lossy(&m));
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    Ok(())
}
