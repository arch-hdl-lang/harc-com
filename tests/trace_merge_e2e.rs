//! End-to-end regression for `harc trace-merge` — TODO #1 in
//! docs/semantic-trace.md ("Add an end-to-end regression fixture that runs
//! `harc sim --waves --wave-format vcd --record-trace`, then runs `harc
//! trace-merge`, and checks that the merged VCD contains the expected
//! semantic scope and event pulses").
//!
//! The existing unit tests in `src/trace_merge.rs` exercise the merger
//! against synthetic hand-built VCDs and JSONL strings. This file drives the
//! real CLI: `harc sim --sv ... --waves --wave-format vcd --record-trace
//! trace.jsonl` produces the inputs, `harc trace-merge` produces the merged
//! VCD, and we then assert against the merged VCD shape documented in
//! `docs/semantic-trace.md` and emitted by `src/trace_merge.rs::emit_semantic_header`.
//!
//! The chosen fixture is `tests/fixtures/tlm_method_bus_test.harc` paired
//! with `tests/dut/TlmMemory.sv`. It exercises BOTH semantic-trace TLM
//! paths in a single run:
//!   - blocking initiator calls   (`read`, `poke`)
//!   - out-of-order tagged calls  (`read_ooo` with `tag` 0 and 1)
//! so a single `#[test]` covers both. The user's task spec called for
//! `harc_arch_pairing/ooo_tags.harc`; that exact path doesn't exist in this
//! repo. The closest cousins are
//! `tests/fixtures/tlm_pairing_arch_initiator_test.harc` and
//! `tests/fixtures/tlm_target_ooo_lanes_test.harc`, but both require an
//! ARCH toolchain checkout (arch sibling repo) or vendored SV DUTs that
//! are tied to specific RTL behaviour. `tlm_method_bus_test.harc` is the
//! self-contained TLM fixture that already lives in `run_fixtures.sh` and
//! covers blocking + OOO + fork paths.
//!
//! The load-bearing assertion (per PR #303) is that each semantic event's
//! `vcd_time` field shows up as a `tlm_call` lane pulse at exactly that
//! VCD timestamp in the merged output. That's what proves the merger used
//! the alignment metadata rather than re-deriving timing.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Minimum number of TLM-call events we expect in the trace. The fixture
/// drives two `read`s, one `poke`, and two `read_ooo` requests, so the
/// observed floor in the current trace mode is 6 events (some
/// request/response pairs may merge into a single recorded edge).
const MIN_TLM_EVENTS: usize = 6;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Minimum Verilator version that accepts `--trace-vcd`. Older versions
/// only know bare `--trace`.
const MIN_VERILATOR_MAJOR: u32 = 5;
const MIN_VERILATOR_MINOR: u32 = 36;

/// Shell out to `verilator --version` and parse a `(major, minor)` pair
/// from output like `Verilator 5.034 2024-12-...`. Returns `None` if
/// verilator is missing or the output is unparseable.
fn detect_verilator_version() -> Option<(u32, u32)> {
    let out = Command::new("verilator").arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    // Expected leading token: "Verilator <maj>.<min>[ ...]"
    let first = s.split_whitespace().nth(1)?;
    let mut it = first.split('.');
    let maj: u32 = it.next()?.parse().ok()?;
    let min: u32 = it.next()?.parse().ok()?;
    Some((maj, min))
}

fn harc_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_harc"))
}

/// Allocate a fresh temp subdir for this test run. Uses
/// `std::env::temp_dir()` so we don't pull in `tempfile` as a new dep
/// (not currently in Cargo.toml).
fn fresh_outdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "harc_trace_merge_e2e_{}_{}",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp outdir");
    dir
}

fn run_harc_sim(outdir: &Path, trace_path: &Path) -> (bool, String) {
    let root = workspace_root();
    let sv = root.join("tests/dut/TlmMemory.sv");
    let fixture = root.join("tests/fixtures/tlm_method_bus_test.harc");

    let out = Command::new(harc_bin())
        .arg("sim")
        .arg("--sv")
        .arg(&sv)
        .arg(&fixture)
        .arg("--top")
        .arg("TlmMemory")
        .arg("--outdir")
        .arg(outdir)
        .arg("--waves")
        .arg("--wave-format")
        .arg("vcd")
        .arg("--record-trace")
        .arg(trace_path)
        .output()
        .expect("spawn harc sim");

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let combined = format!("--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}");
    (out.status.success(), combined)
}

