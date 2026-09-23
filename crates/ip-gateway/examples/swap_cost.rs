//! What a node's readers pay while its routing table is replaced under them.
//!
//! A reload builds a whole new table and swaps it in. The question this answers is whether
//! the swap stops the requests that are in flight: if readers had to wait for the writer,
//! every node reloading at once would stall the cluster together.
//!
//! Two arms run in alternating order each round, so neither gets the warm cache: one where
//! nothing swaps, one where a writer swaps as fast as a reload ever would and then far
//! faster. The reader loop is the one the gateway runs per request: load the table, resolve
//! a key.
//!
//! `cargo run --release -p ip-gateway --example swap_cost`

use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use ip_config::Config;
use ip_core::{AbsPath, ApiId, Endpoint, Host, Port, Protocol, RouteKey, RouteRule, RouteTarget};
use ip_gateway::RoutingTable;

/// Rules in the table, which is more than a deployment starts with.
const RULES: usize = 1_000;

/// Readers hammering the table at once.
const READERS: usize = 8;

/// How long each arm runs.
const ARM: Duration = Duration::from_millis(400);

/// Resolves per timing sample, so the clock costs little of what is measured.
const BATCH: usize = 64;

/// Rounds, each running both arms in alternating order.
const ROUNDS: usize = 6;

const CONFIG: &str = r#"
[server]
listen = "127.0.0.1:8080"

[storage]
backend = "sqlite"
path = "./unopened.db"

[cache]
backend = "sled"
path = "./unopened"

[auth.jwt]
issuer = "intelli-prism"
secret = "0123456789abcdef0123456789abcdef"
"#;

fn endpoint(host: &str, path: &str) -> Endpoint {
    Endpoint::new(
        Protocol::Https,
        Host::new(host).unwrap(),
        Port::new(443).unwrap(),
        AbsPath::new(path).unwrap(),
    )
}

/// A table of `rules` stored rules, all on one host so lookups scan one authority group.
fn table(rules: usize, target: &str) -> RoutingTable {
    let config = Config::parse(CONFIG).unwrap();
    let dynamic = (0..rules)
        .map(|n| RouteRule {
            api: ApiId::new("chat").unwrap(),
            key: RouteKey::new(endpoint("gateway.local", &format!("/api-{n}"))),
            target: RouteTarget::new(endpoint(target, "/v1")),
        })
        .collect();
    RoutingTable::build(&config, dynamic).unwrap()
}

/// The nanoseconds one resolve took, over every sample.
fn read_for(held: &Arc<ArcSwap<RoutingTable>>, until: Instant, samples: &mut Vec<u64>) {
    let key = RouteKey::new(endpoint("gateway.local", "/api-999/chat"));
    while Instant::now() < until {
        let started = Instant::now();
        for _ in 0..BATCH {
            let table = held.load();
            black_box(table.resolve(&key));
        }
        let each = started.elapsed().as_nanos() / BATCH as u128;
        samples.push(u64::try_from(each).unwrap_or(u64::MAX));
    }
}

/// Runs every reader for one arm, with a writer swapping every `swap_every` when it is set.
fn arm(held: &Arc<ArcSwap<RoutingTable>>, swap_every: Option<Duration>) -> (Vec<u64>, u64) {
    let until = Instant::now() + ARM;
    let swapping = Arc::new(AtomicBool::new(true));
    let swaps = Arc::new(AtomicU64::new(0));
    let writer = swap_every.map(|every| {
        let held = Arc::clone(held);
        let swapping = Arc::clone(&swapping);
        let swaps = Arc::clone(&swaps);
        // The tables are built up front: a reload builds off the request path, and what is
        // measured here is the swap, not the building.
        let tables = [
            Arc::new(table(RULES, "one.example.com")),
            Arc::new(table(RULES, "two.example.com")),
        ];
        thread::spawn(move || {
            let mut next = 0;
            while swapping.load(Ordering::Relaxed) {
                held.store(Arc::clone(&tables[next % 2]));
                swaps.fetch_add(1, Ordering::Relaxed);
                next += 1;
                thread::sleep(every);
            }
        })
    });

    let readers: Vec<_> = (0..READERS)
        .map(|_| {
            let held = Arc::clone(held);
            thread::spawn(move || {
                let mut samples = Vec::with_capacity(4_096);
                read_for(&held, until, &mut samples);
                samples
            })
        })
        .collect();
    let mut samples: Vec<u64> = readers
        .into_iter()
        .flat_map(|reader| reader.join().unwrap())
        .collect();
    swapping.store(false, Ordering::Relaxed);
    if let Some(writer) = writer {
        writer.join().unwrap();
    }
    samples.sort_unstable();
    (samples, swaps.load(Ordering::Relaxed))
}

