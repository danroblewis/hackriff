//! Null draws for the calibration generator (ADR-0015 §2.2, §13.3; T-853 = MAUTO M-2).
//!
//! Runs each [`hk_synth::nullchain::NullChain`] over `--windows` seeded noise windows at each of
//! its supports and prints one JSON document per block: the block's `name@version`, the
//! corpus it was drawn from, and per support the raw values of every metric the block
//! published, with the metric's stage, declared group and direction. `py/hkpy/calibrate.py`
//! turns this into the `hackriff.calibration/1` file; this program does no statistics.
//!
//! ```text
//! cargo run --release -p hk-synth --example calibration_draws -- [--windows 4096] [--seed 1] [--threads 6] [BLOCK ...]
//! ```

use std::collections::BTreeMap;

use hk_blocks::Registry;
use hk_synth::nullchain::{NULL_RATE_HZ, NULL_SIGMA_LSB, null_chains, smaller_is_evidence};
use serde_json::{Value, json};

fn main() {
    let mut windows = 4096u32;
    let mut seed = 1u64;
    let mut threads = 6usize;
    let mut only: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut num = |name: &str| -> u64 {
            args.next()
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| panic!("{name} needs a number"))
        };
        match a.as_str() {
            "--windows" => windows = num("--windows") as u32,
            "--seed" => seed = num("--seed"),
            "--threads" => threads = num("--threads").max(1) as usize,
            _ => only.push(a),
        }
    }
    let registry = Registry::builtin();
    let mut docs = Vec::new();
    for chain in null_chains() {
        if !only.is_empty() && !only.iter().any(|b| b == chain.block) {
            continue;
        }
        let mut cells = Vec::new();
        for (si, &n) in chain.supports.iter().enumerate() {
            // Seeds are disjoint per (block, support, window) and from the check's (which
            // starts at 1 << 40).
            let base = seed
                .wrapping_mul(1_000_003)
                .wrapping_add((si as u64) << 32)
                .wrapping_add(hash(chain.block) << 20);
            let per = windows.div_ceil(threads as u32);
            let draws: Vec<Vec<hk_blocks::Evidence>> = std::thread::scope(|s| {
                let hs: Vec<_> = (0..threads as u32)
                    .map(|t| {
                        let (chain, registry) = (&chain, &registry);
                        s.spawn(move || {
                            let lo = t * per;
                            let hi = ((t + 1) * per).min(windows);
                            (lo..hi)
                                .map(|w| {
                                    chain
                                        .draw(registry, n, base.wrapping_add(u64::from(w)))
                                        .expect("null draw")
                                })
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect();
                hs.into_iter()
                    .flat_map(|h| h.join().expect("thread"))
                    .collect()
            });
            let mut metrics: BTreeMap<&'static str, Value> = BTreeMap::new();
            let mut raws: BTreeMap<&'static str, Vec<f64>> = BTreeMap::new();
            let (mut min_n, mut max_n) = (u32::MAX, 0u32);
            for d in &draws {
                for e in d {
                    min_n = min_n.min(e.n);
                    max_n = max_n.max(e.n);
                    raws.entry(e.metric.as_str())
                        .or_default()
                        .push(f64::from(e.raw));
                    metrics.entry(e.metric.as_str()).or_insert_with(|| {
                        json!({
                            "stage": e.stage,
                            "group": e.group,
                            "direction": if smaller_is_evidence(e.metric) { "smaller" } else { "larger" },
                        })
                    });
                }
            }
            for (m, v) in metrics.iter_mut() {
                let r = raws.remove(m).unwrap_or_default();
                if r.len() != draws.len() {
                    // A metric absent from some windows cannot be paired for correlation.
                    eprintln!(
                        "{}: {m} present in {} of {} windows",
                        chain.block,
                        r.len(),
                        draws.len()
                    );
                }
                v["raw"] = json!(r);
            }
            cells.push(json!({ "n": n, "min_n": min_n, "max_n": max_n, "windows": draws.len(), "metrics": metrics }));
            eprintln!("{} n={n}: {} windows", chain.block, draws.len());
        }
        docs.push(json!({
            "block": chain.block_version(&registry),
            "corpus": format!(
                "noise sigma={NULL_SIGMA_LSB}LSB 8-bit @{NULL_RATE_HZ}Hz via {}",
                chain_desc(&chain)
            ),
            "fill": { "sigma_lsb": NULL_SIGMA_LSB, "clip_fraction": 0.0 },
            "cells": cells,
        }));
    }
    println!("{}", serde_json::to_string(&docs).expect("json"));
}

fn chain_desc(c: &hk_synth::nullchain::NullChain) -> String {
    let r = c.recipe();
    r.nodes
        .iter()
        .map(|n| n.block.as_str())
        .collect::<Vec<_>>()
        .join(">")
}

fn hash(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    }) & 0xfff
}
