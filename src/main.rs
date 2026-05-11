use clap::{Parser, Subcommand};
use miette::{IntoDiagnostic, NamedSource, Report, Result};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

#[derive(Parser, Debug)]
#[command(name = "harc", version, about = "HARC verification language compiler")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Parse and type-check HARC source file(s). Exits 0 on success.
    ///
    /// Today: parse only. Type-checking lands with phase 1a elaboration.
    /// Counterpart to `arch check`.
    Check {
        /// Input .harc file(s)
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// Print the parsed AST in debug form.
        #[arg(long)]
        ast: bool,
    },
    /// Pretty-print HARC source file(s) (round-trip-safe).
    Fmt {
        /// Input .harc file
        file: PathBuf,
        /// Write back to the file in place. Without this flag, prints to stdout.
        #[arg(short, long)]
        write: bool,
    },
    /// Compile a HARC test against a DUT and run it.
    ///
    /// Counterpart to `arch sim`. Two DUT paths are supported (spec §10.5):
    ///
    /// - `--dut <file.arch>` (one or more) — ARCH DUT path. HARC emits a
    ///   C++ TB and invokes `arch sim --tb` to compile and execute.
    /// - `--sv <file.sv>` (one or more) — Verilator-compiled SV DUT path
    ///   (interop). HARC emits a C++ TB and invokes Verilator directly to
    ///   build and run, no `arch` involvement at simulation time. The SV
    ///   may itself come from `arch build`.
    ///
    /// Pass exactly one of `--dut` or `--sv`. Multiple `.harc` input files
    /// may be passed — useful when scopes are split across files via
    /// `extend test T`. All input files are parsed; extends are merged
    /// into their base test before codegen.
    Sim {
        /// Input .harc file(s) — base test plus any extension files.
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// ARCH DUT source file(s) — repeat for designs with packages /
        /// shared bus definitions. Conflicts with `--sv`.
        #[arg(long, conflicts_with = "sv")]
        dut: Vec<PathBuf>,
        /// SystemVerilog DUT source file(s). Drives Verilator directly,
        /// bypassing `arch sim`. Conflicts with `--dut`.
        #[arg(long)]
        sv: Vec<PathBuf>,
        /// SV top-module name (Verilator `--top-module`). Defaults to the
        /// type of `let dut : <Type>` in the HARC source.
        #[arg(long)]
        top: Option<String>,
        /// Pick a specific test by name (when input contains more than one).
        #[arg(long)]
        test: Option<String>,
        /// Output directory for the generated C++ TB and arch_sim_build/
        #[arg(long)]
        outdir: Option<PathBuf>,
        /// PRNG seed for randomize() calls. Default: env HARC_SEED, else 1.
        /// The seed is logged to sim.log on every run for reproducibility.
        #[arg(long)]
        seed: Option<u64>,
        /// Just emit the C++ TB; do not invoke `arch sim`.
        #[arg(long)]
        emit_only: bool,
        /// Path to the `arch` binary (default: search $PATH, fall back to
        /// `cargo run --bin arch --manifest-path ../arch-com/Cargo.toml`).
        #[arg(long)]
        arch_bin: Option<PathBuf>,
        /// Run bound-driver/bound-monitor coroutine actors on dedicated
        /// OS threads with dual-barrier sync (Phase 3a). Default is the
        /// cooperative single-OS-thread model — typically faster on
        /// real fixtures because per-cycle barrier overhead exceeds
        /// per-cycle actor work. Use `--mt` for correctness validation
        /// of the multi-actor model, or for fixtures with substantial
        /// per-cycle compute that genuinely benefit from parallelism.
        #[arg(long)]
        mt: bool,
        /// Enable Verilator coverage collection (`--coverage`). The
        /// emitted TB writes `coverage.dat` next to the simulation
        /// log on clean shutdown; `verilator_coverage` can then
        /// post-process it. Used by the CVDP-style scorer at
        /// `bench/cvdp/score.py` to measure DUT coverage achieved by
        /// the TB. Off by default (small compile/runtime cost).
        #[arg(long)]
        coverage: bool,
    },
    // ── Learning store (sister to `arch advise` and friends, port of
    // arch-com/src/learn.rs). Every `harc check` / `harc sim` records
    // its failure→fix pairs into `~/.harc/learn/events.jsonl`; the
    // subcommands below let users (and agents) interact with the store
    // and retrieve past fixes. All on-device, no network. ─────────────
    /// Retrieve past error→fix pairs matching the query (BM25).
    Advise {
        /// Free-text query (matched against error codes, messages, diffs).
        /// May be omitted when `--from-stderr` is set.
        query: Vec<String>,
        /// Number of top results to print.
        #[arg(short = 'k', long, default_value_t = 3)]
        top: usize,
        /// Read the query from stdin (e.g.
        /// `harc check foo.harc 2>&1 | harc advise --from-stderr`).
        #[arg(long)]
        from_stderr: bool,
        /// Restrict the search to `kind: "feature"` events (the
        /// spec→source provenance from `///` / `//!` / `//! ---`
        /// doc comments harvested on successful compiles).
        /// Default is to return only error→fix events.
        #[arg(long)]
        feature: bool,
    },
    /// Rebuild the BM25 retrieval index over `~/.harc/learn/events.jsonl`.
    /// Run this after a batch of new error→fix pairs; `harc advise`
    /// works without an explicit index but a freshly-built one gives
    /// better IDF weighting.
    LearnIndex,
    /// Show stats about the local learning store (event counts by
    /// error_code, total store size).
    LearnStats,
    /// Delete the entire local learning store at `~/.harc/learn/`.
    LearnClear,
    /// Seed the local learning store with feature events harvested
    /// from a directory of `.harc` files. Walks the path recursively,
    /// parses each file, and emits one feature event per top-level
    /// construct that carries `///` / `//!` / `//! ---` content.
    /// Silently skips files that fail to parse. Re-running replaces
    /// the existing feature events for each harvested file — safe to
    /// run repeatedly. Build the BM25 index afterwards with
    /// `harc learn-index`.
    LearnBootstrap {
        /// Directory to walk (default: `tests/fixtures` under the
        /// current working directory). Recurses into subdirectories.
        #[arg(default_value = "tests/fixtures")]
        path: PathBuf,
    },
    /// Remove individual events from the learning store by filter.
    /// Combine filters freely; an event is removed if ANY filter matches.
    LearnPrune {
        /// Remove events whose error_code equals this string.
        #[arg(long)]
        code: Option<String>,
        /// Remove events whose diff/message/file_path contains this substring.
        #[arg(long)]
        contains: Option<String>,
        /// Remove events older than this many days.
        #[arg(long)]
        older_than_days: Option<u64>,
        /// Report what would be removed without modifying the store.
        #[arg(long)]
        dry_run: bool,
    },
    // Future, mirroring ARCH:
    //   Build  — transpile to SystemVerilog + UVM (spec §10.2, phase 5)
    //   Formal — emit BTOR2 / SMT-LIB2 (spec §10.3, phase 4)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Check { files, ast } => learn_wrap(&files, || cmd_check(files.clone(), ast)),
        Cmd::Fmt { file, write } => cmd_fmt(file, write),
        Cmd::Sim { files, dut, sv, top, test, outdir, seed, emit_only, arch_bin, mt, coverage } => {
            let captured = files.clone();
            learn_wrap(&captured, || cmd_sim(
                files.clone(), dut.clone(), sv.clone(), top.clone(), test.clone(),
                outdir.clone(), seed, emit_only, arch_bin.clone(), mt, coverage,
            ))
        }
        Cmd::Advise { query, top, from_stderr, feature } =>
            cmd_advise(query, top, from_stderr, feature),
        Cmd::LearnBootstrap { path } => cmd_learn_bootstrap(&path),
        Cmd::LearnIndex => {
            let n = harc::learn::build_index().map_err(|e| miette::miette!("{}", e))?;
            eprintln!("Indexed {n} events.");
            Ok(())
        }
        Cmd::LearnStats => {
            harc::learn::print_stats().map_err(|e| miette::miette!("{}", e))?;
            Ok(())
        }
        Cmd::LearnClear => {
            harc::learn::clear_store().map_err(|e| miette::miette!("{}", e))?;
            eprintln!("Cleared ~/.harc/learn/");
            Ok(())
        }
        Cmd::LearnPrune { code, contains, older_than_days, dry_run } => {
            let (kept, removed) = harc::learn::prune(
                code.as_deref(),
                contains.as_deref(),
                older_than_days,
                dry_run,
            ).map_err(|e| miette::miette!("{}", e))?;
            if dry_run {
                eprintln!("[dry-run] would remove {removed} event(s); {kept} would remain.");
            } else {
                eprintln!("Removed {removed} event(s); {kept} remain.");
            }
            Ok(())
        }
    }
}

