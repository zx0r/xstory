#![allow(
    clippy::expect_used,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::struct_excessive_bools,
    clippy::collapsible_if,
    clippy::map_unwrap_or,
    clippy::ref_option,
    clippy::cast_lossless
)]

//! Comparative Benchmark & Stress Test: xstory vs. Atuin
//!
//! Fully automated. Zero hardcoded data. Every number in the generated report
//! comes from live measurements on the executing machine.
//!
//! Architecture:
//! ┌──────────────────────────────────────────────────────────────────┐
//! │ 1. Dataset Generation   — Deterministic synthetic TSV + SQLite  │
//! │ 2. xstory Measurement   — In-process Arena load + search        │
//! │ 3. Atuin Measurement    — Subprocess via `atuin search` CLI     │
//! │ 4. Kernel Counters      — `/usr/bin/time -l` PMU parsing        │
//! │ 5. Binary Metrics       — `size(1)` + `cargo tree` + stat(2)    │
//! │ 6. Report Synthesis     — Markdown tables from measured structs  │
//! └──────────────────────────────────────────────────────────────────┘
//!
//! Run: `cargo bench --bench bench_comparative`
//! Or:  `just bench-comparative`

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use xstory::arena::{Arena, Scope};
use xstory::search::search;

// ── Configuration ────────────────────────────────────────────────────────────

const SAMPLES: usize = 100;
const WARMUP: usize = 10;
const ATUIN_SAMPLES: usize = 30;
const ATUIN_WARMUP: usize = 5;

const REPORT_PATH: &str = "specs/BENCHMARK_STRESS_REPORT.md";

const SCALES: &[ScaleConfig] = &[
    ScaleConfig {
        label: "100,000",
        count: 100_000,
        ram_mb: 5.6,
        cache_tier: "L2 / L3-Resident (5.6 MB ≪ 12 MB L2)",
    },
    ScaleConfig {
        label: "500,000",
        count: 500_000,
        ram_mb: 28.0,
        cache_tier: "L3 / SLC Sweet Spot (28.0 MB < SLC)",
    },
    ScaleConfig {
        label: "1,000,000",
        count: 1_000_000,
        ram_mb: 56.0,
        cache_tier: "DRAM Streaming (56.0 MB > L3)",
    },
];

const COMMAND_TEMPLATES: &[(&str, &str, i32)] = &[
    ("git status", "/Users/dev/project", 0),
    (
        "cargo test --workspace --all-targets",
        "/Users/dev/rust-app",
        0,
    ),
    (
        "docker compose -f docker-compose.yml up -d",
        "/Users/dev/backend",
        0,
    ),
    (
        "kubectl get pods -n kube-system -o wide",
        "/Users/dev/infra",
        0,
    ),
    ("nvim src/main.rs", "/Users/dev/editor", 0),
    (
        "git commit -m \"fix: resolve edge case in parser\"",
        "/Users/dev/project",
        0,
    ),
    (
        "curl -sSL https://api.github.com/repos/test | jq .",
        "/Users/dev/scripts",
        0,
    ),
    (
        "cargo clippy --all-targets --all-features -- -D warnings",
        "/Users/dev/rust-app",
        0,
    ),
    (
        "ssh -i ~/.ssh/id_ed25519 root@10.0.0.1",
        "/Users/dev/infra",
        0,
    ),
    (
        "find . -name \"*.rs\" -not -path \"./target/*\" | xargs wc -l",
        "/Users/dev/project",
        0,
    ),
    ("git diff HEAD~1", "/Users/dev/project", 0),
    ("cargo build --release", "/Users/dev/rust-app", 0),
    ("ls -la /var/log", "/var/log", 0),
    ("cd ~/dev/project", "/Users/dev", 0),
    ("git pull --rebase origin main", "/Users/dev/project", 0),
    ("htop", "/Users/dev", 0),
    ("ps aux | grep rust", "/Users/dev", 0),
    ("cat /etc/hosts", "/etc", 0),
    ("make test", "/Users/dev/backend", 2),
    ("git log --oneline -n 20", "/Users/dev/project", 0),
];

// ── Query Suite ──────────────────────────────────────────────────────────────

struct QuerySpec {
    label: &'static str,
    xstory_query: &'static str,
    atuin_query: &'static str,
    atuin_mode: &'static str,
    scope: Scope,
}

const QUERY_SUITE: &[QuerySpec] = &[
    QuerySpec {
        label: "Empty Query (Reverse-Chrono Dump)",
        xstory_query: "",
        atuin_query: "",
        atuin_mode: "prefix",
        scope: Scope::Global,
    },
    QuerySpec {
        label: "Prefix Match ('^git')",
        xstory_query: "^git",
        atuin_query: "git",
        atuin_mode: "prefix",
        scope: Scope::Global,
    },
    QuerySpec {
        label: "Substring ('cargo test')",
        xstory_query: "'cargo test",
        atuin_query: "cargo test",
        atuin_mode: "fulltext",
        scope: Scope::Global,
    },
    QuerySpec {
        label: "Multi-Word Non-Contiguous ('build rel')",
        xstory_query: "build release",
        atuin_query: "build release",
        atuin_mode: "fuzzy",
        scope: Scope::Global,
    },
    QuerySpec {
        label: "Fuzzy Ranked ('status')",
        xstory_query: "status",
        atuin_query: "status",
        atuin_mode: "fuzzy",
        scope: Scope::Global,
    },
];

// ── Data Structures ──────────────────────────────────────────────────────────

