use assert_cmd::Command;
use predicates::prelude::*;
use std::io::Write;
use xstory::arena::{Arena, Scope};

#[test]
fn e2e_ingestion_and_roundtrip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let history_path = dir.path().join("history.tsv");

    let mut cmd = Command::cargo_bin("xstory").expect("binary");
    cmd.env("XSTORY_HISTORY_PATH", &history_path)
        .args(["record", "git", "commit", "-m", "initial", "--exit=0", "--duration=120", "--cwd=/test/project"])
        .assert()
        .success();

    let arena = Arena::load(&history_path, None).expect("load");
    assert_eq!(arena.len(), 1);
    assert_eq!(arena.cmd_str(0), "git commit -m initial");
    assert_eq!(arena.cwd_str(0), "/test/project");
    assert_eq!(arena.exit_codes[0], 0);
    assert_eq!(arena.durations[0], 120);
}

#[test]
fn e2e_deduplication_newest_wins() {
    let dir = tempfile::tempdir().expect("tempdir");
    let history_path = dir.path().join("history.tsv");

    // Record the same command 3 times
    for i in 1..=3 {
        let mut cmd = Command::cargo_bin("xstory").expect("binary");
        cmd.env("XSTORY_HISTORY_PATH", &history_path)
            .args(["record", "--", "cargo test", "0", &format!("{i}0")])
            .assert()
            .success();
    }

    let arena = Arena::load(&history_path, None).expect("load");
    assert_eq!(arena.len(), 3, "raw entries must be 3");
    assert_eq!(arena.global_indices.len(), 1, "deduped global indices must be 1");
    // Newest index is 2
    assert_eq!(arena.global_indices[0], 2);
    assert_eq!(arena.cmd_str(arena.global_indices[0]), "cargo test");
}

#[test]
fn e2e_scope_isolation_global_and_local() {
    let dir = tempfile::tempdir().expect("tempdir");
    let history_path = dir.path().join("history.tsv");

    let mut cmd1 = Command::cargo_bin("xstory").expect("binary");
    cmd1.env("XSTORY_HISTORY_PATH", &history_path)
        .args(["record", "npm", "start", "--exit=0", "--duration=100", "--cwd=/home/user/frontend"])
        .assert()
        .success();

    let mut cmd2 = Command::cargo_bin("xstory").expect("binary");
    cmd2.env("XSTORY_HISTORY_PATH", &history_path)
        .args(["record", "cargo", "run", "--exit=0", "--duration=200", "--cwd=/home/user/backend"])
        .assert()
        .success();

    let arena_frontend = Arena::load(&history_path, Some("/home/user/frontend")).expect("load");
    assert_eq!(arena_frontend.scope_indices(Scope::Global).len(), 2);
    assert_eq!(arena_frontend.scope_indices(Scope::Local).len(), 1);
    assert_eq!(arena_frontend.cmd_str(arena_frontend.scope_indices(Scope::Local)[0]), "npm start");

    let arena_backend = Arena::load(&history_path, Some("/home/user/backend")).expect("load");
    assert_eq!(arena_backend.scope_indices(Scope::Local).len(), 1);
    assert_eq!(arena_backend.cmd_str(arena_backend.scope_indices(Scope::Local)[0]), "cargo run");
}

#[test]
fn e2e_security_and_privacy_policy() {
    let dir = tempfile::tempdir().expect("tempdir");
    let history_path = dir.path().join("history.tsv");

    // 1. Excluded commands: exit, clear
    let mut cmd_exit = Command::cargo_bin("xstory").expect("binary");
    cmd_exit.env("XSTORY_HISTORY_PATH", &history_path)
        .args(["record", "exit", "--exit=0", "--duration=10"])
        .assert()
        .success();

    let mut cmd_clear = Command::cargo_bin("xstory").expect("binary");
    cmd_clear.env("XSTORY_HISTORY_PATH", &history_path)
        .args(["record", "clear", "--exit=0", "--duration=10"])
        .assert()
        .success();

    // 2. Ignorespace command
    let mut cmd_space = Command::cargo_bin("xstory").expect("binary");
    cmd_space.env("XSTORY_HISTORY_PATH", &history_path)
        .args(["record", "--", " secret_command", "0", "10"])
        .assert()
        .success();

    // 3. Secret masking: AWS_SECRET and --password
    let mut cmd_secret = Command::cargo_bin("xstory").expect("binary");
    cmd_secret.env("XSTORY_HISTORY_PATH", &history_path)
        .args(["record", "curl", "--password", "supersecret123", "https://api.internal"])
        .assert()
        .success();

    let arena = Arena::load(&history_path, None).expect("load");
    // Only the masked command was recorded; exit, clear, and leading-space command were rejected
    assert_eq!(arena.len(), 1);
    let recorded_cmd = arena.cmd_str(0);
    assert!(!recorded_cmd.contains("supersecret123"));
    assert!(recorded_cmd.contains("***"));
}

#[test]
fn e2e_graceful_degeneration() {
    // Missing file
    let nonexistent = std::path::PathBuf::from("/tmp/nonexistent_xstory_dir_xyz/test.tsv");
    let arena = Arena::load(&nonexistent, None).expect("load missing file");
    assert!(arena.is_empty());

    // Empty file
    let dir = tempfile::tempdir().expect("tempdir");
    let empty_path = dir.path().join("empty.tsv");
    std::fs::File::create(&empty_path).expect("create");
    let arena_empty = Arena::load(&empty_path, None).expect("load empty file");
    assert!(arena_empty.is_empty());

    // File with corrupted lines
    let corrupt_path = dir.path().join("corrupt.tsv");
    let mut f = std::fs::File::create(&corrupt_path).expect("create");
    writeln!(f, "corrupted line without tabs").expect("write");
    writeln!(f, "1000\t50\t0\t/home\tvalid cmd 1").expect("write");
    writeln!(f, "bad\ttab\tformat").expect("write");
    writeln!(f, "2000\t50\t0\t/home\tvalid cmd 2").expect("write");
    drop(f);

    let arena_corrupt = Arena::load(&corrupt_path, None).expect("load corrupt file");
    assert_eq!(arena_corrupt.len(), 2);
    assert_eq!(arena_corrupt.cmd_str(0), "valid cmd 1");
    assert_eq!(arena_corrupt.cmd_str(1), "valid cmd 2");
}

#[test]
fn e2e_cli_dispatch_and_exit_codes() {
    // Version
    let mut cmd = Command::cargo_bin("xstory").expect("binary");
    cmd.arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("xstory 0.1.0"));

    // Help
    let mut cmd = Command::cargo_bin("xstory").expect("binary");
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Usage:"));

    // Init hooks
    let mut cmd = Command::cargo_bin("xstory").expect("binary");
    cmd.args(["init", "bash"])
        .assert()
        .success()
        .stdout(predicate::str::contains("__xstory_search"));

    let mut cmd = Command::cargo_bin("xstory").expect("binary");
    cmd.args(["init", "zsh"])
        .assert()
        .success()
        .stdout(predicate::str::contains("__xstory_widget"));

    let mut cmd = Command::cargo_bin("xstory").expect("binary");
    cmd.args(["init", "fish"])
        .assert()
        .success()
        .stdout(predicate::str::contains("__xstory_search"));

    // Unknown subcommand
    let mut cmd = Command::cargo_bin("xstory").expect("binary");
    cmd.arg("invalid_subcommand_123")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("unknown subcommand"));
}