/// Wrap a `harc check` / `harc sim` invocation with learning capture.
/// Honors `HARC_NO_LEARN=1` opt-out. On failure, stashes a pending
/// record per input file; on success, pairs with any prior pending
/// record to emit an `error_fix` event. Also prints an inline
/// `💡 harc advise found N similar past fixes` hint when the store
/// has past matches for the failing message.
fn learn_wrap<F>(files: &[PathBuf], f: F) -> miette::Result<()>
where
    F: FnOnce() -> Result<()>,
{
    let enabled = harc::learn::is_enabled();
    if enabled {
        let _ = harc::learn::maybe_print_first_run_notice();
    }
    let result = f();
    if !enabled {
        return result;
    }
    match &result {
        Ok(()) => {
            for file in files {
                let path_str = file.display().to_string();
                if let Ok(src) = fs::read_to_string(file) {
                    if let Ok(Some(ev)) = harc::learn::record_success_if_pending(&path_str, &src) {
                        eprintln!("📚 Learned: [{}] {}", ev.error_code, ev.diff_summary);
                    }
                    // Feature harvest: emit one `kind: "feature"` event
                    // per top-level construct that carries doc text.
                    // Re-running on the same source replaces those
                    // events (idempotent). Silently skipped if the
                    // file fails to parse — error_fix capture above
                    // covered that case.
                    if let Ok(ast) = harc::parser::parse_source(&src) {
                        let file_path = path_str.clone();
                        let _ = harc::learn::harvest_features(&ast, |_item| {
                            file_path.clone()
                        });
                    }
                }
            }
        }
        Err(report) => {
            let msg = format!("{report:?}");
            let code = harc::learn::classify_error(&msg);
            for file in files {
                let path_str = file.display().to_string();
                if let Ok(src) = fs::read_to_string(file) {
                    let _ = harc::learn::record_failure(&path_str, &code, &msg, &src);
                }
            }
            // Inline hint: if the local store has similar past fixes,
            // tell the user. `peek` doesn't bump retrieval counters.
            let query = format!("{code} {msg}");
            if let Ok(hits) = harc::learn::peek(&query, 3) {
                if !hits.is_empty() {
                    let suggest = hits[0].event.error_code.clone();
                    eprintln!(
                        "💡 harc advise found {} similar past fix{} — run `harc advise \"{}\"` to see them.",
                        hits.len(),
                        if hits.len() == 1 { "" } else { "es" },
                        suggest,
                    );
                }
            }
        }
    }
    result
}

