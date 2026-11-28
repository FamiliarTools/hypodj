//! Measure the `searchall` fuzzy fallback (task 01m8bhyp) against the LIVE server,
//! read-only: the spike's 36 flawed queries (hits@5 = intended item in the first 5
//! of its kind, after ranking), per-query latency of the whole path and of the local
//! part, index build time and heap, and a nonsense precision set that must produce
//! no approximate hit.
//!
//! Usage: cargo run --release -j4 --bin fuzzy-bench -- <config.toml>
//! Prints no credentials (only names and numbers).

use hypodj_core::config::Config;
use hypodj_core::fuzzy::{self, FuzzyIndex};
use hypodj_core::handler::searchall_hits;
use hypodj_core::subsonic::SubsonicClient;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::Instant;

/// Counts live heap bytes, so the index's memory is MEASURED, not estimated.
struct Counting;
static LIVE: AtomicIsize = AtomicIsize::new(0);
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        LIVE.fetch_add(l.size() as isize, Ordering::Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size() as isize, Ordering::Relaxed);
        unsafe { System.dealloc(p, l) }
    }
}
#[global_allocator]
static A: Counting = Counting;

#[derive(Clone, Copy)]
enum K {
    Artist,
    Album,
    Song,
}

const Q: &[(&str, K, &str)] = &[
    ("bjork", K::Artist, "Björk"),
    ("bjrk", K::Artist, "Björk"),
    ("foehn", K::Artist, "Föhn"),
    ("fohn", K::Artist, "Föhn"),
    ("roman fluegel", K::Artist, "Roman Flügel"),
    ("roman flugel", K::Artist, "Roman Flügel"),
    ("vig mihaly", K::Artist, "Víg Mihály"),
    ("masive attack", K::Artist, "Massive Attack, Mos Def"),
    ("amon tobbin", K::Artist, "Amon Tobin"),
    ("amon tobn", K::Artist, "Amon Tobin"),
    ("tosca", K::Artist, "Tosca"),
    ("sakurazaka 46", K::Artist, "Sakurazaka46"),
    ("rolling stone", K::Artist, "The Rolling Stones"),
    ("rollingstones", K::Artist, "The Rolling Stones"),
    ("simon turner", K::Artist, "Simon Fisher Turner"),
    ("turner simon fisher", K::Artist, "Simon Fisher Turner"),
    ("bonnie prince billy", K::Artist, "Bonnie 'Prince' Billy"),
    ("life without buildngs", K::Artist, "Life Without Buildings"),
    ("pere ubuu", K::Artist, "Pere Ubu"),
    ("dntl", K::Artist, "Dntel"),
    ("klangstoff", K::Artist, "Klangstof"),
    ("suzanne craft", K::Artist, "Suzanne Kraft"),
    ("hunky dorry", K::Album, "Hunky Dory"),
    ("parklif", K::Album, "Parklife (Canadian Expanded Issue)"),
    ("modern life is rubish", K::Album, "Modern Life Is Rubbish (1993 Japanese Expanded Issue)"),
    ("bananenrauber", K::Album, "Bananenräuber EP"),
    ("any other citty", K::Album, "Any Other City"),
    ("i against i", K::Album, "I Against I"),
    ("eleven numbrs", K::Album, "Eleven Numbers"),
    ("kesz az egesz", K::Song, "Kész az egész"),
    ("hyperon", K::Song, "Hyperion"),
    ("feeling sleezy", K::Song, "Feeling Sleazy"),
    ("could be nise", K::Song, "Could Be Nice"),
    ("today i was evil one", K::Song, "Today I Was an Evil One"),
    ("dont loose the center", K::Song, "Don\u{2019}t Loose The Center feat. KAYYAK"),
    ("pain a rolin away", K::Song, "Pain A Rollin\u{2019}Away"),
];

/// Queries that name nothing in the library: any approximate hit is a false positive.
const NONSENSE: &[&str] = &[
    "xqzv plorth",
    "zzyzx quantum",
    "mmmmmmmm",
    "flibbertigibbet",
    "qwertyuiop",
    "glorp snarf",
    "vexillology handbook",
    "kxjw",
    "purple monkey dishwasher",
    "trombone jellyfish",
];

