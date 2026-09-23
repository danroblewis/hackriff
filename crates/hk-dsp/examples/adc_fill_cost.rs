//! Per-block cost of `adc_fill_ci8` (T-750). A cost measurement, so it reports wall-clock.
//!
//! `cargo run --release -p hk-dsp --example adc_fill_cost`
//! Compare with the 65 536-sample block period at 20 Msps (3.28 ms): the capture-thread budget.
use hk_dsp::floor::adc_fill_ci8;
use std::time::Instant;

fn main() {
    const BLOCK: usize = 65_536;
    let mut s = 1u32;
    let buf: Vec<i8> = (0..BLOCK * 2)
        .map(|_| {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            ((s >> 24) as i8) / 8
        })
        .collect();
    let n = 2000;
    let t = Instant::now();
    let mut acc = 0f32;
    for _ in 0..n {
        acc += adc_fill_ci8(std::hint::black_box(&buf))
            .sigma_lsb
            .unwrap_or(0.0);
    }
    let per = t.elapsed().as_secs_f64() / n as f64;
    let period = BLOCK as f64 / 20e6;
    println!(
        "adc_fill_ci8: {:.1} us/block ({:.2}% of the {:.2} ms block period at 20 Msps) [{acc}]",
        per * 1e6,
        100.0 * per / period,
        period * 1e3
    );
}