fn run_trace_merge(vcd: &Path, trace: &Path, out_vcd: &Path) {
    let out = Command::new(harc_bin())
        .arg("trace-merge")
        .arg("--vcd")
        .arg(vcd)
        .arg("--trace")
        .arg(trace)
        .arg("--out")
        .arg(out_vcd)
        .output()
        .expect("spawn harc trace-merge");
    assert!(
        out.status.success(),
        "harc trace-merge failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Parse JSONL trace and return the `vcd_time` of every `tlm_call` event.
/// Hand-rolled extraction so we don't add a `serde_json` dev-dep (the
/// merger itself ships a hand-rolled JSON parser for the same reason).
fn tlm_call_vcd_times(trace_text: &str) -> Vec<u64> {
    let mut times = Vec::new();
    for line in trace_text.lines() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        if !l.contains("\"type\":\"tlm_call\"") {
            continue;
        }
        let Some(idx) = l.find("\"vcd_time\":") else {
            continue;
        };
        let rest = &l[idx + "\"vcd_time\":".len()..];
        let mut end = 0;
        for (i, c) in rest.char_indices() {
            if !c.is_ascii_digit() {
                end = i;
                break;
            }
        }
        if end == 0 {
            continue;
        }
        if let Ok(n) = rest[..end].parse::<u64>() {
            times.push(n);
        }
    }
    times
}

/// Walk the merged VCD top-to-bottom, tracking the current `#<time>`
/// timestamp, and collect every timestamp at which any
/// `event<lane>_valid` lane transitions to `1`. Lane IDs are recovered
/// from the `$var wire 1 <id> event<lane>_valid $end` declarations.
fn tlm_call_pulse_times(merged_vcd: &str) -> Vec<u64> {
    // 1. Recover valid-lane VCD IDs from the header.
    let mut valid_ids: Vec<String> = Vec::new();
    for line in merged_vcd.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        // `$var wire 1 <id> event<N>_valid $end`
        if parts.len() >= 6
            && parts[0] == "$var"
            && parts[2] == "1"
            && parts[4].starts_with("event")
            && parts[4].ends_with("_valid")
        {
            valid_ids.push(parts[3].to_string());
        }
    }
    assert!(
        !valid_ids.is_empty(),
        "merged VCD has no `event<lane>_valid` lanes; header missing?"
    );

    let mut pulses = Vec::new();
    let mut cur_time: Option<u64> = None;
    // The merger emits `0<id>` at the start of each `#<t>` block and then
    // `1<id>` for active lanes (see emit_events_at_time). We collect
    // timestamps at which any valid lane goes high.
    let mut seen_at_time: std::collections::HashSet<u64> = std::collections::HashSet::new();
    for line in merged_vcd.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix('#') {
            if let Ok(t) = rest.parse::<u64>() {
                cur_time = Some(t);
            }
            continue;
        }
        if let Some(t) = cur_time {
            // Match `1<id>` exactly (no extra fields after the id).
            for vid in &valid_ids {
                let needle = format!("1{vid}");
                if l == needle && seen_at_time.insert(t) {
                    // record once per timestamp (independent of which lane)
                    pulses.push(t);
                    break;
                }
            }
        }
    }
    pulses
}