fn at(samples: &[u64], share: f64) -> u64 {
    let at = ((samples.len() as f64 - 1.0) * share) as usize;
    samples[at]
}

fn report(name: &str, unit: &str, rounds: &mut [u64]) {
    rounds.sort_unstable();
    println!(
        "{name:<34} min {:>7} {unit}   median {:>7} {unit}   max {:>7} {unit}",
        rounds[0],
        rounds[rounds.len() / 2],
        rounds[rounds.len() - 1]
    );
}

fn main() {
    println!(
        "{RULES} rules, {READERS} readers, {}ms per arm, {ROUNDS} rounds\n",
        ARM.as_millis()
    );

    let mut quiet_median = Vec::new();
    let mut quiet_worst = Vec::new();
    let mut swapping_median = Vec::new();
    let mut swapping_worst = Vec::new();
    let mut swapping_tail = Vec::new();
    let mut quiet_tail = Vec::new();
    let mut swaps_seen = 0;

    for round in 0..ROUNDS {
        let held = Arc::new(ArcSwap::from_pointee(table(RULES, "one.example.com")));
        // The arms alternate so neither always runs on the warm cache the other left.
        let order = [round % 2 == 0, round % 2 != 0];
        for quiet_first in order {
            if quiet_first {
                let (samples, _) = arm(&held, None);
                quiet_median.push(at(&samples, 0.5));
                quiet_tail.push(at(&samples, 0.999));
                quiet_worst.push(*samples.last().unwrap());
            } else {
                let (samples, swaps) = arm(&held, Some(Duration::from_micros(200)));
                swaps_seen += swaps;
                swapping_median.push(at(&samples, 0.5));
                swapping_tail.push(at(&samples, 0.999));
                swapping_worst.push(*samples.last().unwrap());
            }
        }
    }

    println!("one resolve, over {ROUNDS} rounds of each arm");
    report("  nothing swapping, median", "ns", &mut quiet_median);
    report("  swapping every 200us, median", "ns", &mut swapping_median);
    report("  nothing swapping, 99.9th", "ns", &mut quiet_tail);
    report("  swapping every 200us, 99.9th", "ns", &mut swapping_tail);
    report("  nothing swapping, worst", "ns", &mut quiet_worst);
    report("  swapping every 200us, worst", "ns", &mut swapping_worst);
    println!("  swaps made while reading: {swaps_seen}");

    let held = Arc::new(ArcSwap::from_pointee(table(RULES, "one.example.com")));
    let replacement = Arc::new(table(RULES, "two.example.com"));
    let mut stores = Vec::new();
    for _ in 0..ROUNDS {
        let started = Instant::now();
        for _ in 0..10_000 {
            held.store(Arc::clone(&replacement));
        }
        stores.push(u64::try_from(started.elapsed().as_nanos() / 10_000).unwrap_or(u64::MAX));
    }
    println!("\nthe swap itself, nothing reading");
    report("  one store", "ns", &mut stores);

    let mut builds = Vec::new();
    for _ in 0..ROUNDS {
        let started = Instant::now();
        black_box(table(RULES, "one.example.com"));
        builds.push(u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX));
    }
    let mut micros: Vec<u64> = builds.iter().map(|each| each / 1_000).collect();
    println!("\nbuilding the table a reload swaps in, {RULES} rules");
    report("  one build", "us", &mut micros);
}
