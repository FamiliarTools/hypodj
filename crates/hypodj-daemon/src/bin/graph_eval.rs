//! The node graph's KILL CRITERION (docs/design/sensing-and-graph/design.md,
//! Part 2, "Acceptance checks"), measured rather than argued:
//!
//! 1. over the sampled seeds, the walk's mean adjacent edge cost is lower than the
//!    server (wire) order's in at least 80%;
//! 2. with "calmer", the walk's largest adjacent energy step is smaller than
//!    pull-only ordering's in at least 80%, with net energy still falling.
//!
//! If either fails, the edge is vocabulary, not capability, and the graph stops.
//!
//! Usage:
//!   graph-eval --synthetic [--seeds N] [--pool N]
//!   graph-eval --config CONFIG.toml --features DIR [--seeds N] [--pool N]
//!
//! The real mode only READS: the features dir (never written) and the server
//! (`getSong`, `getSimilarSongs2`). A seed or candidate without a valid measured
//! file is dropped, so the verdict is about measured tracks only. Exit status 0 =
//! both criteria hold, 2 = the kill criterion fired.

use hypodj_core::config::Config;
use hypodj_core::features::FileFeatureStore;
use hypodj_core::graph::{kill_criterion, synthetic_cases, EvalCase};
use hypodj_core::intelligence::FeatureStore;
use hypodj_core::model::SongId;
use hypodj_core::subsonic::SubsonicClient;
use std::path::PathBuf;

fn usage() -> ! {
    eprintln!(
        "usage: graph-eval --synthetic [--seeds N] [--pool N]\n       graph-eval --config CONFIG.toml --features DIR [--seeds N] [--pool N]"
    );
    std::process::exit(64)
}

/// FNV-1a: a stable, dependency-free order for sampling seeds.
fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3))
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    let (mut synthetic, mut config, mut dir, mut seeds, mut pool) = (false, None, None, 50usize, 20usize);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| usage());
        match a.as_str() {
            "--synthetic" => synthetic = true,
            "--config" => config = Some(PathBuf::from(val())),
            "--features" => dir = Some(PathBuf::from(val())),
            "--seeds" => seeds = val().parse().unwrap_or_else(|_| usage()),
            "--pool" => pool = val().parse().unwrap_or_else(|_| usage()),
            _ => usage(),
        }
    }
    let cases = if synthetic {
        println!("graph-eval: SYNTHETIC library ({seeds} seeds, pool {pool}); exercises the harness, says nothing about the real library");
        synthetic_cases(seeds, pool, 0x5eed)
    } else {
        let (Some(config), Some(dir)) = (config, dir) else { usage() };
        real_cases(&config, dir, seeds, pool).await?
    };
    let v = kill_criterion(&cases);
    let pct = |n: usize| if v.cases == 0 { 0.0 } else { 100.0 * n as f64 / v.cases as f64 };
    println!("seeds evaluated: {}", v.cases);
    println!("edge: walk beats wire order in {} ({:.0}%, need >= 80%)", v.edge_wins, pct(v.edge_wins));
    println!(
        "calmer: walk's largest step smaller than pull-only, net still down, in {} ({:.0}%, need >= 80%)",
        v.calmer_wins,
        pct(v.calmer_wins)
    );
    if v.passes() {
        println!("verdict: PASS - the edge is capability");
        Ok(())
    } else {
        println!("verdict: KILL - the edge is vocabulary, not capability");
        std::process::exit(2)
    }
}

async fn real_cases(config: &std::path::Path, dir: PathBuf, seeds: usize, pool: usize) -> anyhow::Result<Vec<EvalCase>> {
    let cfg = Config::load(config)?;
    let client = SubsonicClient::connect(&cfg.server)?;
    client.ping().await?;
    let mut ids: Vec<String> = std::fs::read_dir(&dir)?
        .flatten()
        .filter_map(|e| e.path().file_stem().and_then(|s| s.to_str()).map(str::to_string))
        .filter(|s| !s.starts_with('.'))
        .collect();
    ids.sort_by_key(|id| fnv(id));
    let store = tokio::task::spawn_blocking(move || FileFeatureStore::open(dir)).await?;
    println!("graph-eval: {} feature files, sampling up to {seeds} seeds, pool {pool}", store.count());
    let mut cases = Vec::new();
    let mut dropped = 0usize;
    for id in ids {
        if cases.len() >= seeds {
            break;
        }
        let Ok(seed) = client.song(&SongId(id.clone())).await else { continue };
        let Some(sf) = store.features(&seed).filter(|f| f.measured) else { continue };
        let similar = client.similar(&seed.id, Some(pool as i32)).await.unwrap_or_default();
        let wire: Vec<_> = similar
            .iter()
            .filter(|s| s.id != seed.id)
            .filter_map(|s| store.features(s).filter(|f| f.measured))
            .collect();
        dropped += similar.len().saturating_sub(wire.len());
        // Fewer than three measured candidates cannot show an ordering effect.
        if wire.len() >= 3 {
            cases.push(EvalCase { seed: sf, wire });
        }
    }
    println!("graph-eval: {dropped} unmeasured candidates dropped");
    Ok(cases)
}
