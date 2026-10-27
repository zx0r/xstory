use std::io::Write;
use std::path::Path;
use std::hint::black_box;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use xstory::arena::{Arena, Scope};
use xstory::search::search;

const BENCH_ENTRY_COUNT: usize = 100_000;

#[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
fn generate_history_tsv(path: &Path, count: usize) -> u64 {
    let mut file = std::fs::File::create(path).expect("create bench tsv");
    let commands = [
        "git status --porcelain",
        "git commit -m 'fix: resolve edge case in parser'",
        "cargo build --release",
        "cargo test --all-targets",
        "docker run -d -p 8080:8080 my-service:latest",
        "kubectl get pods -n production",
        "curl -v -H 'Accept: application/json' https://api.example.com/v1/health",
        "npm run build",
        "vim src/main.rs",
        "cat /var/log/nginx/access.log | grep 404 | awk '{print $1}'",
        "ls -la /usr/local/bin",
        "find . -name '*.rs' | xargs wc -l",
    ];

    let cwds = [
        "/Users/developer/projects/xstory",
        "/Users/developer/projects/backend",
        "/Users/developer/projects/frontend",
        "/tmp",
        "/var/log",
    ];

    let mut total_bytes = 0_u64;
    for i in 0..count {
        let cmd = commands[i % commands.len()];
        let cwd = cwds[i % cwds.len()];
        let ts = 1_700_000_000_000_i64 + (i as i64 * 1000);
        let dur = (i % 500) as u32;
        let exit = i32::from(i % 17 == 0);
        let line = format!("{ts}\t{dur}\t{exit}\t{cwd}\t{cmd}_{i}\n");
        total_bytes += line.len() as u64;
        file.write_all(line.as_bytes()).expect("write");
    }
    file.flush().expect("flush");
    total_bytes
}

fn bench_arena_load(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bench_load_100k.tsv");
    let total_bytes = generate_history_tsv(&path, BENCH_ENTRY_COUNT);

    let mut group = c.benchmark_group("arena_load");
    group.throughput(Throughput::Bytes(total_bytes));
    group.bench_function(BenchmarkId::new("load_100k", BENCH_ENTRY_COUNT), |b| {
        b.iter(|| {
            let arena = Arena::load(&path, None).expect("load");
            black_box(arena);
        });
    });
    group.finish();
}

fn bench_search_pipeline(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bench_search_100k.tsv");
    generate_history_tsv(&path, BENCH_ENTRY_COUNT);
    let arena = Arena::load(&path, Some("/Users/developer/projects/xstory")).expect("load");

    let mut group = c.benchmark_group("search_pipeline");

    group.bench_function("empty_query", |b| {
        b.iter(|| {
            let res = search(&arena, "", Scope::Global);
            black_box(res);
        });
    });

    group.bench_function("prefix_git", |b| {
        b.iter(|| {
            let res = search(&arena, "^git", Scope::Global);
            black_box(res);
        });
    });

    group.bench_function("substring_release", |b| {
        b.iter(|| {
            let res = search(&arena, "'release", Scope::Global);
            black_box(res);
        });
    });

    group.bench_function("suffix_latest", |b| {
        b.iter(|| {
            let res = search(&arena, "latest$", Scope::Global);
            black_box(res);
        });
    });

    group.bench_function("fuzzy_crbld", |b| {
        b.iter(|| {
            let res = search(&arena, "crbld", Scope::Global);
            black_box(res);
        });
    });

    group.bench_function("local_scope_prefix", |b| {
        b.iter(|| {
            let res = search(&arena, "^git", Scope::Local);
            black_box(res);
        });
    });

    group.finish();
}

criterion_group!(benches, bench_arena_load, bench_search_pipeline);
criterion_main!(benches);