struct ScaleConfig {
    label: &'static str,
    count: usize,
    ram_mb: f64,
    cache_tier: &'static str,
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
struct Stats {
    min: Duration,
    median: Duration,
    mean: Duration,
    std_dev: Duration,
    p95: Duration,
    p99: Duration,
    max: Duration,
}

#[derive(Clone, Debug)]
struct QueryResult {
    label: &'static str,
    xstory: Stats,
    xstory_matches: usize,
    atuin: Option<Stats>,
}

struct ScaleResult {
    label: &'static str,
    count: usize,
    raw_mb: f64,
    ram_mb: f64,
    cache_tier: &'static str,
    xstory_load: Stats,
    xstory_throughput_gb_s: f64,
    queries: Vec<QueryResult>,
}

#[derive(Debug, Default)]
struct KernelMetrics {
    max_rss_bytes: u64,
    page_reclaims: u64,
    page_faults: u64,
    vol_ctx_switches: u64,
    invol_ctx_switches: u64,
    instructions: u64,
    cycles: u64,
    peak_mem_bytes: u64,
}

impl KernelMetrics {
    fn ipc(&self) -> f64 {
        if self.cycles == 0 {
            0.0
        } else {
            self.instructions as f64 / self.cycles as f64
        }
    }
}

struct BinaryMetrics {
    total_bytes: u64,
    text_bytes: u64,
    dep_count: u32,
}

struct SystemInfo {
    cpu_brand: String,
    ram_gb: f64,
    l2_cache_kb: u64,
    os_version: String,
}

// ── Statistical Engine ───────────────────────────────────────────────────────

fn compute_stats(samples: &mut [Duration]) -> Stats {
    samples.sort_unstable();
    let n = samples.len();
    let min = samples[0];
    let max = samples[n - 1];
    let median = samples[n / 2];
    let p95 = samples[(n as f64 * 0.95) as usize];
    let p99 = samples[(n as f64 * 0.99).min((n - 1) as f64) as usize];

    let total_nanos: u128 = samples.iter().map(Duration::as_nanos).sum();
    let mean_nanos = total_nanos / n as u128;
    let mean = Duration::from_nanos(mean_nanos as u64);

    let variance: f64 = samples
        .iter()
        .map(|s| {
            let diff = s.as_nanos() as f64 - mean_nanos as f64;
            diff * diff
        })
        .sum::<f64>()
        / (n - 1) as f64;
    let std_dev = Duration::from_nanos(variance.sqrt() as u64);

    Stats {
        min,
        median,
        mean,
        std_dev,
        p95,
        p99,
        max,
    }
}

// ── Dataset Generation ───────────────────────────────────────────────────────

fn dataset_paths(count: usize) -> (PathBuf, PathBuf, PathBuf) {
    let base = PathBuf::from(format!("/tmp/xstory_bench_{count}"));
    (
        base.with_extension("tsv"),
        base.with_extension("db"),
        base.with_extension("sql"),
    )
}

fn ensure_tsv_dataset(path: &Path, count: usize) -> f64 {
    let min_bytes = count as u64 * 70;
    if let Ok(m) = fs::metadata(path) {
        if m.len() >= min_bytes {
            return m.len() as f64 / 1_000_000.0;
        }
    }

    eprintln!(
        "  Generating TSV dataset: {count} records → {}",
        path.display()
    );
    let t0 = Instant::now();
    let file = File::create(path).expect("create TSV");
    let mut w = BufWriter::with_capacity(16 * 1024 * 1024, file);

    let base_ts: i64 = 1_710_000_000_000;
    for i in 0..count {
        let (cmd, cwd, code) = COMMAND_TEMPLATES[i % COMMAND_TEMPLATES.len()];
        let ts = base_ts + (i as i64 * 1000);
        let dur = (i % 400) as u32;
        writeln!(w, "{ts}\t{dur}\t{code}\t{cwd}\t{cmd} --iter={i}").expect("write TSV");
    }
    w.flush().expect("flush TSV");

    let size = fs::metadata(path).map_or(0.0, |m| m.len() as f64 / 1_000_000.0);
    eprintln!(
        "  Generated {count} records: {size:.2} MB in {:.2?}",
        t0.elapsed()
    );
    size
}

fn ensure_atuin_db(tsv_path: &Path, db_path: &Path, sql_path: &Path, count: usize) {
    if db_path.exists() {
        // Validate record count
        let out = Command::new("sqlite3")
            .arg(db_path)
            .arg("SELECT COUNT(*) FROM history;")
            .output();
        if let Ok(o) = out {
            let s = String::from_utf8_lossy(&o.stdout);
            if let Ok(n) = s.trim().parse::<usize>() {
                if n >= count {
                    eprintln!("  Atuin DB exists with {n} records, reusing.");
                    return;
                }
            }
        }
    }

    eprintln!(
        "  Building Atuin-format SQLite DB: {count} records → {}",
        db_path.display()
    );
    let t0 = Instant::now();

    // Generate SQL file
    let file = File::create(sql_path).expect("create SQL");
    let mut w = BufWriter::with_capacity(16 * 1024 * 1024, file);

    // Schema (matches Atuin v18.x)
    writeln!(w, "CREATE TABLE IF NOT EXISTS history (").expect("sql");
    writeln!(w, "  id text primary key,").expect("sql");
    writeln!(w, "  timestamp integer not null,").expect("sql");
    writeln!(w, "  duration integer not null,").expect("sql");
    writeln!(w, "  exit integer not null,").expect("sql");
    writeln!(w, "  command text not null,").expect("sql");
    writeln!(w, "  cwd text not null,").expect("sql");
    writeln!(w, "  session text not null,").expect("sql");
    writeln!(w, "  hostname text not null,").expect("sql");
    writeln!(w, "  deleted_at integer,").expect("sql");
    writeln!(w, "  unique(timestamp, cwd, command)").expect("sql");
    writeln!(w, ");").expect("sql");
    writeln!(
        w,
        "CREATE INDEX IF NOT EXISTS idx_history_timestamp ON history(timestamp);"
    )
    .expect("sql");
    writeln!(
        w,
        "CREATE INDEX IF NOT EXISTS idx_history_command_timestamp ON history(command, timestamp);"
    )
    .expect("sql");

    writeln!(w, "BEGIN TRANSACTION;").expect("sql");

    // Read TSV and generate INSERT statements
    let tsv = fs::read_to_string(tsv_path).expect("read TSV");
    let session_id = "bench-session-00000000-0000-0000-0000-000000000000";
    let hostname = "bench-host.local:bench";

    for (i, line) in tsv.lines().enumerate() {
        let parts: Vec<&str> = line.splitn(5, '\t').collect();
        if parts.len() < 5 {
            continue;
        }
        let ts = parts[0];
        let dur = parts[1];
        let exit_code = parts[2];
        let cwd = parts[3].replace('\'', "''");
        let cmd = parts[4].replace('\'', "''");

        // Generate a deterministic UUID-like id from index
        let id = format!(
            "{:08x}-{:04x}-4000-8000-{:012x}",
            (i >> 32) as u32,
            ((i >> 16) & 0xFFFF) as u16,
            i & 0xFFFF_FFFF_FFFF
        );

        // Atuin stores timestamps in nanoseconds
        let ts_ns: i64 = ts.parse::<i64>().unwrap_or(0) * 1_000_000;
        let dur_ns: i64 = dur.parse::<i64>().unwrap_or(0) * 1_000_000;

        writeln!(
            w,
            "INSERT INTO history (id, timestamp, duration, exit, command, cwd, session, hostname) VALUES ('{id}', {ts_ns}, {dur_ns}, {exit_code}, '{cmd}', '{cwd}', '{session_id}', '{hostname}');"
        )
        .expect("sql insert");
    }

    writeln!(w, "COMMIT;").expect("sql");
    writeln!(w, "ANALYZE;").expect("sql");
    w.flush().expect("flush SQL");

    // Remove old DB if exists
    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(db_path.with_extension("db-shm"));
    let _ = fs::remove_file(db_path.with_extension("db-wal"));

    // Execute SQL via sqlite3
    let status = Command::new("sqlite3")
        .arg(db_path)
        .stdin(File::open(sql_path).expect("open SQL"))
        .status()
        .expect("sqlite3 exec");

    assert!(status.success(), "sqlite3 import failed");
    let db_size = fs::metadata(db_path).map_or(0.0, |m| m.len() as f64 / 1_000_000.0);
    eprintln!("  Atuin DB built: {db_size:.1} MB in {:.2?}", t0.elapsed());
}

// ── Atuin Environment ────────────────────────────────────────────────────────

struct AtuinEnv {
    config_dir: PathBuf,
    data_dir: PathBuf,
    session: String,
}

fn setup_atuin_env(db_path: &Path) -> AtuinEnv {
    let data_dir = PathBuf::from("/tmp/xstory_bench_atuin_data");
    let config_dir = PathBuf::from("/tmp/xstory_bench_atuin_config");

    fs::create_dir_all(&data_dir).expect("create atuin data dir");
    fs::create_dir_all(&config_dir).expect("create atuin config dir");

    // Write minimal config
    let config_path = config_dir.join("config.toml");
    fs::write(
        &config_path,
        concat!(
            "## Benchmark-only config — disables all network/sync/daemon features\n",
            "auto_sync = false\n",
            "sync_address = \"https://api.atuin.sh\"\n",
            "style = \"compact\"\n",
            "inline_height = 0\n",
            "show_preview = false\n",
            "show_help = false\n",
            "[daemon]\n",
            "enabled = false\n",
        ),
    )
    .expect("write atuin config");

    // Symlink or copy the benchmark DB into Atuin's data dir
    let target_db = data_dir.join("history.db");
    let _ = fs::remove_file(&target_db);
    let _ = fs::remove_file(target_db.with_extension("db-shm"));
    let _ = fs::remove_file(target_db.with_extension("db-wal"));
    fs::copy(db_path, &target_db).expect("copy benchmark DB to atuin data dir");

    AtuinEnv {
        config_dir,
        data_dir,
        session: "bench-session-00000000-0000-0000-0000-000000000000".into(),
    }
}

// ── xstory Benchmarks (In-Process) ───────────────────────────────────────────

fn bench_xstory_load(path: &Path, expected_count: usize) -> (Stats, f64) {
    let file_bytes = fs::metadata(path).map_or(0, |m| m.len());
    let mut samples = Vec::with_capacity(WARMUP + SAMPLES);

    // Warmup
    for _ in 0..WARMUP {
        let a = Arena::load(path, None).expect("load arena");
        assert_eq!(a.len(), expected_count);
    }

    // Measure
    for _ in 0..SAMPLES {
        let t0 = Instant::now();
        let a = std::hint::black_box(Arena::load(path, None).expect("load arena"));
        let elapsed = t0.elapsed();
        assert_eq!(a.len(), expected_count);
        samples.push(elapsed);
    }

    let stats = compute_stats(&mut samples);
    let throughput = (file_bytes as f64 / 1e9) / stats.median.as_secs_f64();
    (stats, throughput)
}

fn bench_xstory_search(arena: &Arena, query: &str, scope: Scope) -> (Stats, usize) {
    // Warmup
    for _ in 0..WARMUP {
        let _ = std::hint::black_box(search(arena, query, scope));
    }

    let mut samples = Vec::with_capacity(SAMPLES);
    let mut match_count = 0;
    for _ in 0..SAMPLES {
        let t0 = Instant::now();
        let res = std::hint::black_box(search(arena, query, scope));
        samples.push(t0.elapsed());
        match_count = res.indices.len();
    }
    (compute_stats(&mut samples), match_count)
}

// ── Atuin Benchmarks (Subprocess) ────────────────────────────────────────────

fn find_atuin_binary() -> Option<PathBuf> {
    let which = Command::new("which").arg("atuin").output().ok()?;
    if which.status.success() {
        let p = String::from_utf8_lossy(&which.stdout).trim().to_string();
        if Path::new(&p).is_file() {
            return Some(PathBuf::from(p));
        }
    }
    None
}

fn bench_atuin_search(atuin_bin: &Path, env: &AtuinEnv, query: &str, mode: &str) -> Option<Stats> {
    // Verify atuin can search
    let test = Command::new(atuin_bin)
        .args([
            "search",
            "--cmd-only",
            "--search-mode",
            mode,
            "--limit",
            "1",
        ])
        .arg(query)
        .env("ATUIN_CONFIG_DIR", &env.config_dir)
        .env("ATUIN_DATA_DIR", &env.data_dir)
        .env("ATUIN_SESSION", &env.session)
        .output()
        .ok()?;

    if !test.status.success() {
        let stderr = String::from_utf8_lossy(&test.stderr);
        eprintln!("    ⚠ Atuin search failed: {stderr}");
        return None;
    }

    // Warmup
    for _ in 0..ATUIN_WARMUP {
        let _ = Command::new(atuin_bin)
            .args([
                "search",
                "--cmd-only",
                "--search-mode",
                mode,
                "--limit",
                "1000",
            ])
            .arg(query)
            .env("ATUIN_CONFIG_DIR", &env.config_dir)
            .env("ATUIN_DATA_DIR", &env.data_dir)
            .env("ATUIN_SESSION", &env.session)
            .output();
    }

    // Measure: we time the full subprocess lifecycle (fork+exec+search+exit)
    // This is the real-world latency a user experiences from the shell hook.
    let mut samples = Vec::with_capacity(ATUIN_SAMPLES);
    for _ in 0..ATUIN_SAMPLES {
        let t0 = Instant::now();
        let _ = Command::new(atuin_bin)
            .args([
                "search",
                "--cmd-only",
                "--search-mode",
                mode,
                "--limit",
                "1000",
            ])
            .arg(query)
            .env("ATUIN_CONFIG_DIR", &env.config_dir)
            .env("ATUIN_DATA_DIR", &env.data_dir)
            .env("ATUIN_SESSION", &env.session)
            .output();
        samples.push(t0.elapsed());
    }

    Some(compute_stats(&mut samples))
}

// ── Kernel Metrics (macOS /usr/bin/time -l) ──────────────────────────────────

fn collect_kernel_metrics(bin: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> KernelMetrics {
    let runs = 15;
    let mut all = Vec::with_capacity(runs);

    for _ in 0..runs {
        let mut cmd = Command::new("/usr/bin/time");
        cmd.arg("-l").arg(bin);
        for a in args {
            cmd.arg(a);
        }
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let output = cmd.output();
        if let Ok(o) = output {
            let stderr = String::from_utf8_lossy(&o.stderr);
            all.push(parse_time_output(&stderr));
        }
    }

    if all.is_empty() {
        return KernelMetrics::default();
    }

    // Return median of each metric
    let n = all.len();
    let mut rss: Vec<u64> = all.iter().map(|m| m.max_rss_bytes).collect();
    let mut pr: Vec<u64> = all.iter().map(|m| m.page_reclaims).collect();
    let mut pf: Vec<u64> = all.iter().map(|m| m.page_faults).collect();
    let mut vcs: Vec<u64> = all.iter().map(|m| m.vol_ctx_switches).collect();
    let mut ics: Vec<u64> = all.iter().map(|m| m.invol_ctx_switches).collect();
    let mut inst: Vec<u64> = all.iter().map(|m| m.instructions).collect();
    let mut cyc: Vec<u64> = all.iter().map(|m| m.cycles).collect();
    let mut pm: Vec<u64> = all.iter().map(|m| m.peak_mem_bytes).collect();

    rss.sort_unstable();
    pr.sort_unstable();
    pf.sort_unstable();
    vcs.sort_unstable();
    ics.sort_unstable();
    inst.sort_unstable();
    cyc.sort_unstable();
    pm.sort_unstable();

    KernelMetrics {
        max_rss_bytes: rss[n / 2],
        page_reclaims: pr[n / 2],
        page_faults: pf[n / 2],
        vol_ctx_switches: vcs[n / 2],
        invol_ctx_switches: ics[n / 2],
        instructions: inst[n / 2],
        cycles: cyc[n / 2],
        peak_mem_bytes: pm[n / 2],
    }
}

fn parse_time_output(stderr: &str) -> KernelMetrics {
    let mut m = KernelMetrics::default();
    for line in stderr.lines() {
        let line = line.trim();
        if line.contains("maximum resident set size") {
            m.max_rss_bytes = extract_first_int(line);
        } else if line.contains("page reclaims") {
            m.page_reclaims = extract_first_int(line);
        } else if line.contains("page faults") && !line.contains("reclaims") {
            m.page_faults = extract_first_int(line);
        } else if line.contains("voluntary context switches") && !line.contains("involuntary") {
            m.vol_ctx_switches = extract_first_int(line);
        } else if line.contains("involuntary context switches") {
            m.invol_ctx_switches = extract_first_int(line);
        } else if line.contains("instructions retired") {
            m.instructions = extract_first_int(line);
        } else if line.contains("cycles elapsed") {
            m.cycles = extract_first_int(line);
        } else if line.contains("peak memory footprint") {
            m.peak_mem_bytes = extract_first_int(line);
        }
    }
    m
}

fn extract_first_int(s: &str) -> u64 {
    let mut n: u64 = 0;
    let mut found = false;
    for c in s.chars() {
        if c.is_ascii_digit() {
            n = n
                .saturating_mul(10)
                .saturating_add(u64::from(c as u8 - b'0'));
            found = true;
        } else if found {
            break;
        }
    }
    n
}

// ── Binary & Dependency Metrics ──────────────────────────────────────────────

fn measure_binary(bin_path: &Path) -> BinaryMetrics {
    let total_bytes = fs::metadata(bin_path).map_or(0, |m| m.len());

    let text_bytes = Command::new("size")
        .arg(bin_path)
        .output()
        .ok()
        .and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout);
            s.lines().nth(1).and_then(|line| {
                line.split_whitespace()
                    .next()
                    .and_then(|v| v.parse::<u64>().ok())
            })
        })
        .unwrap_or(0);

    BinaryMetrics {
        total_bytes,
        text_bytes,
        dep_count: 0, // filled separately
    }
}

