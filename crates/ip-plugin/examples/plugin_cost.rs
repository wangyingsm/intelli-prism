//! Measures what one plugin call costs in a release build, and what the pool saves.
//!
//! Both hosts are timed in one process, alternating within each round, because this machine's
//! clock settles at a different speed in each process and that difference is larger than the
//! one being measured. Each is reported as its fastest, middle and slowest round, since a
//! single figure invites a conclusion the spread does not support.

use std::hint::black_box;
use std::time::{Duration, Instant};

use ip_core::Checksum;
use ip_plugin::{PluginHost, PluginLimits};

/// A plugin that hands back exactly what it was given.
const ECHO: &str = r#"(module
  (memory (export "memory") 16)
  (func (export "alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "dealloc") (param i32 i32))
  (func (export "transform") (param i32 i32) (result i64)
    (i64.or
      (i64.shl (i64.extend_i32_u (local.get 0)) (i64.const 32))
      (i64.extend_i32_u (local.get 1)))))"#;

/// Calls timed per round.
const ROUNDS: u32 = 50_000;
/// Rounds run per host.
const REPEATS: usize = 9;

fn main() {
    let wasm = wat::parse_str(ECHO).unwrap();
    let checksum = Checksum::of(&wasm);
    let limits = PluginLimits::default();

    let pooled = PluginHost::new(limits).unwrap();
    pooled.load(&checksum, &wasm).unwrap();
    let on_demand = PluginHost::on_demand(limits).unwrap();
    on_demand.load(&checksum, &wasm).unwrap();
    let input = vec![7_u8; 4096];

    // A powersave governor idles this machine at its slowest clock, which must ramp before timing.
    let warmup = Instant::now();
    while warmup.elapsed() < Duration::from_millis(500) {
        call(&pooled, &checksum, &input);
        call(&on_demand, &checksum, &input);
    }

    let mut pooled_rounds = Vec::with_capacity(REPEATS);
    let mut on_demand_rounds = Vec::with_capacity(REPEATS);
    for _ in 0..REPEATS {
        pooled_rounds.push(time(&pooled, &checksum, &input));
        on_demand_rounds.push(time(&on_demand, &checksum, &input));
    }
    pooled_rounds.sort_unstable();
    on_demand_rounds.sort_unstable();

    report("pooled   ", &pooled_rounds);
    report("on demand", &on_demand_rounds);
}

/// Runs one whole call: instantiate, transform, and drop what it took.
fn call(host: &PluginHost, checksum: &Checksum, input: &[u8]) {
    black_box(
        host.instantiate(checksum)
            .unwrap()
            .transform(input)
            .unwrap(),
    );
}

/// Times a round of whole calls.
fn time(host: &PluginHost, checksum: &Checksum, input: &[u8]) -> Duration {
    let started = Instant::now();
    for _ in 0..ROUNDS {
        call(host, checksum, input);
    }
    started.elapsed()
}

/// Prints the fastest, middle and slowest round of a host, per call.
fn report(name: &str, taken: &[Duration]) {
    let per = |at: usize| taken[at].as_nanos() as f64 / f64::from(ROUNDS);
    println!(
        "{name}  min {:>8.0} ns   median {:>8.0} ns   max {:>8.0} ns",
        per(0),
        per(taken.len() / 2),
        per(taken.len() - 1)
    );
}
