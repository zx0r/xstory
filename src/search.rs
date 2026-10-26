//! Search pipeline: bitmask prune → nucleo fuzzy → Top-K heap.
//! Borrows Arena immutably. Returns owned `SearchResult`.

use std::collections::BinaryHeap;

use nucleo_matcher::{Matcher, Utf32Str};

use crate::arena::{char_mask, Arena, Scored, Scope};

// ── Constants ────────────────────────────────────────────────────

/// Maximum results returned from search.
const TOP_K: usize = 1000;

/// Chunk size for parallel search. Below this, single-thread.
const PAR_THRESHOLD: usize = 500;

/// Rayon chunk size for parallel iteration.
const CHUNK_SIZE: usize = 32768;

// ── Query Parsing ────────────────────────────────────────────────

/// Parsed query tokens extracted once per keystroke.
#[derive(Clone, Copy)]
pub enum QueryMode<'a> {
    Empty,
    Prefix(&'a str),     // ^foo
    Substring(&'a str),  // 'foo
    Suffix(&'a str),     // foo$
    Fuzzy(&'a str),      // default
}

#[allow(clippy::option_if_let_else)]
fn parse_query(q: &str) -> QueryMode<'_> {
    let trimmed = q.trim();
    if trimmed.is_empty() {
        return QueryMode::Empty;
    }
    if let Some(rest) = trimmed.strip_prefix('^') {
        if rest.is_empty() { QueryMode::Empty } else { QueryMode::Prefix(rest) }
    } else if let Some(rest) = trimmed.strip_prefix('\'') {
        if rest.is_empty() { QueryMode::Empty } else { QueryMode::Substring(rest) }
    } else if let Some(rest) = trimmed.strip_suffix('$') {
        if rest.is_empty() { QueryMode::Empty } else { QueryMode::Suffix(rest) }
    } else {
        QueryMode::Fuzzy(trimmed)
    }
}

// ── Search Result ────────────────────────────────────────────────

/// Immutable search result: indices into Arena, sorted by score descending.
pub struct SearchResult {
    pub indices: Vec<u32>,
    #[allow(dead_code)]
    pub total:   u32,
}

impl SearchResult {
    #[must_use]
    pub const fn empty() -> Self {
        Self { indices: Vec::new(), total: 0 }
    }
}

// ── Core Match Dispatch ──────────────────────────────────────────

/// Match a single candidate against the query. Returns score or None.
#[allow(clippy::cast_possible_truncation)]
fn match_candidate(
    matcher: &mut Matcher,
    mode: QueryMode<'_>,
    haystack_str: &str,
    needle_buf: &mut Vec<char>,
    haystack_buf: &mut Vec<char>,
) -> Option<u32> {
    let needle = Utf32Str::new(match mode {
        QueryMode::Empty => return Some(0),
        QueryMode::Prefix(s) | QueryMode::Substring(s) | QueryMode::Suffix(s) | QueryMode::Fuzzy(s) => s,
    }, needle_buf);

    let haystack = Utf32Str::new(haystack_str, haystack_buf);

    let score = match mode {
        QueryMode::Empty => unreachable!(),
        QueryMode::Prefix(_) => matcher.prefix_match(haystack, needle),
        QueryMode::Substring(_) => matcher.substring_match(haystack, needle),
        QueryMode::Suffix(_) => matcher.postfix_match(haystack, needle),
        QueryMode::Fuzzy(_) => matcher.fuzzy_match(haystack, needle),
    };

    score.map(u32::from)
}

// ── Top-K Heap ───────────────────────────────────────────────────

/// Push a scored candidate into a bounded heap.
/// If heap is full and new score ≤ min score, reject O(1).
fn heap_push(heap: &mut BinaryHeap<Scored>, scored: Scored, k: usize) {
    if heap.len() < k {
        heap.push(scored);
    } else if let Some(min) = heap.peek() {
        // Scored has reverse Ord: peek() returns the minimum score
        if scored.score > min.score {
            // Replace the minimum with our better candidate
            // pop() removes the min (due to reverse Ord), then push new
            let _ = heap.pop();
            heap.push(scored);
        }
    }
}

/// Drain heap into a sorted Vec (score descending).
fn heap_to_sorted(heap: BinaryHeap<Scored>) -> Vec<u32> {
    let mut items: Vec<Scored> = heap.into_vec();
    // Sort by score descending (natural order, since Scored has reverse Ord for heap)
    items.sort_unstable_by(|a, b| b.score.cmp(&a.score).then_with(|| a.idx.cmp(&b.idx)));
    items.iter().map(|s| s.idx).collect()
}

// ── Single-threaded Search ───────────────────────────────────────

#[allow(clippy::cast_possible_truncation)]
fn search_sequential(
    arena: &Arena,
    indices: &[u32],
    mode: QueryMode<'_>,
    query_mask: u64,
) -> SearchResult {
    let mut matcher = Matcher::new(nucleo_matcher::Config::DEFAULT);
    let mut needle_buf = Vec::new();
    let mut haystack_buf = Vec::new();
    let mut heap = BinaryHeap::with_capacity(TOP_K + 1);

    for &idx in indices {
        // Bitmask fast-reject
        if query_mask != 0 && (arena.masks[idx as usize] & query_mask) != query_mask {
            continue;
        }

        let cmd = arena.cmd_str(idx);
        if let Some(score) = match_candidate(&mut matcher, mode, cmd, &mut needle_buf, &mut haystack_buf) {
            heap_push(&mut heap, Scored { score, idx }, TOP_K);
        }
    }

    let total = indices.len() as u32;
    SearchResult {
        indices: heap_to_sorted(heap),
        total,
    }
}

// ── Parallel Search ──────────────────────────────────────────────

#[allow(clippy::cast_possible_truncation)]
fn search_parallel(
    arena: &Arena,
    indices: &[u32],
    mode: QueryMode<'_>,
    query_mask: u64,
) -> SearchResult {
    use rayon::prelude::*;

    let local_heaps: Vec<BinaryHeap<Scored>> = indices
        .par_chunks(CHUNK_SIZE)
        .map(|chunk| {
            let mut matcher = Matcher::new(nucleo_matcher::Config::DEFAULT);
            let mut needle_buf = Vec::new();
            let mut haystack_buf = Vec::new();
            let mut heap = BinaryHeap::with_capacity(TOP_K + 1);

            for &idx in chunk {
                if query_mask != 0 && (arena.masks[idx as usize] & query_mask) != query_mask {
                    continue;
                }
                let cmd = arena.cmd_str(idx);
                if let Some(score) = match_candidate(&mut matcher, mode, cmd, &mut needle_buf, &mut haystack_buf) {
                    heap_push(&mut heap, Scored { score, idx }, TOP_K);
                }
            }
            heap
        })
        .collect();

    // Merge all local heaps into final
    let mut final_heap = BinaryHeap::with_capacity(TOP_K + 1);
    for local in local_heaps {
        for item in local {
            heap_push(&mut final_heap, item, TOP_K);
        }
    }

    let total = indices.len() as u32;
    SearchResult {
        indices: heap_to_sorted(final_heap),
        total,
    }
}

// ── Public API ───────────────────────────────────────────────────

/// Execute search pipeline: bitmask prune → nucleo fuzzy → Top-K heap.
/// Borrows Arena immutably. Returns owned `SearchResult` (`Vec<u32>` of indices).
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub fn search(arena: &Arena, query: &str, scope: Scope) -> SearchResult {
    let indices = arena.scope_indices(scope);
    if indices.is_empty() {
        return SearchResult::empty();
    }

    let mode = parse_query(query);

    // Empty query → return first K indices in reverse chrono (already sorted)
    if matches!(mode, QueryMode::Empty) {
        let n = indices.len().min(TOP_K);
        return SearchResult {
            indices: indices[..n].to_vec(),
            total: indices.len() as u32,
        };
    }

    let query_bytes = match mode {
        QueryMode::Prefix(s) | QueryMode::Substring(s) | QueryMode::Suffix(s) | QueryMode::Fuzzy(s) => s.as_bytes(),
        QueryMode::Empty => unreachable!(),
    };
    let query_mask = char_mask(query_bytes);

    if indices.len() < PAR_THRESHOLD {
        search_sequential(arena, indices, mode, query_mask)
    } else {
        search_parallel(arena, indices, mode, query_mask)
    }
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    fn write_tsv(lines: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.tsv");
        let mut f = std::fs::File::create(&path).expect("create");
        for line in lines {
            writeln!(f, "{line}").expect("write");
        }
        (dir, path)
    }

    fn make_arena(lines: &[&str]) -> (tempfile::TempDir, Arena) {
        let (dir, path) = write_tsv(lines);
        let arena = Arena::load(&path, Some("/home")).expect("load");
        (dir, arena)
    }

    #[test]
    fn empty_query_reverse_chrono() {
        let (_dir, arena) = make_arena(&[
            "1000\t50\t0\t/home\tls",
            "2000\t50\t0\t/home\tpwd",
            "3000\t50\t0\t/home\tgit status",
        ]);
        let result = search(&arena, "", Scope::Global);
        assert_eq!(result.indices.len(), 3);
        // Newest first
        assert_eq!(arena.cmd_str(result.indices[0]), "git status");
        assert_eq!(arena.cmd_str(result.indices[1]), "pwd");
        assert_eq!(arena.cmd_str(result.indices[2]), "ls");
    }

    #[test]
    fn prefix_match() {
        let (_dir, arena) = make_arena(&[
            "1000\t50\t0\t/home\tgit status",
            "2000\t50\t0\t/home\tgit pull",
            "3000\t50\t0\t/home\tls -la",
            "4000\t50\t0\t/home\tgrep foo",
        ]);
        let result = search(&arena, "^git", Scope::Global);
        assert_eq!(result.indices.len(), 2);
        for &idx in &result.indices {
            assert!(arena.cmd_str(idx).starts_with("git"), "expected prefix match: {}", arena.cmd_str(idx));
        }
    }

    #[test]
    fn substring_match() {
        let (_dir, arena) = make_arena(&[
            "1000\t50\t0\t/home\techo hello world",
            "2000\t50\t0\t/home\tls -la",
            "3000\t50\t0\t/home\tgrep hello file",
        ]);
        let result = search(&arena, "'hello", Scope::Global);
        assert_eq!(result.indices.len(), 2);
        for &idx in &result.indices {
            assert!(arena.cmd_str(idx).contains("hello"), "expected substring match: {}", arena.cmd_str(idx));
        }
    }

    #[test]
    fn suffix_match() {
        let (_dir, arena) = make_arena(&[
            "1000\t50\t0\t/home\tls -la",
            "2000\t50\t0\t/home\tgit status",
            "3000\t50\t0\t/home\techo -la",
        ]);
        let result = search(&arena, "-la$", Scope::Global);
        assert_eq!(result.indices.len(), 2);
        for &idx in &result.indices {
            assert!(arena.cmd_str(idx).ends_with("-la"), "expected suffix match: {}", arena.cmd_str(idx));
        }
    }

    #[test]
    fn fuzzy_ranking_exact_higher() {
        let (_dir, arena) = make_arena(&[
            "1000\t50\t0\t/home\tgit",
            "2000\t50\t0\t/home\tgit status --porcelain",
            "3000\t50\t0\t/home\tgit commit -m fix",
        ]);
        let result = search(&arena, "git", Scope::Global);
        assert!(!result.indices.is_empty());
        // Exact match "git" should score highest
        let top_cmd = arena.cmd_str(result.indices[0]);
        assert_eq!(top_cmd, "git", "exact match should rank first, got: {top_cmd}");
    }

    #[test]
    fn bitmask_no_false_negatives() {
        let (_dir, arena) = make_arena(&[
            "1000\t50\t0\t/home\tabc",
            "2000\t50\t0\t/home\tdef",
            "3000\t50\t0\t/home\tabcdef",
        ]);
        // "abc" search: should find "abc" and "abcdef" but not "def"
        let result = search(&arena, "abc", Scope::Global);
        assert!(result.indices.len() >= 2);
        let cmds: Vec<&str> = result.indices.iter().map(|&i| arena.cmd_str(i)).collect();
        assert!(cmds.contains(&"abc"));
        assert!(cmds.contains(&"abcdef"));
    }

    #[test]
    fn top_k_bounded() {
        // Create more than TOP_K entries
        let mut lines = Vec::new();
        let owned: Vec<String> = (0..1500)
            .map(|i| format!("{}\t50\t0\t/home\tcmd_{i:04}", 1000 + i))
            .collect();
        for line in &owned {
            lines.push(line.as_str());
        }
        let (_dir, arena) = make_arena(&lines);
        let result = search(&arena, "cmd", Scope::Global);
        assert!(result.indices.len() <= TOP_K);
    }

    #[test]
    fn scope_filtering() {
        let (_dir, arena) = make_arena(&[
            "1000\t50\t0\t/home\tls",
            "2000\t50\t0\t/tmp\tpwd",
            "3000\t50\t0\t/home\tgit status",
        ]);
        let global = search(&arena, "", Scope::Global);
        let local = search(&arena, "", Scope::Local);
        assert_eq!(global.indices.len(), 3);
        assert_eq!(local.indices.len(), 2);
    }

    #[test]
    fn empty_arena() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("empty.tsv");
        std::fs::File::create(&path).expect("create");
        let arena = Arena::load(&path, None).expect("load");
        let result = search(&arena, "foo", Scope::Global);
        assert!(result.indices.is_empty());
        assert_eq!(result.total, 0);
    }

    #[test]
    fn parallel_vs_sequential_same_results() {
        // Build enough entries to trigger parallel path
        let mut lines = Vec::new();
        let owned: Vec<String> = (0..600)
            .map(|i| format!("{}\t50\t0\t/home\tcommand_{i:04}_test", 1000 + i))
            .collect();
        for line in &owned {
            lines.push(line.as_str());
        }
        let (_dir, arena) = make_arena(&lines);

        let seq = search_sequential(
            &arena,
            arena.scope_indices(Scope::Global),
            QueryMode::Fuzzy("test"),
            char_mask(b"test"),
        );
        let par = search_parallel(
            &arena,
            arena.scope_indices(Scope::Global),
            QueryMode::Fuzzy("test"),
            char_mask(b"test"),
        );

        assert_eq!(seq.indices.len(), par.indices.len());
        // Same set of indices (order may differ slightly for equal scores)
        let mut seq_sorted = seq.indices;
        let mut par_sorted = par.indices;
        seq_sorted.sort_unstable();
        par_sorted.sort_unstable();
        assert_eq!(seq_sorted, par_sorted);
    }

    #[test]
    fn query_parse_modes() {
        assert!(matches!(parse_query(""), QueryMode::Empty));
        assert!(matches!(parse_query("  "), QueryMode::Empty));
        assert!(matches!(parse_query("^foo"), QueryMode::Prefix("foo")));
        assert!(matches!(parse_query("'bar"), QueryMode::Substring("bar")));
        assert!(matches!(parse_query("baz$"), QueryMode::Suffix("baz")));
        assert!(matches!(parse_query("qux"), QueryMode::Fuzzy("qux")));
        assert!(matches!(parse_query("^"), QueryMode::Empty));
        assert!(matches!(parse_query("'"), QueryMode::Empty));
        assert!(matches!(parse_query("$"), QueryMode::Empty));
    }
}