/// `harc advise <query>` — retrieve top-K past error→fix pairs.
/// With `--feature`, retrieves `kind: "feature"` events instead
/// (spec→source provenance from harvested doc-comments).
fn cmd_advise(query: Vec<String>, top: usize, from_stderr: bool, feature: bool) -> Result<()> {
    let mut q = query.join(" ");
    if from_stderr {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf).into_diagnostic()?;
        if !buf.trim().is_empty() {
            if !q.is_empty() { q.push(' '); }
            q.push_str(buf.trim());
        }
    }
    if q.trim().is_empty() {
        return Err(miette::miette!(
            "empty query — pass a query string or pipe via --from-stderr"
        ));
    }
    // Pull a deeper pool when `--feature` is set so filtering doesn't
    // starve the result set.
    let pool_size = if feature { top.max(1) * 8 } else { top };
    let matches = harc::learn::advise(&q, pool_size).map_err(|e| miette::miette!("{}", e))?;
    let matches: Vec<_> = if feature {
        matches.into_iter().filter(|m| m.event.kind == "feature").take(top).collect()
    } else {
        matches.into_iter().filter(|m| m.event.kind != "feature").take(top).collect()
    };
    if matches.is_empty() {
        eprintln!("No matches.");
        return Ok(());
    }
    for (i, m) in matches.iter().enumerate() {
        println!(
            "── match #{} (score {:.3}, retrieved {}×) ──────────────────────",
            i + 1, m.score, m.retrieved_count
        );
        if m.event.kind == "feature" {
            // Feature event: file::construct + doc snippet.
            println!("  kind:      {}", m.event.error_code);
            println!("  construct: {}", m.event.diff_summary);
            println!("  file:      {}", m.event.file_path);
            let snippet: String = m.event.error_message.chars().take(240).collect();
            let truncated = m.event.error_message.chars().count() > 240;
            println!(
                "  doc:       {}{}",
                snippet.replace('\n', " "),
                if truncated { " …" } else { "" }
            );
        } else {
            println!("  code:    {}", m.event.error_code);
            println!("  message: {}", m.event.error_message);
            println!("  file:    {}", m.event.file_path);
            println!("  diff:    {}", m.event.diff_summary);
        }
        println!();
    }
    Ok(())
}

