//! `hackriffd`: the hackriff daemon. It owns the SDR, the pipeline and the stores, and serves the
//! control API. Stub until the pipeline tasks land.

use clap::Parser;

#[derive(Parser)]
#[command(name = "hackriffd", version, about = "hackriff daemon (stub)")]
struct Args {}

fn main() {
    let Args {} = Args::parse();
    println!(
        "hackriffd {}: daemon stub (T-001); no pipeline yet",
        env!("CARGO_PKG_VERSION")
    );
}