fn names(h: &fuzzy::Augmented, k: K) -> Vec<(String, bool)> {
    match k {
        K::Artist => h.artists.iter().map(|a| (a.name.clone(), h.approx.contains_key(&a.id.0))).collect(),
        K::Album => h.albums.iter().map(|a| (a.name.clone(), h.approx.contains_key(&a.id.0))).collect(),
        K::Song => h.songs.iter().map(|s| (s.title.clone(), h.approx.contains_key(&s.id.0))).collect(),
    }
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() - 1) as f64 * p).round() as usize]
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    let cfg_path = std::env::args().nth(1).unwrap_or_else(|| "hypodj.toml".into());
    let cfg = Config::load(std::path::Path::new(&cfg_path))?;
    let client = SubsonicClient::connect(&cfg.server)?;
    client.ping().await?;

    let t = Instant::now();
    let before = LIVE.load(Ordering::Relaxed);
    let ix: FuzzyIndex = fuzzy::fetch(&client).await?;
    let heap = LIVE.load(Ordering::Relaxed) - before;
    let (na, nb, ns) = ix.len();
    println!(
        "index: {na} artists, {nb} albums, {ns} songs; fetch+build {} ms; heap {:.2} MB (incl. the stored rows)",
        t.elapsed().as_millis(),
        heap as f64 / 1e6
    );

    let (mut base_hits, mut fuzzy_hits) = (0, 0);
    let (mut lat_total, mut lat_local) = (Vec::new(), Vec::new());
    for &(q, k, want) in Q {
        let base = searchall_hits(&client, None, q).await?;
        let b5 = names(&base, k).iter().take(5).any(|(n, _)| n == want);
        let t = Instant::now();
        let full = searchall_hits(&client, Some(&ix), q).await?;
        lat_total.push(t.elapsed().as_secs_f64() * 1e3);
        // The local half alone: augment + rank over the same server reply.
        let srv = searchall_hits(&client, None, q).await?;
        let t = Instant::now();
        let mut aug = fuzzy::augment(q, srv.artists, srv.albums, srv.songs, Some(&ix));
        hypodj_core::find_rank::rank(q, &mut aug.artists, &mut aug.albums, &mut aug.songs, &aug.approx);
        lat_local.push(t.elapsed().as_secs_f64() * 1e3);
        let top: Vec<(String, bool)> = names(&full, k).into_iter().take(5).collect();
        let pos = top.iter().position(|(n, _)| n == want);
        base_hits += usize::from(b5);
        fuzzy_hits += usize::from(pos.is_some());
        let how = match pos {
            Some(i) if top[i].1 => format!("#{} approx", i + 1),
            Some(i) => format!("#{} server", i + 1),
            None => "MISS".into(),
        };
        let raw = ix.best_scores(q)[k as usize];
        println!("  {:<24} base {:<4} fuzzy {:<12} best {:.2} top: {:?}", q, if b5 { "hit" } else { "miss" }, how, raw,
            top.iter().map(|(n, a)| if *a { format!("~{n}") } else { n.clone() }).collect::<Vec<_>>());
    }
    println!("hits@5: baseline {base_hits}/{}, with fuzzy {fuzzy_hits}/{}", Q.len(), Q.len());
    println!(
        "latency ms (whole searchall library path): median {:.1}, p90 {:.1}, max {:.1}",
        pct(&mut lat_total.clone(), 0.5), pct(&mut lat_total.clone(), 0.9), pct(&mut lat_total, 1.0)
    );
    println!(
        "latency ms (local augment+rank only): median {:.3}, p90 {:.3}, max {:.3}",
        pct(&mut lat_local.clone(), 0.5), pct(&mut lat_local.clone(), 0.9), pct(&mut lat_local, 1.0)
    );

    let mut false_pos = 0;
    for &q in NONSENSE {
        let h = searchall_hits(&client, Some(&ix), q).await?;
        let raw = ix.best_scores(q);
        false_pos += h.approx.len();
        println!("  nonsense {:<26} approximate rows {} (best raw scores a/al/s {:.2} {:.2} {:.2}, threshold {})",
            q, h.approx.len(), raw[0], raw[1], raw[2], fuzzy::THRESHOLD);
    }
    println!("precision: {false_pos} approximate rows across {} nonsense queries", NONSENSE.len());
    Ok(())
}