/// `harc learn-bootstrap <dir>` — recursively walk a directory of
/// `.harc` files, parse each, and call `harvest_features` on every
/// successful parse. Idempotent: re-running replaces existing feature
/// events for each file. Files that fail to parse are skipped
/// silently.
fn cmd_learn_bootstrap(path: &std::path::Path) -> Result<()> {
    if std::env::var("HARC_NO_LEARN").is_ok_and(|v| v != "0") {
        eprintln!("HARC_NO_LEARN is set — bootstrap skipped.");
        return Ok(());
    }
    if !path.exists() {
        return Err(miette::miette!("path does not exist: {}", path.display()));
    }
    let mut harc_files: Vec<PathBuf> = Vec::new();
    fn walk(p: &std::path::Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        if p.is_dir() {
            for entry in fs::read_dir(p)? {
                let entry = entry?;
                walk(&entry.path(), out)?;
            }
        } else if p.is_file() {
            if p.extension().and_then(|e| e.to_str()) == Some("harc") {
                out.push(p.to_path_buf());
            }
        }
        Ok(())
    }
    walk(path, &mut harc_files).into_diagnostic()?;
    harc_files.sort();
    let mut total = 0usize;
    let mut parsed_ok = 0usize;
    let mut parse_skipped = 0usize;
    for f in &harc_files {
        let src = match fs::read_to_string(f) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let parsed = match harc::parser::parse_source(&src) {
            Ok(p) => p,
            Err(_) => {
                parse_skipped += 1;
                continue;
            }
        };
        parsed_ok += 1;
        let file_path = f.display().to_string();
        let n = harc::learn::harvest_features(&parsed, |_item| file_path.clone())
            .map_err(|e| miette::miette!("{}", e))?;
        total += n;
    }
    eprintln!(
        "Bootstrap: parsed {parsed_ok}/{} files ({} skipped); harvested {total} feature event(s).",
        harc_files.len(),
        parse_skipped,
    );
    eprintln!("Run `harc learn-index` to rebuild the BM25 index.");
    Ok(())
}

/// Resolve `use Name;` items in the parsed test files against a small
/// set of search paths and return any extra parsed `SourceFile`s
/// containing the imported items (currently `bus` decls). Search
/// order:
///
/// 1. `$HARC_LIB_PATH` (colon-separated, like `PATH`).
/// 2. The repo's own `stdlib/` directory (relative to the first
///    input file, then to the working directory).
/// 3. Sibling `../arch-com/stdlib/` and `../arch-com/examples/`
///    (relative to the input file's directory).
///
/// Each resolved file is parsed; only `Item::Bus` items survive (the
/// rest are dropped — HARC isn't a full ARCH compiler). Unresolved
/// `use` paths silently no-op so existing fixtures with
/// `use arc.stdlib.BusAxi4` lines that don't yet match anything keep
/// parsing.
fn resolve_use_imports(
    files: &[harc::ast::SourceFile],
    first_input: Option<&PathBuf>,
) -> Vec<harc::ast::SourceFile> {
    use harc::ast::Item;

    let mut wanted: std::collections::HashSet<String> = std::collections::HashSet::new();
    for f in files {
        for it in &f.items {
            if let Item::Use(u) = it {
                if let Some(last) = u.path.segments.last() {
                    wanted.insert(last.name.clone());
                }
            }
        }
    }
    if wanted.is_empty() {
        return Vec::new();
    }

    // Build search-path list.
    let mut search: Vec<PathBuf> = Vec::new();
    if let Ok(envp) = std::env::var("HARC_LIB_PATH") {
        for p in envp.split(':') {
            if !p.is_empty() { search.push(PathBuf::from(p)); }
        }
    }
    let input_dir = first_input
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    search.push(input_dir.join("stdlib"));
    search.push(PathBuf::from("stdlib"));
    search.push(input_dir.join("../arch-com/stdlib"));
    search.push(input_dir.join("../arch-com/examples"));
    search.push(PathBuf::from("../arch-com/stdlib"));
    search.push(PathBuf::from("../arch-com/examples"));

    let mut imported: Vec<harc::ast::SourceFile> = Vec::new();
    let mut already: std::collections::HashSet<String> = files.iter()
        .flat_map(|f| f.items.iter().filter_map(|it| match it {
            Item::Bus(b) => Some(b.name.name.clone()),
            _ => None,
        }))
        .collect();

    for name in &wanted {
        if already.contains(name) { continue; }
        let mut found_path: Option<PathBuf> = None;
        for dir in &search {
            for ext in &["arch", "harc"] {
                let candidate = dir.join(format!("{name}.{ext}"));
                if candidate.exists() { found_path = Some(candidate); break; }
            }
            if found_path.is_some() { break; }
        }
        let Some(path) = found_path else { continue; };

        let src = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let parsed = match harc::parser::parse_source(&src) {
            Ok(p) => p,
            Err(_) => continue,  // skip files we can't parse
        };
        let bus_only: Vec<Item> = parsed.items.into_iter()
            .filter(|it| matches!(it, Item::Bus(_)))
            .collect();
        if !bus_only.is_empty() {
            for it in &bus_only {
                if let Item::Bus(b) = it { already.insert(b.name.name.clone()); }
            }
            imported.push(harc::ast::SourceFile {
                items: bus_only,
                inner_doc: None,
                frontmatter: None,
            });
        }
    }
    imported
}