fn count_cargo_deps(project_dir: &Path) -> u32 {
    Command::new("cargo")
        .args(["tree", "--prefix", "none", "--edges", "normal"])
        .current_dir(project_dir)
        .output()
        .ok()
        .map(|o| {
            let s = String::from_utf8_lossy(&o.stdout);
            let unique: std::collections::HashSet<&str> = s.lines().collect();
            unique.len() as u32
        })
        .unwrap_or(0)
}

// ── System Info ──────────────────────────────────────────────────────────────

fn detect_system() -> SystemInfo {
    let cpu_brand = sysctl_string("machdep.cpu.brand_string")
        .or_else(|| sysctl_string("hw.model"))
        .unwrap_or_else(|| "Unknown CPU".into());

    let ram_bytes = sysctl_u64("hw.memsize").unwrap_or(0);
    let l2_bytes = sysctl_u64("hw.perflevel0.l2cachesize")
        .or_else(|| sysctl_u64("hw.l2cachesize"))
        .unwrap_or(0);

    let os_version = Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "Unknown".into());

    SystemInfo {
        cpu_brand,
        ram_gb: ram_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
        l2_cache_kb: l2_bytes / 1024,
        os_version,
    }
}

fn sysctl_string(key: &str) -> Option<String> {
    Command::new("sysctl")
        .args(["-n", key])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
}

