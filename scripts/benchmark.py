#!/usr/bin/env python3
"""
Empirical Performance Benchmark: xstory vs Atuin
Compares binary footprints, dependencies, kernel counters, and CLI latency.
"""

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys


def count_sloc(directory: Path):
    if not directory.is_dir():
        return {"files": 0, "total": 0, "code": 0, "comments": 0, "blanks": 0}
    total = code = comments = blanks = files = 0
    for root, _, filenames in os.walk(directory):
        if any(x in root for x in ["target", ".git", "vendor", "tests"]):
            continue
        for f in filenames:
            if f.endswith(".rs"):
                files += 1
                with open(
                    os.path.join(root, f), "r", encoding="utf-8", errors="ignore"
                ) as fh:
                    for line in fh:
                        total += 1
                        s = line.strip()
                        if not s:
                            blanks += 1
                        elif (
                            s.startswith("//")
                            or s.startswith("/*")
                            or s.startswith("*")
                        ):
                            comments += 1
                        else:
                            code += 1
    return {
        "files": files,
        "total": total,
        "code": code,
        "comments": comments,
        "blanks": blanks,
    }


def get_binary_size(path: Path):
    if not path.is_file():
        return {
            "total_bytes": 0,
            "total_mb": 0.0,
            "text_bytes": 0,
            "text_mb": 0.0,
            "data_bytes": 0,
            "data_kb": 0.0,
        }
    size_bytes = path.stat().st_size
    text_size = 0
    data_size = 0
    try:
        cmd = ["size", str(path)]
        out = subprocess.check_output(cmd, text=True)
        lines = out.strip().splitlines()
        if len(lines) >= 2:
            parts = lines[1].split()
            if len(parts) >= 2:
                text_size = int(parts[0])
                data_size = int(parts[1])
    except Exception:
        pass

    return {
        "total_bytes": size_bytes,
        "total_mb": size_bytes / (1024 * 1024),
        "text_bytes": text_size,
        "text_mb": text_size / (1024 * 1024),
        "data_bytes": data_size,
        "data_kb": data_size / 1024,
    }


def get_dependency_count(cwd: Path):
    if not cwd.is_dir() or not (cwd / "Cargo.toml").exists():
        return 0
    try:
        out = subprocess.check_output(
            "cargo tree --prefix none | sort -u | wc -l",
            shell=True,
            cwd=str(cwd),
            text=True,
        )
        return int(out.strip())
    except Exception:
        return 0