fn parse_file(path: &PathBuf) -> Result<harc::ast::SourceFile> {
    let src = fs::read_to_string(path).into_diagnostic()?;
    harc::parser::parse_source(&src).map_err(|e| {
        Report::new(e).with_source_code(NamedSource::new(
            path.display().to_string(),
            src,
        ))
    })
}

fn cmd_check(files: Vec<PathBuf>, ast: bool) -> Result<()> {
    let mut total_items = 0;
    for file in &files {
        let parsed = parse_file(file)?;
        total_items += parsed.items.len();
        if ast {
            println!("// {}", file.display());
            println!("{:#?}", parsed);
        }
    }
    if !ast {
        println!("ok: {} file(s), {} top-level item(s)", files.len(), total_items);
    }
    Ok(())
}

fn cmd_fmt(file: PathBuf, write: bool) -> Result<()> {
    let parsed = parse_file(&file)?;
    let out = harc::pretty::print(&parsed);
    if write {
        fs::write(&file, out).into_diagnostic()?;
    } else {
        print!("{out}");
    }
    Ok(())
}

/// Drive Verilator directly against an SV DUT plus the HARC-emitted C++ TB.
/// Verilator's `--build --exe` builds + links + produces `<obj_dir>/V<top>`;
/// we then run that binary with `HARC_SIM_LOG` and `HARC_LOG_DIR` set so
/// `sim.log` and any `logf("foo.log", ...)` files land next to build outputs.
fn run_verilator(
    top: &str,
    sv: &[PathBuf],
    cpp: &PathBuf,
    outdir_abs: &PathBuf,
    sim_log_path: &PathBuf,
    seed: Option<u64>,
    coverage: bool,
) -> Result<()> {
    let mdir = outdir_abs.join("obj_dir");
    let _ = fs::remove_dir_all(&mdir); // start clean — stale .o's bite us
    fs::create_dir_all(&mdir).into_diagnostic()?;

    // Detect Z3 (Homebrew on macOS, /usr/{include,lib} on Linux). When
    // present, link it so the generated TB can use the inline solver path
    // for `randomize(t) with <constraints>`. When absent, the build still
    // works for solver-free TBs; constraint TBs would fail at link time
    // with a clearer error than verilator default.
    let z3_inc = ["/opt/homebrew/include", "/usr/local/include", "/usr/include"]
        .iter().map(PathBuf::from).find(|p| p.join("z3++.h").exists());
    let z3_lib = ["/opt/homebrew/lib", "/usr/local/lib", "/usr/lib"]
        .iter().map(PathBuf::from).find(|p| {
            p.join("libz3.dylib").exists() || p.join("libz3.so").exists()
        });

    let mut args: Vec<String> = vec![
        "--cc".into(), "--exe".into(), "--build".into(),
        "-Wno-fatal".into(), "-Wno-WIDTH".into(),
        // Tolerate SV quirks Xcelium accepts but Verilator escalates:
        //   BLKANDNBLK — same variable written by `=` in one block
        //                and `<=` in another. Common in CVDP DUTs
        //                (e.g. resets via `=` + clocked updates via
        //                `<=` on the same reg).
        //   UNOPTFLAT  — combinational signal in an `always_comb`
        //                that Verilator's optimizer flags as
        //                potentially looped (false positive on most
        //                CVDP DUTs).
        "-Wno-BLKANDNBLK".into(), "-Wno-UNOPTFLAT".into(),
        // Cycle-based TBs don't need delay semantics; tell Verilator
        // to elide `#N` delay statements rather than refusing to
        // elaborate. HARC's `wait N cycles` is always cycle-based
        // (handled by the runtime scheduler) — delays inside a DUT
        // are a property of the DUT author, not the TB, and CVDP
        // coverage scoring ignores delay semantics too.
        "--no-timing".into(),
        "--top-module".into(), top.into(),
        "--Mdir".into(), mdir.display().to_string(),
    ];
    if coverage {
        // Enable Verilator coverage on the DUT — full umbrella
        // (`line` + `toggle` + `expr` + `user`). For CVDP cid012
        // scoring we aggregate across all metrics, mirroring how
        // Cadence IMC's "Average %" combines block, branch, toggle,
        // and expression coverage into one number. Different DUT
        // classes lean on different metrics:
        //   - Pure-dataflow modules (only `assign` + sub-instances,
        //     no `always` blocks) score 0/0 on line+branch alone —
        //     toggle on signals is the only meaningful metric.
        //   - Combinational `always @(*)` with wide internal regs
        //     get line+branch hits; toggle on internal bits beyond
        //     the input range is unreachable but offsets only a
        //     small fraction of the total denominator.
        //   - Sequential modules get a mix of all four.
        //
        // Emitted TB writes `coverage.dat` at clean shutdown (see
        // cpp_tb.rs main() emission). `verilator_coverage`
        // post-processes the .dat into per-instance metrics that
        // the CVDP-style scorer reads.
        args.push("--coverage".into());
    }
    args.extend([
        // Force C++20 by overriding verilator's default
        // `CFG_CXXFLAGS_STD = -std=gnu++17` Makefile variable, so
        // `<coroutine>` and our `co_await`-based wait primitives
        // compile. Verilator's own CFLAGS append AFTER user CFLAGS
        // on the compile command line, so user `-std=c++20` would
        // be overridden — `-MAKEFLAGS` is forwarded to `make` and
        // replaces the variable cleanly.
        //
        // Optimization stays at verilator's default `-Os`. The
        // emitted test `.cpp` opts out via `#pragma clang optimize
        // off` (see cpp_tb.rs's emit prelude) — clang 17+ on Apple
        // Silicon and Linux x86_64 mis-optimizes our `[&]`-capturing
        // C++20 lambda coroutines at `-Os` / `-O2` (closure reference
        // members fold against a freed stack frame after suspension,
        // SEGV on resume). The pragma is per-file so the verilator-
        // generated DUT code (in separate .cpp files) keeps `-Os`
        // for fast simulation.
        //
        // GCC has the same class of miscompile, but its
        // `#pragma optimize` doesn't propagate correctly through
        // C++20 coroutine codegen (`-O0`-pragma SEGVs trivial
        // tests; `-O1`-pragma still SEGVs the bound-actor tests).
        // CI sets `CXX=clang++-15` for the verilator build so the
        // clang pragma applies on both platforms; see
        // `.github/workflows/ci.yml`.
        "-MAKEFLAGS".into(),
        // `CXX=${HARC_CXX:-c++}` lets CI override the compiler
        // without changing harc-com source. On macOS, `c++` aliases
        // clang and the existing `#pragma clang optimize off` does
        // its thing. On Linux GitHub runners, `c++` would alias
        // g++ — and GCC's pragma-equivalent doesn't propagate
        // through C++20 coroutine codegen — so CI sets
        // `HARC_CXX=clang++` explicitly. Local Linux users without
        // the env var get the system `c++` (g++ on most distros);
        // they can `export HARC_CXX=clang++` if they want the
        // clang fix.
        format!(
            "CFG_CXXFLAGS_STD=-std=gnu++20 CXX={}",
            std::env::var("HARC_CXX").unwrap_or_else(|_| "c++".to_string())
        ),
    ]);
    // Make the build dir an include path so the emitted `.cpp`'s
    // `#include "harc_thread_rt.h"` resolves — verilator builds in
    // `obj_dir/` (cwd at compile time) and the header lives one level up.
    args.push("-CFLAGS".into());
    args.push(format!("-I{}", outdir_abs.display()));
    if let Some(inc) = &z3_inc {
        args.push("-CFLAGS".into());
        args.push(format!("-I{}", inc.display()));
    }
    if let Some(lib) = &z3_lib {
        args.push("-LDFLAGS".into());
        args.push(format!("-L{} -lz3", lib.display()));
    }
    for s in sv {
        args.push(s.display().to_string());
    }
    args.push(cpp.display().to_string());

    let build_log_path = outdir_abs.join("build.log");
    eprintln!("running: verilator {}", args.join(" "));
    let output = Command::new("verilator")
        .args(&args)
        .output()
        .into_diagnostic()?;
    // Tee verilator/clang output: print to terminal + persist in build.log
    // for post-mortem (warnings, deprecation notices, missing symbols).
    let mut build_log = String::new();
    build_log.push_str(&format!("$ verilator {}\n\n", args.join(" ")));
    build_log.push_str("--- stdout ---\n");
    build_log.push_str(&String::from_utf8_lossy(&output.stdout));
    build_log.push_str("\n--- stderr ---\n");
    build_log.push_str(&String::from_utf8_lossy(&output.stderr));
    fs::write(&build_log_path, &build_log).into_diagnostic()?;
    if !output.stdout.is_empty() {
        std::io::Write::write_all(&mut std::io::stdout(), &output.stdout).into_diagnostic()?;
    }
    if !output.stderr.is_empty() {
        std::io::Write::write_all(&mut std::io::stderr(), &output.stderr).into_diagnostic()?;
    }
    eprintln!("build.log written to {}", build_log_path.display());
    if !output.status.success() {
        return Err(miette::miette!("verilator build failed (status {})", output.status));
    }

    let bin = mdir.join(format!("V{top}"));
    eprintln!("running: {}", bin.display());
    let mut cmd = Command::new(&bin);
    cmd.env("HARC_SIM_LOG", sim_log_path)
       .env("HARC_LOG_DIR", outdir_abs);
    if let Some(s) = seed {
        cmd.env("HARC_SEED", s.to_string());
    }
    let status = cmd.status().into_diagnostic()?;
    if status.success() {
        eprintln!("sim.log written to {}", sim_log_path.display());
    } else {
        return Err(miette::miette!("simulation exited with status {status}"));
    }
    Ok(())
}