/// Single end-to-end test covering both blocking and OOO-tag paths. The
/// chosen fixture (`tlm_method_bus_test.harc`) drives `read` (blocking),
/// `poke` (blocking write), and `read_ooo` with two outstanding tags in a
/// single run.
#[test]
fn trace_merge_blocking_and_ooo_tags_e2e() {
    // VCD path requires Verilator >= 5.036; see src/main.rs:1167 — older versions need bare `--trace`.
    match detect_verilator_version() {
        Some((maj, min)) if (maj, min) < (MIN_VERILATOR_MAJOR, MIN_VERILATOR_MINOR) => {
            eprintln!(
                "SKIP trace_merge_blocking_and_ooo_tags_e2e: detected Verilator \
                 {maj}.{min:03}, need >= {MIN_VERILATOR_MAJOR}.{MIN_VERILATOR_MINOR:03}. \
                 `harc sim --wave-format vcd` passes `--trace-vcd` to Verilator \
                 (src/main.rs:1167), which only exists on Verilator >= 5.036."
            );
            return;
        }
        None => {
            eprintln!(
                "SKIP trace_merge_blocking_and_ooo_tags_e2e: could not detect \
                 Verilator version (need >= {MIN_VERILATOR_MAJOR}.{MIN_VERILATOR_MINOR:03}); \
                 see src/main.rs:1167."
            );
            return;
        }
        Some(_) => {}
    }

    let outdir = fresh_outdir("blocking_ooo");
    let trace_path = outdir.join("trace.jsonl");

    let (ok, log) = run_harc_sim(&outdir, &trace_path);
    assert!(
        ok,
        "harc sim --waves --wave-format vcd --record-trace failed.\n{log}\n\
         Note: requires Verilator new enough to accept `--trace-vcd` \
         (5.036+). On older Verilators the compiler emits a flag the \
         tool rejects."
    );

    // Locate the produced wave VCD. Default path is
    // `<outdir>/<TestName>.vcd` when --test is given, else `waves.vcd`.
    // The fixture has a single test, so `harc sim` defaults to that
    // test's struct name; we search the outdir to be tolerant of either
    // naming.
    let vcd_path = {
        let entries: Vec<_> = std::fs::read_dir(&outdir)
            .expect("read outdir")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("vcd"))
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "expected exactly one .vcd in {}; found {:?}\nsim log:\n{log}",
            outdir.display(),
            entries
        );
        entries.into_iter().next().unwrap()
    };
    assert!(
        trace_path.exists(),
        "trace.jsonl not produced by harc sim\nsim log:\n{log}"
    );

    // Now run the merger.
    let merged_path = outdir.join("merged.vcd");
    run_trace_merge(&vcd_path, &trace_path, &merged_path);

    let merged = std::fs::read_to_string(&merged_path).expect("read merged.vcd");
    let trace_text = std::fs::read_to_string(&trace_path).expect("read trace.jsonl");

    // --- Assertion 1: harc_semantic scope is present.
    assert!(
        merged.contains("$scope module harc_semantic $end"),
        "merged VCD missing $scope module harc_semantic $end\n--- merged head ---\n{}",
        merged.lines().take(40).collect::<Vec<_>>().join("\n")
    );

    // --- Assertion 2: tlm_call lane plumbing is present.
    // emit_semantic_header declares lanes like `event0_valid`,
    // `event0_type`, etc. and `HARC_TRACE_MAP event_type N tlm_call`
    // entries. Confirm both: at least one valid lane AND the
    // tlm_call mapping comment.
    assert!(
        merged.contains("event0_valid"),
        "merged VCD missing event0_valid lane"
    );
    assert!(
        merged.contains("$comment HARC_TRACE_MAP event_type") && merged.contains("tlm_call"),
        "merged VCD missing HARC_TRACE_MAP event_type ... tlm_call entry"
    );

    // --- Assertion 3: HARC_TRACE_MAP comments name the bus + methods
    // we actually called. The merger interns these into the `bus` and
    // `method` tables (see intern_event + emit_map_comments).
    assert!(
        merged.contains("HARC_TRACE_MAP bus ") && merged.contains(" mem "),
        "merged VCD missing bus=mem in HARC_TRACE_MAP"
    );
    for method in ["read", "poke", "read_ooo"] {
        assert!(
            merged.contains(&format!("HARC_TRACE_MAP method "))
                && merged.contains(&format!(" {method} ")),
            "merged VCD missing HARC_TRACE_MAP method ... {method} entry"
        );
    }

    // --- Assertion 4 (load-bearing, per PR #303): every tlm_call
    // event's `vcd_time` in the JSONL aligns with a `tlm_call` lane
    // pulse at exactly that VCD timestamp in the merged VCD. This
    // proves the merger used the alignment metadata rather than
    // re-deriving timing.
    let expected_times = tlm_call_vcd_times(&trace_text);
    assert!(
        expected_times.len() >= MIN_TLM_EVENTS,
        "expected >= {MIN_TLM_EVENTS} tlm_call events in trace, got {}: {:?}",
        expected_times.len(),
        expected_times
    );

    let pulse_times = tlm_call_pulse_times(&merged);
    for t in &expected_times {
        assert!(
            pulse_times.contains(t),
            "merged VCD has no semantic-lane pulse at #{t}; pulses observed: {:?}\n\
             expected (from JSONL vcd_time of tlm_call events): {:?}",
            pulse_times,
            expected_times
        );
    }
}