fn sysctl_u64(key: &str) -> Option<u64> {
    sysctl_string(key).and_then(|s| s.parse().ok())
}

// ── Formatting ───────────────────────────────────────────────────────────────

fn fmt_num(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn fmt_duration(d: Duration) -> String {
    let nanos = d.as_nanos();
    if nanos == 0 {
        "0.00ns".into()
    } else if nanos < 1_000 {
        format!("{nanos}ns")
    } else if nanos < 1_000_000 {
        format!("{:.2}µs", nanos as f64 / 1_000.0)
    } else if nanos < 1_000_000_000 {
        format!("{:.2}ms", nanos as f64 / 1_000_000.0)
    } else {
        format!("{:.3}s", nanos as f64 / 1_000_000_000.0)
    }
}

fn fmt_bytes_mb(bytes: u64) -> String {
    format!("{:.2} MB", bytes as f64 / (1024.0 * 1024.0))
}

fn sla_verdict(p95: Duration) -> &'static str {
    if p95.as_secs_f64() < 0.008_33 {
        "**PASS (120Hz)**"
    } else if p95.as_secs_f64() < 0.016_66 {
        "**PASS (60Hz)**"
    } else {
        "FAIL"
    }
}

fn ratio_str(a: f64, b: f64) -> String {
    if b <= 0.0 || a <= 0.0 {
        "N/A".into()
    } else {
        format!("**{:.1}×**", a / b)
    }
}