fn cmd_sim(
    files: Vec<PathBuf>,
    dut: Vec<PathBuf>,
    sv: Vec<PathBuf>,
    top: Option<String>,
    test: Option<String>,
    outdir: Option<PathBuf>,
    seed: Option<u64>,
    emit_only: bool,
    arch_bin: Option<PathBuf>,
    mt: bool,
    coverage: bool,
) -> Result<()> {
    if dut.is_empty() && sv.is_empty() {
        return Err(miette::miette!("pass either --dut <file.arch> or --sv <file.sv>"));
    }

    // Parse every input file, then fold `extend test T` blocks into their
    // matching base test before codegen.
    let mut parsed_files = Vec::with_capacity(files.len());
    for f in &files {
        parsed_files.push(parse_file(f)?);
    }
    // Resolve `use Name` declarations against the search path. For each
    // unresolved `use`, look for `<Name>.arch` (or `<Name>.harc`) in
    // a small set of conventional locations, parse it, and append any
    // `bus` items it declares to the synthetic file list. Unresolved
    // uses silently no-op (back-compat — many existing fixtures
    // include `use arc.stdlib.X` lines that don't resolve to anything
    // yet).
    let extra_files = resolve_use_imports(&parsed_files, files.first());
    let mut all_files = parsed_files;
    all_files.extend(extra_files);

    let merged = harc::codegen::merge::merge_for_sim(&all_files, test.as_deref())
        .map_err(|e| miette::miette!("{}", e))?;

    let cpp = harc::codegen::cpp_tb::emit_with_opts(&merged, harc::codegen::cpp_tb::EmitOpts { mt })
        .map_err(|e| miette::miette!("{}", e))?;

    let outdir = outdir.unwrap_or_else(|| PathBuf::from("harc_sim_build"));
    fs::create_dir_all(&outdir).into_diagnostic()?;
    let stem = files[0].file_stem().and_then(|s| s.to_str()).unwrap_or("harc_tb");
    let cpp_path = outdir.join(format!("{stem}.cpp"));
    fs::write(&cpp_path, &cpp).into_diagnostic()?;
    eprintln!("emitted {}", cpp_path.display());

    // Drop the coroutine runtime header alongside the emitted .cpp so
    // verilator's standard `--Mdir`-relative include search picks it up
    // without needing an extra `-I` flag. The .cpp file `#include`s
    // it as `"harc_thread_rt.h"`. Bundled as a baked-in string via
    // `include_str!` so a binary install of `harc` ships the runtime
    // without a separate file dependency.
    let rt_header_path = outdir.join("harc_thread_rt.h");
    fs::write(&rt_header_path, harc::codegen::cpp_tb::THREAD_RT_HEADER)
        .into_diagnostic()?;

    if emit_only {
        return Ok(());
    }

    let cpp_abs = fs::canonicalize(&cpp_path).into_diagnostic()?;
    let outdir_abs = fs::canonicalize(&outdir).into_diagnostic()?;
    let sim_log_path = outdir_abs.join("sim.log");

    if !sv.is_empty() {
        // SV / Verilator path — no `arch sim` involvement. Resolves the top
        // module name from `--top` if given, else from the HARC `let dut : T`.
        let top_name = top.or_else(|| harc::codegen::cpp_tb::dut_type_name(&merged))
            .ok_or_else(|| miette::miette!(
                "could not determine SV top module — pass --top or declare `let dut : T`"
            ))?;
        let mut sv_abs = Vec::with_capacity(sv.len());
        for s in &sv {
            sv_abs.push(fs::canonicalize(s).into_diagnostic()?);
        }
        return run_verilator(&top_name, &sv_abs, &cpp_abs, &outdir_abs, &sim_log_path, seed, coverage);
    }

    // ARCH path: run `arch sim <dut...> --tb <cpp_path>`.
    let mut dut_abs = Vec::with_capacity(dut.len());
    for d in &dut {
        dut_abs.push(fs::canonicalize(d).into_diagnostic()?);
    }

    let (program, mut prefix_args, working_dir) = match &arch_bin {
        Some(p) => (p.clone(), Vec::<String>::new(), std::env::current_dir().into_diagnostic()?),
        None => {
            // Default: invoke arch via cargo against the sibling arch-com checkout.
            // Working dir = the arch-com root so its relative paths (runtime/, etc.) resolve.
            let arch_root = std::env::current_dir()
                .into_diagnostic()?
                .join("..")
                .join("arch-com");
            (
                PathBuf::from("cargo"),
                vec![
                    "run".into(),
                    "--quiet".into(),
                    "--manifest-path".into(),
                    arch_root.join("Cargo.toml").display().to_string(),
                    "--bin".into(),
                    "arch".into(),
                    "--".into(),
                ],
                arch_root,
            )
        }
    };
    prefix_args.push("sim".into());
    for d in &dut_abs {
        prefix_args.push(d.display().to_string());
    }
    prefix_args.push("--tb".into());
    prefix_args.push(cpp_abs.display().to_string());
    prefix_args.push("--outdir".into());
    prefix_args.push(outdir_abs.display().to_string());

    eprintln!("running: {} {}", program.display(), prefix_args.join(" "));
    let mut cmd = Command::new(&program);
    cmd.args(&prefix_args)
       .current_dir(&working_dir)
       .env("HARC_SIM_LOG", &sim_log_path)
       // Anchor relative `logf("foo.log", ...)` paths to the build dir so
       // per-component log files land next to sim.log instead of under
       // arch-com/ (where the binary actually runs from).
       .env("HARC_LOG_DIR", &outdir_abs);
    if let Some(s) = seed {
        cmd.env("HARC_SEED", s.to_string());
    }
    let status = cmd.status().into_diagnostic()?;
    if status.success() {
        eprintln!("sim.log written to {}", sim_log_path.display());
    }
    if !status.success() {
        return Err(miette::miette!("arch sim exited with status {status}"));
    }
    Ok(())
}