def get_kernel_metrics(bin_path: Path, args, runs=15):
    if not bin_path.is_file():
        return {
            "max_rss": 0,
            "page_reclaims": 0,
            "page_faults": 0,
            "vol_switches": 0,
            "invol_switches": 0,
            "instructions": 0,
            "cycles": 0,
            "peak_mem": 0,
            "ipc": 0.0,
        }

    patterns = {
        "max_rss": re.compile(r"(\d+)\s+maximum resident set size"),
        "page_reclaims": re.compile(r"(\d+)\s+page reclaims"),
        "page_faults": re.compile(r"(\d+)\s+page faults"),
        "vol_switches": re.compile(r"(\d+)\s+voluntary context switches"),
        "invol_switches": re.compile(r"(\d+)\s+involuntary context switches"),
        "instructions": re.compile(r"(\d+)\s+instructions retired"),
        "cycles": re.compile(r"(\d+)\s+cycles elapsed"),
        "peak_mem": re.compile(r"(\d+)\s+peak memory footprint"),
    }

    collected = {k: [] for k in patterns}

    for _ in range(runs):
        proc = subprocess.run(
            ["/usr/bin/time", "-l", str(bin_path)] + args,
            capture_output=True,
            text=True,
        )
        combined = proc.stderr
        for key, pat in patterns.items():
            m = pat.search(combined)
            if m:
                collected[key].append(int(m.group(1)))

    def median(lst):
        if not lst:
            return 0
        s = sorted(lst)
        n = len(s)
        return s[n // 2]

    med = {k: median(v) for k, v in collected.items()}
    ipc = (med["instructions"] / med["cycles"]) if med["cycles"] > 0 else 0.0
    med["ipc"] = ipc
    return med


def run_hyperfine_bench(cmd_a, cmd_b, name, warmup=10, runs=150):
    json_path = f"/tmp/bench_{name}.json"
    hf_cmd = [
        "hyperfine",
        "--warmup",
        str(warmup),
        "--runs",
        str(runs),
        "--export-json",
        json_path,
        "-n",
        "xstory",
        cmd_a,
        "-n",
        "atuin",
        cmd_b,
    ]
    subprocess.run(hf_cmd, check=True)
    with open(json_path, "r", encoding="utf-8") as fh:
        data = json.load(fh)
    results = {}
    for r in data["results"]:
        name_key = r.get("name") or r["command"]
        if "summary" in r and "time_wall_clock" in r["summary"]:
            wall = r["summary"]["time_wall_clock"]
            results[name_key] = {
                "mean": wall["mean"] * 1000,
                "stddev": wall["stddev"] * 1000,
                "median": wall["median"] * 1000,
                "min": wall["min"] * 1000,
                "max": wall["max"] * 1000,
            }
        elif "mean" in r:
            results[name_key] = {
                "mean": r["mean"] * 1000,
                "stddev": r["stddev"] * 1000,
                "median": r["median"] * 1000,
                "min": r["min"] * 1000,
                "max": r["max"] * 1000,
            }
    return results


def main():
    repo_root = Path(__file__).resolve().parent.parent

    parser = argparse.ArgumentParser(description="xstory vs Atuin Benchmark Suite")
    parser.add_argument(
        "--xstory-dir",
        type=Path,
        default=Path(os.environ.get("XSTORY_DIR", repo_root)),
        help="Path to xstory repository directory",
    )
    parser.add_argument(
        "--xstory-bin",
        type=Path,
        default=None,
        help="Path to compiled xstory release executable",
    )
    parser.add_argument(
        "--atuin-dir",
        type=Path,
        default=Path(os.environ.get("ATUIN_DIR", repo_root.parent / "atuin")),
        help="Path to atuin repository directory",
    )
    parser.add_argument(
        "--atuin-bin",
        type=Path,
        default=None,
        help="Path to atuin executable",
    )
    parser.add_argument("--warmup", type=int, default=10, help="Warmup iterations")
    parser.add_argument("--runs", type=int, default=150, help="Benchmark run count")
    parser.add_argument(
        "--output-md",
        type=Path,
        default=None,
        help="Target file for generated Markdown report",
    )
    args = parser.parse_args()

    xstory_dir = args.xstory_dir.resolve()
    xstory_src = xstory_dir / "src"

    # Resolve xstory binary
    if args.xstory_bin:
        xstory_bin = args.xstory_bin.resolve()
    else:
        candidates = [
            xstory_dir / "target" / "release" / "xstory",
            Path.home() / ".local" / "bin" / "xstory",
        ]
        which_xstory = shutil.which("xstory")
        if which_xstory:
            candidates.insert(0, Path(which_xstory))
        xstory_bin = next((c for c in candidates if c.is_file()), candidates[0])

    # Resolve atuin directory and binary
    atuin_dir = args.atuin_dir.resolve()
    atuin_src = atuin_dir / "crates"

    if args.atuin_bin:
        atuin_bin = args.atuin_bin.resolve()
    else:
        which_atuin = shutil.which("atuin")
        atuin_bin = Path(which_atuin) if which_atuin else Path("/usr/local/bin/atuin")

    print(f"xstory dir: {xstory_dir}")
    print(f"xstory bin: {xstory_bin}")
    print(f"atuin  dir: {atuin_dir} (exists: {atuin_dir.is_dir()})")
    print(f"atuin  bin: {atuin_bin} (exists: {atuin_bin.is_file()})")

    if not xstory_bin.is_file():
        print(f"Error: xstory binary not found at {xstory_bin}. Run `cargo build --release` first.", file=sys.stderr)
        sys.exit(1)

    print("\n=== 1. Measuring SLOC ===")
    x_sloc = count_sloc(xstory_src)
    a_sloc = count_sloc(atuin_src)
    print(f"xstory: {x_sloc['code']} code lines across {x_sloc['files']} files")
    print(f"atuin : {a_sloc['code']} code lines across {a_sloc['files']} files")

    print("\n=== 2. Measuring Binary Sizes & Mach-O Sections ===")
    x_bin = get_binary_size(xstory_bin)
    a_bin = get_binary_size(atuin_bin)
    print(f"xstory bin: {x_bin['total_mb']:.2f} MB (TEXT: {x_bin['text_mb']:.2f} MB)")
    print(f"atuin bin : {a_bin['total_mb']:.2f} MB (TEXT: {a_bin['text_mb']:.2f} MB)")

    print("\n=== 3. Measuring Dependency Graphs ===")
    x_deps = get_dependency_count(xstory_dir)
    a_deps = get_dependency_count(atuin_dir)
    print(f"xstory dependencies: {x_deps} crates")
    print(f"atuin dependencies : {a_deps} crates")

    print("\n=== 4. Measuring Kernel Syscalls & Hardware Counters ===")
    x_kern = get_kernel_metrics(xstory_bin, ["--version"], runs=15)
    a_kern = get_kernel_metrics(atuin_bin, ["--version"], runs=15)
    print(
        f"xstory retired: {x_kern['instructions']:,} instr | cycles: {x_kern['cycles']:,} | IPC: {x_kern['ipc']:.2f} | RSS: {x_kern['max_rss'] / (1024*1024):.2f} MB"
    )
    print(
        f"atuin  retired: {a_kern['instructions']:,} instr | cycles: {a_kern['cycles']:,} | IPC: {a_kern['ipc']:.2f} | RSS: {a_kern['max_rss'] / (1024*1024):.2f} MB"
    )

    if not shutil.which("hyperfine"):
        print("\nNote: `hyperfine` is not installed in PATH. Skipping latency benchmark.")
        return

    print(f"\n=== 5. Running Hyperfine Latency Benchmarks (Warmup {args.warmup}, Runs {args.runs}) ===")
    hf_version = run_hyperfine_bench(
        f"{xstory_bin} --version", f"{atuin_bin} --version", "version", warmup=args.warmup, runs=args.runs
    )
    hf_help = run_hyperfine_bench(
        f"{xstory_bin} --help", f"{atuin_bin} --help", "help", warmup=args.warmup, runs=args.runs
    )

    test_tsv = "/tmp/bench_xstory.tsv"
    hf_record = run_hyperfine_bench(
        f"{xstory_bin} -H {test_tsv} record -c /tmp -d 50 -e 0 'git status'",
        f"{atuin_bin} history start 'git status' >/dev/null",
        "record",
        warmup=args.warmup,
        runs=args.runs,
    )

    # Markdown Table Generation
    a_mb = a_bin["total_mb"] or 1.0
    x_mb = x_bin["total_mb"] or 1.0
    a_text = a_bin["text_mb"] or 1.0
    x_text = x_bin["text_mb"] or 1.0
    a_code = a_sloc["code"] or 1
    x_code = x_sloc["code"] or 1
    a_dp = a_deps or 1
    x_dp = x_deps or 1

    table_lines = [
        "| Metric / Dimension | Atuin | xstory | Delta / Ratio | Mathematical Significance |",
        "|:---|:---:|:---:|:---:|:---|",
        f"| **Executable Binary Size** | {a_mb:.2f} MB | {x_mb:.2f} MB | **{a_mb / x_mb:.1f}× smaller** | Minimalist Mach-O footprint |",
        f"| **Mach-O `__TEXT` Section** | {a_text:.2f} MB | {x_text:.2f} MB | **{a_text / x_text:.1f}× smaller** | Executable machine code density |",
        f"| **Source Code (Rust SLOC)** | {a_code:,} lines | {x_code:,} lines | **{a_code / x_code:.1f}× leaner** | Auditability & zero bloated abstractions |",
        f"| **Dependency Graph** | {a_dp} crates | {x_dp} crates | **{a_dp / x_dp:.1f}× fewer** | Attack surface & build time reduction |",
        f"| **Cold-Start Latency (`--version`)** | {hf_version['atuin']['mean']:.2f} ± {hf_version['atuin']['stddev']:.2f} ms | {hf_version['xstory']['mean']:.2f} ± {hf_version['xstory']['stddev']:.2f} ms | **{hf_version['atuin']['mean'] / max(hf_version['xstory']['mean'], 0.001):.1f}× faster** | Cold process spawn latency |",
        f"| **CLI Help Latency (`--help`)** | {hf_help['atuin']['mean']:.2f} ± {hf_help['atuin']['stddev']:.2f} ms | {hf_help['xstory']['mean']:.2f} ± {hf_help['xstory']['stddev']:.2f} ms | **{hf_help['atuin']['mean'] / max(hf_help['xstory']['mean'], 0.001):.1f}× faster** | Instantaneous responsiveness |",
        f"| **Shell Hook Append (Hot Path)** | {hf_record['atuin']['mean']:.2f} ± {hf_record['atuin']['stddev']:.2f} ms | {hf_record['xstory']['mean']:.2f} ± {hf_record['xstory']['stddev']:.2f} ms | **{hf_record['atuin']['mean'] / max(hf_record['xstory']['mean'], 0.001):.1f}× faster** | Sub-millisecond prompt latency |",
        f"| **Max Resident Set Size (RSS)** | {a_kern['max_rss'] / (1024*1024):.2f} MB | {x_kern['max_rss'] / (1024*1024):.2f} MB | **{(a_kern['max_rss'] / max(x_kern['max_rss'], 1)):.1f}× lighter** | Zero memory pressure on shell sessions |",
        f"| **Peak Memory Footprint** | {a_kern['peak_mem'] / (1024*1024):.2f} MB | {x_kern['peak_mem'] / (1024*1024):.2f} MB | **{(a_kern['peak_mem'] / max(x_kern['peak_mem'], 1)):.1f}× lighter** | Kernel Mach allocator efficiency |",
        f"| **Kernel Page Reclaims (Faults)** | {a_kern['page_reclaims']:,} | {x_kern['page_reclaims']:,} | **{(a_kern['page_reclaims'] / max(x_kern['page_reclaims'], 1)):.1f}× fewer** | Kernel VM subsystem trap minimization |",
        f"| **Instructions Retired** | {a_kern['instructions']:,} | {x_kern['instructions']:,} | **{(a_kern['instructions'] / max(x_kern['instructions'], 1)):.1f}× fewer** | Raw CPU work required for execution |",
        f"| **CPU Cycles Elapsed** | {a_kern['cycles']:,} | {x_kern['cycles']:,} | **{(a_kern['cycles'] / max(x_kern['cycles'], 1)):.1f}× fewer** | CPU thermal & battery efficiency |",
        f"| **Instructions Per Cycle (IPC)** | {a_kern['ipc']:.2f} | {x_kern['ipc']:.2f} | **{(x_kern['ipc'] / max(a_kern['ipc'], 0.01)):.2f}× pipeline** | Hardware branch prediction & locality |",
    ]

    table_md = "\n".join(table_lines)
    print("\n=== GENERATED TABLE ===")
    print(table_md)

    out_file = args.output_md or (xstory_dir / "BENCHMARK_ATUIN.md")
    with open(out_file, "w", encoding="utf-8") as fh:
        fh.write(
            f"# Empirical Benchmark: xstory vs Atuin\n\nGenerated with `hyperfine` (warmup={args.warmup}, runs={args.runs}) and macOS Mach kernel instrumentation (`/usr/bin/time -l`).\n\n{table_md}\n"
        )
    print(f"\nReport written to {out_file}")


if __name__ == "__main__":
    main()