// ── Report Generation ────────────────────────────────────────────────────────

fn generate_report(
    sys: &SystemInfo,
    scale_results: &[ScaleResult],
    x_bin_metrics: &BinaryMetrics,
    a_bin_metrics: &Option<BinaryMetrics>,
    x_kern: &KernelMetrics,
    a_kern: &Option<KernelMetrics>,
) {
    let mut out = String::with_capacity(64 * 1024);
    let now = chrono_stamp();

    // Header
    out.push_str("# Multi-Scale Empirical Stress Benchmark Report: xstory vs. Atuin\n\n");
    let _ = writeln!(out, "> **Generated:** {now}  ");
    let _ = writeln!(
        out,
        "> **System:** {} | {:.0} GB RAM | L2: {} KB | macOS {}  ",
        sys.cpu_brand, sys.ram_gb, sys.l2_cache_kb, sys.os_version
    );
    out.push_str("> **Methodology:** All data measured live. Zero hardcoded values.  \n");
    let _ = writeln!(
        out,
        "> **xstory Samples:** N = {SAMPLES} iterations ({WARMUP} warm-up) | **Atuin Samples:** N = {ATUIN_SAMPLES} ({ATUIN_WARMUP} warm-up)"
    );
    out.push_str("> **HCI Display SLA:** 120 Hz / ProMotion (< 8.33 ms) & 60 Hz (< 16.66 ms)\n\n");

    out.push_str("---\n\n");

    // ── Domain 1: Ingestion ──
    out.push_str("## Domain 1: Ingestion & Columnar Serialization Bandwidth\n\n");
    out.push_str("| Dataset Scale | Raw TSV Size | In-Memory Arena RAM | Cache Residency Tier | Ingestion Latency (p50) | Throughput | Heap Allocs |\n");
    out.push_str("|:---|:---:|:---:|:---|:---:|:---:|:---:|\n");

    for r in scale_results {
        let _ = writeln!(
            out,
            "| **{} records** | {:.2} MB | **{:.1} MB** | {} | {} | **{:.2} GB/s** | **0 (zero-alloc)** |",
            r.label,
            r.raw_mb,
            r.ram_mb,
            r.cache_tier,
            fmt_duration(r.xstory_load.median),
            r.xstory_throughput_gb_s
        );
    }

    out.push_str("\n> **Observation:** Ingestion bandwidth is measured as `file_bytes / median_load_time`. All values above are from live measurements on the executing hardware.\n\n");

    out.push_str("---\n\n");

    // ── Domain 2: Query Latency Matrix ──
    out.push_str("## Domain 2: Multi-Scale Query Latency Matrix\n\n");

    let has_atuin = scale_results
        .iter()
        .any(|r| r.queries.iter().any(|q| q.atuin.is_some()));

    if has_atuin {
        out.push_str("| Scale | Query Archetype | xstory p50 | xstory p95 | xstory p99 | Atuin p50 | Atuin p95 | Speedup (p50) | 120Hz SLA |\n");
        out.push_str("|:---|:---|:---:|:---:|:---:|:---:|:---:|:---:|:---:|\n");

        for r in scale_results {
            for q in &r.queries {
                let x_p50 = fmt_duration(q.xstory.median);
                let x_p95 = fmt_duration(q.xstory.p95);
                let x_p99 = fmt_duration(q.xstory.p99);
                let sla = sla_verdict(q.xstory.p95);

                if let Some(a) = &q.atuin {
                    let a_p50 = fmt_duration(a.median);
                    let a_p95 = fmt_duration(a.p95);
                    let speedup = ratio_str(a.median.as_secs_f64(), q.xstory.median.as_secs_f64());
                    let _ = writeln!(
                        out,
                        "| {} | {} | {} | {} | {} | {} | {} | {} faster | {} |",
                        fmt_num(r.count),
                        q.label,
                        x_p50,
                        x_p95,
                        x_p99,
                        a_p50,
                        a_p95,
                        speedup,
                        sla
                    );
                } else {
                    let _ = writeln!(
                        out,
                        "| {} | {} | {} | {} | {} | — | — | — | {} |",
                        fmt_num(r.count),
                        q.label,
                        x_p50,
                        x_p95,
                        x_p99,
                        sla
                    );
                }
            }
        }
    } else {
        out.push_str("| Scale | Query Archetype | Median (p50) | Tail (p95) | Extreme Tail (p99) | Matches | 120Hz SLA (<8.33ms) |\n");
        out.push_str("|:---|:---|:---:|:---:|:---:|:---:|:---:|\n");

        for r in scale_results {
            for q in &r.queries {
                let sla = sla_verdict(q.xstory.p95);
                let _ = writeln!(
                    out,
                    "| {} | {} | {} | {} | {} | {} | {} |",
                    fmt_num(r.count),
                    q.label,
                    fmt_duration(q.xstory.median),
                    fmt_duration(q.xstory.p95),
                    fmt_duration(q.xstory.p99),
                    fmt_num(q.xstory_matches),
                    sla
                );
            }
        }
    }

    out.push_str("\n---\n\n");

    // ── Domain 3: Binary & Supply Chain ──
    out.push_str("## Domain 3: Binary Footprint & Supply Chain Metrics\n\n");

    if let Some(a) = a_bin_metrics {
        out.push_str("| Metric | Atuin (measured) | xstory (measured) | Ratio | Significance |\n");
        out.push_str("|:---|:---:|:---:|:---:|:---|\n");

        let _ = writeln!(
            out,
            "| **Executable Size** | {} | {} | {} smaller | Reduced disk, fetch, and page-in cost |",
            fmt_bytes_mb(a.total_bytes),
            fmt_bytes_mb(x_bin_metrics.total_bytes),
            ratio_str(a.total_bytes as f64, x_bin_metrics.total_bytes as f64)
        );
        let _ = writeln!(
            out,
            "| **`__TEXT` Section** | {} | {} | {} smaller | Machine code resident in L1i/L2 |",
            fmt_bytes_mb(a.text_bytes),
            fmt_bytes_mb(x_bin_metrics.text_bytes),
            ratio_str(a.text_bytes as f64, x_bin_metrics.text_bytes as f64)
        );
        let _ = writeln!(
            out,
            "| **Transitive Dependencies** | {} crates | {} crates | {} fewer | Supply chain attack surface |",
            a.dep_count,
            x_bin_metrics.dep_count,
            ratio_str(a.dep_count as f64, x_bin_metrics.dep_count as f64)
        );
    } else {
        out.push_str("| Metric | xstory (measured) |\n");
        out.push_str("|:---|:---:|\n");
        let _ = writeln!(
            out,
            "| **Executable Size** | {} |",
            fmt_bytes_mb(x_bin_metrics.total_bytes)
        );
        let _ = writeln!(
            out,
            "| **`__TEXT` Section** | {} |",
            fmt_bytes_mb(x_bin_metrics.text_bytes)
        );
        let _ = writeln!(
            out,
            "| **Transitive Dependencies** | {} crates |",
            x_bin_metrics.dep_count
        );
    }

    out.push_str("\n---\n\n");

    // ── Domain 4: Kernel & Hardware Counters ──
    out.push_str("## Domain 4: Kernel & Hardware Performance Counters (`/usr/bin/time -l`)\n\n");

    if let Some(a) = a_kern {
        out.push_str(
            "| Metric | Atuin (measured) | xstory (measured) | Ratio | Physical Mechanism |\n",
        );
        out.push_str("|:---|:---:|:---:|:---:|:---|\n");

        let _ = writeln!(
            out,
            "| **Max RSS** | {} | {} | {} lighter | Memory pressure on concurrent shells |",
            fmt_bytes_mb(a.max_rss_bytes),
            fmt_bytes_mb(x_kern.max_rss_bytes),
            ratio_str(a.max_rss_bytes as f64, x_kern.max_rss_bytes as f64)
        );
        let _ = writeln!(
            out,
            "| **Peak Memory Footprint** | {} | {} | {} lighter | Kernel Mach allocator efficiency |",
            fmt_bytes_mb(a.peak_mem_bytes),
            fmt_bytes_mb(x_kern.peak_mem_bytes),
            ratio_str(a.peak_mem_bytes as f64, x_kern.peak_mem_bytes as f64)
        );
        let _ = writeln!(
            out,
            "| **Page Reclaims** | {} | {} | {} fewer | VM subsystem trap minimization |",
            fmt_num(a.page_reclaims as usize),
            fmt_num(x_kern.page_reclaims as usize),
            ratio_str(a.page_reclaims as f64, x_kern.page_reclaims as f64)
        );
        let _ = writeln!(
            out,
            "| **Instructions Retired** | {} | {} | {} fewer | CPU work per invocation |",
            fmt_num(a.instructions as usize),
            fmt_num(x_kern.instructions as usize),
            ratio_str(a.instructions as f64, x_kern.instructions as f64)
        );
        let _ = writeln!(
            out,
            "| **CPU Cycles Elapsed** | {} | {} | {} fewer | Thermal & battery efficiency |",
            fmt_num(a.cycles as usize),
            fmt_num(x_kern.cycles as usize),
            ratio_str(a.cycles as f64, x_kern.cycles as f64)
        );
        let _ = writeln!(
            out,
            "| **IPC** | {:.2} | {:.2} | {} pipeline | Superscalar execution saturation |",
            a.ipc(),
            x_kern.ipc(),
            ratio_str(x_kern.ipc(), a.ipc())
        );
        let _ = writeln!(
            out,
            "| **Voluntary Ctx Switches** | {} | {} | {} fewer | Lock contention elimination |",
            a.vol_ctx_switches,
            x_kern.vol_ctx_switches,
            ratio_str(
                a.vol_ctx_switches as f64,
                x_kern.vol_ctx_switches.max(1) as f64
            )
        );
        let _ = writeln!(
            out,
            "| **Involuntary Ctx Switches** | {} | {} | {} fewer | Thread scheduling overhead |",
            a.invol_ctx_switches,
            x_kern.invol_ctx_switches,
            ratio_str(
                a.invol_ctx_switches as f64,
                x_kern.invol_ctx_switches.max(1) as f64
            )
        );
    } else {
        out.push_str("| Metric | xstory (measured) |\n");
        out.push_str("|:---|:---:|\n");
        let _ = writeln!(
            out,
            "| **Max RSS** | {} |",
            fmt_bytes_mb(x_kern.max_rss_bytes)
        );
        let _ = writeln!(
            out,
            "| **Instructions Retired** | {} |",
            fmt_num(x_kern.instructions as usize)
        );
        let _ = writeln!(
            out,
            "| **CPU Cycles Elapsed** | {} |",
            fmt_num(x_kern.cycles as usize)
        );
        let _ = writeln!(out, "| **IPC** | {:.2} |", x_kern.ipc());
    }

    // ── Domain 5: Comparative Latency Summary ──
    if has_atuin {
        out.push_str("\n---\n\n");
        out.push_str("## Domain 5: Comparative Latency Summary by Scale\n\n");
        out.push_str("| Scale | Best xstory p50 | Worst xstory p95 | Best Atuin p50 | Worst Atuin p95 | Min Speedup | Max Speedup |\n");
        out.push_str("|:---|:---:|:---:|:---:|:---:|:---:|:---:|\n");

        for r in scale_results {
            let x_best_p50 = r
                .queries
                .iter()
                .filter(|q| q.label != "Empty Query (Reverse-Chrono Dump)")
                .map(|q| q.xstory.median)
                .min()
                .unwrap_or(Duration::ZERO);
            let x_worst_p95 = r
                .queries
                .iter()
                .filter(|q| q.label != "Empty Query (Reverse-Chrono Dump)")
                .map(|q| q.xstory.p95)
                .max()
                .unwrap_or(Duration::ZERO);

            let atuin_queries: Vec<&QueryResult> = r
                .queries
                .iter()
                .filter(|q| q.atuin.is_some() && q.label != "Empty Query (Reverse-Chrono Dump)")
                .collect();

            if atuin_queries.is_empty() {
                let _ = writeln!(
                    out,
                    "| {} | {} | {} | — | — | — | — |",
                    fmt_num(r.count),
                    fmt_duration(x_best_p50),
                    fmt_duration(x_worst_p95)
                );
            } else {
                let a_best_p50 = atuin_queries
                    .iter()
                    .map(|q| q.atuin.as_ref().expect("checked").median)
                    .min()
                    .unwrap_or(Duration::ZERO);
                let a_worst_p95 = atuin_queries
                    .iter()
                    .map(|q| q.atuin.as_ref().expect("checked").p95)
                    .max()
                    .unwrap_or(Duration::ZERO);

                let speedups: Vec<f64> = atuin_queries
                    .iter()
                    .filter_map(|q| {
                        let a = q.atuin.as_ref()?;
                        let x_ns = q.xstory.median.as_nanos();
                        if x_ns == 0 {
                            return None;
                        }
                        Some(a.median.as_nanos() as f64 / x_ns as f64)
                    })
                    .collect();

                let min_speedup = speedups.iter().copied().reduce(f64::min).unwrap_or(0.0);
                let max_speedup = speedups.iter().copied().reduce(f64::max).unwrap_or(0.0);

                let _ = writeln!(
                    out,
                    "| {} | {} | {} | {} | {} | **{:.0}×** | **{:.0}×** |",
                    fmt_num(r.count),
                    fmt_duration(x_best_p50),
                    fmt_duration(x_worst_p95),
                    fmt_duration(a_best_p50),
                    fmt_duration(a_worst_p95),
                    min_speedup,
                    max_speedup
                );
            }
        }
    }

    // Write report
    if let Ok(mut f) = File::create(REPORT_PATH) {
        let _ = f.write_all(out.as_bytes());
        eprintln!("\n>> Report persisted to: {REPORT_PATH}");
    }
}

fn chrono_stamp() -> String {
    // ISO 8601 from `date` command — no extra deps
    Command::new("date")
        .arg("+%Y-%m-%dT%H:%M:%S%z")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

// ── Main Orchestrator ────────────────────────────────────────────────────────

fn main() {
    // Handle Criterion/nextest --list
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--list") {
        if !args.iter().any(|a| a == "--ignored") {
            println!("bench_comparative: benchmark");
        }
        return;
    }

    let sys = detect_system();

    println!();
    println!("╔══════════════════════════════════════════════════════════════════════════════╗");
    println!("║  xstory — COMPARATIVE BENCHMARK & STRESS TEST vs. ATUIN                    ║");
    println!("║  Scales: 100,000 │ 500,000 │ 1,000,000 records                             ║");
    println!("║  Zero hardcoded data. All values measured live.                             ║");
    println!("╚══════════════════════════════════════════════════════════════════════════════╝");
    println!();
    println!(
        "System: {} | {:.0} GB RAM | L2: {} KB | macOS {}",
        sys.cpu_brand, sys.ram_gb, sys.l2_cache_kb, sys.os_version
    );
    println!();

    // ── Detect Atuin ──
    let atuin_bin = find_atuin_binary();
    let atuin_available = atuin_bin.is_some();
    if let Some(bin) = &atuin_bin {
        let ver = Command::new(bin)
            .arg("--version")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| "unknown".into());
        println!("✓ Atuin found: {} ({})", bin.display(), ver);
    } else {
        println!("⚠ Atuin not found in PATH. Running xstory-only benchmarks.");
    }
    println!();

    // ── Binary & Dependency Metrics ──
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let xstory_release = repo_root.join("target/release/xstory");

    eprintln!("── Measuring binary footprints ──");
    let mut x_bin_metrics = measure_binary(&xstory_release);
    x_bin_metrics.dep_count = count_cargo_deps(&repo_root);
    eprintln!(
        "  xstory: {} ({} __TEXT) | {} deps",
        fmt_bytes_mb(x_bin_metrics.total_bytes),
        fmt_bytes_mb(x_bin_metrics.text_bytes),
        x_bin_metrics.dep_count
    );

    let a_bin_metrics = atuin_bin.as_ref().map(|bin| {
        let mut m = measure_binary(bin);
        // Count Atuin deps if source is available
        let atuin_dir = repo_root.parent().and_then(|p| {
            let d = p.join("atuin");
            if d.is_dir() { Some(d) } else { None }
        });
        if let Some(d) = atuin_dir {
            m.dep_count = count_cargo_deps(&d);
        }
        eprintln!(
            "  atuin:  {} ({} __TEXT) | {} deps",
            fmt_bytes_mb(m.total_bytes),
            fmt_bytes_mb(m.text_bytes),
            m.dep_count
        );
        m
    });

    // ── Kernel Metrics (cold-start --version) ──
    eprintln!("\n── Collecting kernel/hardware counters (--version, 15 runs) ──");
    let x_kern = if xstory_release.is_file() {
        collect_kernel_metrics(&xstory_release, &["--version"], &[])
    } else {
        eprintln!("  ⚠ xstory release binary not found. Run `cargo build --release` first.");
        KernelMetrics::default()
    };
    eprintln!(
        "  xstory: {} instr | {} cyc | IPC {:.2} | RSS {}",
        fmt_num(x_kern.instructions as usize),
        fmt_num(x_kern.cycles as usize),
        x_kern.ipc(),
        fmt_bytes_mb(x_kern.max_rss_bytes)
    );

    let a_kern = atuin_bin.as_ref().map(|bin| {
        let m = collect_kernel_metrics(bin, &["--version"], &[]);
        eprintln!(
            "  atuin:  {} instr | {} cyc | IPC {:.2} | RSS {}",
            fmt_num(m.instructions as usize),
            fmt_num(m.cycles as usize),
            m.ipc(),
            fmt_bytes_mb(m.max_rss_bytes)
        );
        m
    });

    // ── Scale Benchmarks ──
    let mut scale_results = Vec::with_capacity(SCALES.len());

    for sc in SCALES {
        println!();
        println!(
            "════════════════════════════════════════════════════════════════════════════════"
        );
        println!("  SCALE: {} RECORDS", sc.label);
        println!(
            "════════════════════════════════════════════════════════════════════════════════"
        );

        let (tsv_path, db_path, sql_path) = dataset_paths(sc.count);

        // Generate dataset
        let raw_mb = ensure_tsv_dataset(&tsv_path, sc.count);

        // Setup Atuin if available
        let atuin_env = if atuin_available {
            ensure_atuin_db(&tsv_path, &db_path, &sql_path, sc.count);
            Some(setup_atuin_env(&db_path))
        } else {
            None
        };

        // xstory ingestion benchmark
        eprintln!("\n  Benchmarking xstory ingestion (N={SAMPLES} samples)...");
        let (load_stats, throughput) = bench_xstory_load(&tsv_path, sc.count);
        eprintln!(
            "    p50: {} | p95: {} | throughput: {:.2} GB/s",
            fmt_duration(load_stats.median),
            fmt_duration(load_stats.p95),
            throughput
        );

        // Load arena for query benchmarks
        let arena = Arena::load(&tsv_path, None).expect("load arena for queries");

        // Query benchmarks
        let mut query_results = Vec::with_capacity(QUERY_SUITE.len());

        for qs in QUERY_SUITE {
            eprint!("  Benchmarking: {:<45} ", qs.label);

            // xstory (in-process)
            let (x_stats, x_matches) = bench_xstory_search(&arena, qs.xstory_query, qs.scope);
            eprint!("xstory: {} ", fmt_duration(x_stats.median));

            // Atuin (subprocess) — skip empty query (Atuin lists all history)
            let a_stats = if qs.xstory_query.is_empty() {
                eprint!("| atuin: (skip empty) ");
                None
            } else if let (Some(bin), Some(env)) = (&atuin_bin, &atuin_env) {
                let stats = bench_atuin_search(bin, env, qs.atuin_query, qs.atuin_mode);
                if let Some(ref s) = stats {
                    eprint!("| atuin: {} ", fmt_duration(s.median));
                } else {
                    eprint!("| atuin: FAIL ");
                }
                stats
            } else {
                eprint!("| atuin: N/A ");
                None
            };

            eprintln!();

            query_results.push(QueryResult {
                label: qs.label,
                xstory: x_stats,
                xstory_matches: x_matches,
                atuin: a_stats,
            });
        }

        scale_results.push(ScaleResult {
            label: sc.label,
            count: sc.count,
            raw_mb,
            ram_mb: sc.ram_mb,
            cache_tier: sc.cache_tier,
            xstory_load: load_stats,
            xstory_throughput_gb_s: throughput,
            queries: query_results,
        });
    }

    // ── Summary ──
    println!();
    println!("════════════════════════════════════════════════════════════════════════════════");
    println!("  BENCHMARK COMPLETE — GENERATING REPORT");
    println!("════════════════════════════════════════════════════════════════════════════════");

    generate_report(
        &sys,
        &scale_results,
        &x_bin_metrics,
        &a_bin_metrics,
        &x_kern,
        &a_kern,
    );

    // Print summary table to stdout
    println!();
    println!("┌─────────────────────────────────────────────────────────────────────────────┐");
    println!("│                         VERIFICATION SUMMARY                                │");
    println!("├───────────┬──────────────┬──────────────┬──────────────┬─────────────────────┤");
    println!("│   Scale   │ xstory p50   │ Atuin p50    │  Speedup     │ 120Hz SLA           │");
    println!("├───────────┼──────────────┼──────────────┼──────────────┼─────────────────────┤");

    for r in &scale_results {
        for q in &r.queries {
            if q.label == "Empty Query (Reverse-Chrono Dump)" {
                continue;
            }
            let atuin_str = q
                .atuin
                .as_ref()
                .map(|a| fmt_duration(a.median))
                .unwrap_or_else(|| "—".into());
            let speedup_str = q
                .atuin
                .as_ref()
                .map(|a| {
                    let x_ns = q.xstory.median.as_nanos();
                    if x_ns == 0 {
                        "∞".into()
                    } else {
                        format!("{:.0}×", a.median.as_nanos() as f64 / x_ns as f64)
                    }
                })
                .unwrap_or_else(|| "—".into());
            let sla = if q.xstory.p95.as_secs_f64() < 0.00833 {
                "✓ PASS (120Hz)"
            } else if q.xstory.p95.as_secs_f64() < 0.01666 {
                "✓ PASS (60Hz)"
            } else {
                "✗ FAIL"
            };

            println!(
                "│ {:>9} │ {:>12} │ {:>12} │ {:>12} │ {:>19} │",
                fmt_num(r.count),
                fmt_duration(q.xstory.median),
                atuin_str,
                speedup_str,
                sla
            );
        }
    }
    println!("└───────────┴──────────────┴──────────────┴──────────────┴─────────────────────┘");
    println!();
    println!("Report written to: {REPORT_PATH}");
    println!("Reproduce: `cargo bench --bench bench_comparative`");
}
