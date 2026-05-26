use clap::{Parser, Subcommand};
use miette::{IntoDiagnostic, NamedSource, Report, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

mod trace_merge;

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
    /// Today: parse plus selected backend/codegen limitation checks.
    /// Type-checking lands with phase 1a elaboration.
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
    ///   may itself come from `arch build`. Verilator control files may be
    ///   passed separately with `--vlt <file.vlt>`.
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
        /// Verilator control file(s), typically `.vlt` waivers or coverage
        /// controls. Forwarded to Verilator before the SV DUT files.
        #[arg(long, conflicts_with = "dut")]
        vlt: Vec<PathBuf>,
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
        /// Enable DUT coverage collection. Works on both DUT paths:
        ///   * `--sv` (Verilator): passes `--coverage` to verilator;
        ///     the emitted TB writes `coverage.dat` next to sim.log
        ///     at clean shutdown.
        ///   * `--dut` (ARCH sim): passes `--coverage` +
        ///     `--coverage-dat=<outdir>/coverage.dat` to `arch sim`,
        ///     which dumps both a `coverage.txt` keyed to .arch
        ///     source lines AND a Verilator-compatible `coverage.dat`.
        /// Output is consumed by `verilator_coverage` and by the
        /// CVDP-style scorer at `bench/cvdp/score.py`. Off by default
        /// (small compile/runtime cost).
        #[arg(long)]
        coverage: bool,
        /// Reference-model C / C++ source file(s) — implementations
        /// for `extern function` declarations (spec §9). Repeatable.
        /// Each file is passed verbatim to the verilator invocation
        /// alongside the emitted TB `.cpp`, so the linker resolves
        /// `extern "C"` forward declarations against them. Typical
        /// use: a one-file reference model (CRC, AES, ISA simulator)
        /// the scoreboard calls to compute expected outputs.
        #[arg(long)]
        ref_src: Vec<PathBuf>,
        /// Z3 installation prefix. Looks for include/z3++.h and lib*/libz3.
        #[arg(long)]
        z3_root: Option<PathBuf>,
        /// Explicit Z3 include directory containing z3++.h.
        #[arg(long)]
        z3_include_dir: Option<PathBuf>,
        /// Explicit Z3 library directory containing libz3.
        #[arg(long)]
        z3_lib_dir: Option<PathBuf>,
        /// Force a clean rebuild: wipes `<outdir>/obj_dir/` before
        /// invoking Verilator. Default (off) reuses the existing
        /// Verilator output when the emitted `.cpp` is byte-identical
        /// — `harc sim --test foo` then `harc sim --test bar` against
        /// the same source skips Verilator entirely. Pass `--rebuild`
        /// when Verilator's version changed, when SV flags changed
        /// in a way the `.cpp` doesn't capture, or when investigating
        /// a suspected stale-`.o` problem. See
        /// docs/separate-compilation-plan.md §1c.
        #[arg(long)]
        rebuild: bool,
        /// Record a semantic execution trace as JSONL. The generated
        /// testbench writes one metadata header followed by runtime
        /// events such as logs, failures, and randomization results.
        #[arg(long)]
        record_trace: Option<PathBuf>,
        /// Enable Verilator VCD/FST waveform dumping. Implies trace
        /// codegen in the emitted C++ TB and `--trace-vcd` /
        /// `--trace-fst` on the Verilator command. Default format is
        /// FST (smaller + faster for large regressions); override
        /// with `--wave-format vcd`. Wave file lands in `<outdir>`
        /// unless `--wave-file` is given. When flipping waveforms on
        /// after a non-waves build (or changing format), pass
        /// `--rebuild` because Verilator reuses cached objects.
        #[arg(long)]
        waves: bool,
        /// Waveform format. `vcd` is verbose but universally
        /// readable; `fst` is compact + indexed but requires GTKWave
        /// (or similar). Default: `fst`.
        #[arg(long, value_parser = ["vcd", "fst"], default_value = "fst")]
        wave_format: String,
        /// Path for the waveform output. Default:
        /// `<outdir>/<TestName>.<vcd|fst>` (or `<outdir>/waves.<ext>`
        /// when no test is selected).
        #[arg(long)]
        wave_file: Option<PathBuf>,
        /// Trace hierarchy depth passed to `dut->trace(tfp, N)`.
        /// Default 99 (deep enough for any realistic DUT).
        #[arg(long, default_value_t = 99)]
        trace_depth: i32,
        /// Disable expansion of packed structs in the waveform
        /// (Verilator `--trace-structs`). Defaults to off — i.e.
        /// when `--waves` is set, struct expansion is on by default
        /// because flat aggregate vectors hide field-level debug.
        /// Pass `--no-trace-structs` to fall back to vectors.
        #[arg(long = "no-trace-structs", action = clap::ArgAction::SetTrue)]
        no_trace_structs: bool,
        /// Maximum traced signal width in bits (Verilator
        /// `--trace-max-width`). Defaults to 8192 when `--waves` is
        /// set so wide packed structs like CSR mirrors stay visible.
        #[arg(long, default_value_t = 8192)]
        trace_max_width: u32,
        /// Maximum traced array size (Verilator `--trace-max-array`).
        /// Only forwarded when set explicitly.
        #[arg(long)]
        trace_max_array: Option<u32>,
        /// Additional Verilator build flag. Repeatable. Appended to
        /// the Verilator command after HARC's defaults but before
        /// SV inputs. Example:
        /// `--verilator-arg --public-flat-rw --verilator-arg -Wno-UNUSEDSIGNAL`.
        #[arg(long = "verilator-arg")]
        verilator_args: Vec<String>,
        /// Additional argument for the generated simulation binary
        /// (e.g. `+plusarg=value`). Repeatable. Forwarded verbatim
        /// after the `--test` selector.
        #[arg(long = "sim-arg")]
        sim_args: Vec<String>,
    },
    /// Merge a semantic JSONL trace into a signal VCD as synthetic events.
    TraceMerge {
        /// Signal waveform VCD produced by `harc sim --waves --wave-format vcd`.
        #[arg(long)]
        vcd: PathBuf,
        /// Semantic JSONL trace produced by `harc sim --record-trace`.
        #[arg(long)]
        trace: PathBuf,
        /// Output merged VCD path.
        #[arg(long)]
        out: PathBuf,
        /// Optional JSON sidecar mapping numeric waveform IDs back to strings.
        #[arg(long)]
        map_out: Option<PathBuf>,
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
        Cmd::Sim {
            files,
            dut,
            sv,
            vlt,
            top,
            test,
            outdir,
            seed,
            emit_only,
            arch_bin,
            mt,
            coverage,
            ref_src,
            z3_root,
            z3_include_dir,
            z3_lib_dir,
            rebuild,
            record_trace,
            waves,
            wave_format,
            wave_file,
            trace_depth,
            no_trace_structs,
            trace_max_width,
            trace_max_array,
            verilator_args,
            sim_args,
        } => {
            let captured = files.clone();
            learn_wrap(&captured, || {
                cmd_sim(
                    files.clone(),
                    dut.clone(),
                    sv.clone(),
                    vlt.clone(),
                    top.clone(),
                    test.clone(),
                    outdir.clone(),
                    seed,
                    emit_only,
                    arch_bin.clone(),
                    mt,
                    coverage,
                    ref_src.clone(),
                    Z3PathOpts {
                        root: z3_root.clone(),
                        include_dir: z3_include_dir.clone(),
                        lib_dir: z3_lib_dir.clone(),
                    },
                    rebuild,
                    record_trace.clone(),
                    WaveOpts {
                        waves,
                        format: wave_format.clone(),
                        file: wave_file.clone(),
                        trace_depth,
                        trace_structs: !no_trace_structs,
                        trace_max_width,
                        trace_max_array,
                        verilator_args: verilator_args.clone(),
                        sim_args: sim_args.clone(),
                    },
                )
            })
        }
        Cmd::TraceMerge {
            vcd,
            trace,
            out,
            map_out,
        } => trace_merge::cmd_trace_merge(&vcd, &trace, &out, map_out.as_deref()),
        Cmd::Advise {
            query,
            top,
            from_stderr,
            feature,
        } => cmd_advise(query, top, from_stderr, feature),
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
        Cmd::LearnPrune {
            code,
            contains,
            older_than_days,
            dry_run,
        } => {
            let (kept, removed) = harc::learn::prune(
                code.as_deref(),
                contains.as_deref(),
                older_than_days,
                dry_run,
            )
            .map_err(|e| miette::miette!("{}", e))?;
            if dry_run {
                eprintln!("[dry-run] would remove {removed} event(s); {kept} would remain.");
            } else {
                eprintln!("Removed {removed} event(s); {kept} remain.");
            }
            Ok(())
        }
    }
}

/// Waveform / Verilator pass-through options carried through `cmd_sim`
/// into `run_verilator`. See `Cmd::Sim` argument docs for the per-field
/// semantics. The struct exists so the long `cmd_sim` signature doesn't
/// grow another nine parameters.
#[derive(Debug, Default, Clone)]
struct WaveOpts {
    waves: bool,
    format: String,
    file: Option<PathBuf>,
    trace_depth: i32,
    trace_structs: bool,
    trace_max_width: u32,
    trace_max_array: Option<u32>,
    verilator_args: Vec<String>,
    sim_args: Vec<String>,
}

#[derive(Debug, Default, Clone)]
struct Z3PathOpts {
    root: Option<PathBuf>,
    include_dir: Option<PathBuf>,
    lib_dir: Option<PathBuf>,
}

#[derive(Debug, Default, Clone)]
struct Z3Paths {
    include_dir: Option<PathBuf>,
    lib_dir: Option<PathBuf>,
}

fn z3_include_dir(dir: &Path) -> Option<PathBuf> {
    dir.join("z3++.h").exists().then(|| dir.to_path_buf())
}

fn z3_lib_dir(dir: &Path) -> Option<PathBuf> {
    ["libz3.so", "libz3.dylib", "libz3.a"]
        .iter()
        .any(|name| dir.join(name).exists())
        .then(|| dir.to_path_buf())
}

fn z3_root_dirs(root: &Path) -> Z3Paths {
    Z3Paths {
        include_dir: z3_include_dir(&root.join("include")),
        lib_dir: z3_lib_dir(&root.join("lib")).or_else(|| z3_lib_dir(&root.join("lib64"))),
    }
}

fn resolve_z3_paths(opts: &Z3PathOpts) -> Z3Paths {
    let env_root = std::env::var_os("HARC_Z3_ROOT").map(PathBuf::from);
    let env_include = std::env::var_os("HARC_Z3_INCLUDE_DIR").map(PathBuf::from);
    let env_lib = std::env::var_os("HARC_Z3_LIB_DIR").map(PathBuf::from);
    resolve_z3_paths_with(
        opts,
        env_root.as_deref(),
        env_include.as_deref(),
        env_lib.as_deref(),
        Path::new("."),
    )
}

fn resolve_z3_paths_with(
    opts: &Z3PathOpts,
    env_root: Option<&Path>,
    env_include: Option<&Path>,
    env_lib: Option<&Path>,
    repo_root: &Path,
) -> Z3Paths {
    let mut out = Z3Paths::default();
    let mut include_locked = false;
    let mut lib_locked = false;

    let apply_explicit = |inc: Option<&Path>,
                          lib: Option<&Path>,
                          out: &mut Z3Paths,
                          include_locked: &mut bool,
                          lib_locked: &mut bool| {
        if !*include_locked {
            if let Some(inc) = inc {
                out.include_dir = z3_include_dir(inc);
                *include_locked = true;
            }
        }
        if !*lib_locked {
            if let Some(lib) = lib {
                out.lib_dir = z3_lib_dir(lib);
                *lib_locked = true;
            }
        }
    };

    apply_explicit(
        opts.include_dir.as_deref(),
        opts.lib_dir.as_deref(),
        &mut out,
        &mut include_locked,
        &mut lib_locked,
    );
    apply_explicit(
        env_include,
        env_lib,
        &mut out,
        &mut include_locked,
        &mut lib_locked,
    );

    let local_root = repo_root.join("third_party/z3");
    for root in [opts.root.as_deref(), env_root, Some(local_root.as_path())]
        .into_iter()
        .flatten()
    {
        let candidate = z3_root_dirs(root);
        if !include_locked && out.include_dir.is_none() {
            out.include_dir = candidate.include_dir;
        }
        if !lib_locked && out.lib_dir.is_none() {
            out.lib_dir = candidate.lib_dir;
        }
    }

    for (inc, lib) in [
        (
            Path::new("/opt/homebrew/include"),
            Path::new("/opt/homebrew/lib"),
        ),
        (Path::new("/usr/local/include"), Path::new("/usr/local/lib")),
        (Path::new("/usr/include"), Path::new("/usr/lib")),
        (
            Path::new("/usr/include"),
            Path::new("/usr/lib/x86_64-linux-gnu"),
        ),
        (
            Path::new("/usr/include"),
            Path::new("/usr/lib/aarch64-linux-gnu"),
        ),
    ] {
        if !include_locked && out.include_dir.is_none() {
            out.include_dir = z3_include_dir(inc);
        }
        if !lib_locked && out.lib_dir.is_none() {
            out.lib_dir = z3_lib_dir(lib);
        }
    }

    out
}

fn prepend_env_path(cmd: &mut Command, name: &str, path: &Path) -> Result<()> {
    let mut paths = vec![path.to_path_buf()];
    if let Some(existing) = std::env::var_os(name) {
        paths.extend(std::env::split_paths(&existing));
    }
    let joined = std::env::join_paths(paths).into_diagnostic()?;
    cmd.env(name, joined);
    Ok(())
}

fn ensure_z3_for_solver(paths: &Z3Paths) -> Result<()> {
    if paths.include_dir.is_some() && paths.lib_dir.is_some() {
        return Ok(());
    }
    let missing = match (&paths.include_dir, &paths.lib_dir) {
        (None, None) => "include and library directories",
        (None, Some(_)) => "include directory",
        (Some(_), None) => "library directory",
        (Some(_), Some(_)) => unreachable!(),
    };
    Err(miette::miette!(
        "Z3 is required for this test's constraint-randomization, but the Z3 {missing} could not be resolved. Set HARC_Z3_ROOT=/path/to/z3, pass --z3-root /path/to/z3, or pass --z3-include-dir and --z3-lib-dir explicitly."
    ))
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
                        let _ = harc::learn::harvest_features(&ast, |_item| file_path.clone());
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
        std::io::stdin()
            .read_to_string(&mut buf)
            .into_diagnostic()?;
        if !buf.trim().is_empty() {
            if !q.is_empty() {
                q.push(' ');
            }
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
        matches
            .into_iter()
            .filter(|m| m.event.kind == "feature")
            .take(top)
            .collect()
    } else {
        matches
            .into_iter()
            .filter(|m| m.event.kind != "feature")
            .take(top)
            .collect()
    };
    if matches.is_empty() {
        eprintln!("No matches.");
        return Ok(());
    }
    for (i, m) in matches.iter().enumerate() {
        println!(
            "── match #{} (score {:.3}, retrieved {}×) ──────────────────────",
            i + 1,
            m.score,
            m.retrieved_count
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
            if !p.is_empty() {
                search.push(PathBuf::from(p));
            }
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
    let mut already: std::collections::HashSet<String> = files
        .iter()
        .flat_map(|f| {
            f.items.iter().filter_map(|it| match it {
                Item::Bus(b) => Some(b.name.name.clone()),
                _ => None,
            })
        })
        .collect();

    for name in &wanted {
        if already.contains(name) {
            continue;
        }
        let mut found_path: Option<PathBuf> = None;
        for dir in &search {
            for ext in &["arch", "harc"] {
                let candidate = dir.join(format!("{name}.{ext}"));
                if candidate.exists() {
                    found_path = Some(candidate);
                    break;
                }
            }
            if found_path.is_some() {
                break;
            }
        }
        let Some(path) = found_path else {
            continue;
        };

        let src = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let parsed = match harc::parser::parse_source(&src) {
            Ok(p) => p,
            Err(_) => continue, // skip files we can't parse
        };
        let bus_only: Vec<Item> = parsed
            .items
            .into_iter()
            .filter(|it| matches!(it, Item::Bus(_)))
            .collect();
        if !bus_only.is_empty() {
            for it in &bus_only {
                if let Item::Bus(b) = it {
                    already.insert(b.name.name.clone());
                }
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
        Report::new(e).with_source_code(NamedSource::new(path.display().to_string(), src))
    })
}

fn parse_file_source(path: &PathBuf, src: &str) -> Result<harc::ast::SourceFile> {
    harc::parser::parse_source(src).map_err(|e| {
        Report::new(e).with_source_code(NamedSource::new(
            path.display().to_string(),
            src.to_string(),
        ))
    })
}

fn width_literal_value(kind: &harc::lexer::TokenKind) -> Option<u32> {
    match kind {
        harc::lexer::TokenKind::DecLiteral(s) => s.replace('_', "").parse::<u32>().ok(),
        harc::lexer::TokenKind::HexLiteral(s) => u32::from_str_radix(
            s.trim_start_matches("0x")
                .trim_start_matches("0X")
                .replace('_', "")
                .as_str(),
            16,
        )
        .ok(),
        harc::lexer::TokenKind::BinLiteral(s) => u32::from_str_radix(
            s.trim_start_matches("0b")
                .trim_start_matches("0B")
                .replace('_', "")
                .as_str(),
            2,
        )
        .ok(),
        _ => None,
    }
}

fn is_backend_width_method(name: &str) -> bool {
    matches!(name, "trunc" | "zext" | "sext" | "resize")
}

fn validate_check_backend_codegen_limitations(path: &PathBuf, src: &str) -> Result<()> {
    let tokens = match harc::lexer::tokenize(src) {
        Ok(tokens) => tokens,
        Err(_) => return Ok(()),
    };
    for window_start in 0..tokens.len().saturating_sub(4) {
        if tokens[window_start].kind != harc::lexer::TokenKind::Dot {
            continue;
        }
        let harc::lexer::TokenKind::Ident(method) = &tokens[window_start + 1].kind else {
            continue;
        };
        if !is_backend_width_method(method)
            || tokens[window_start + 2].kind != harc::lexer::TokenKind::Lt
        {
            continue;
        }

        let Some(close_idx) = tokens
            .iter()
            .enumerate()
            .skip(window_start + 3)
            .find_map(|(idx, tok)| (tok.kind == harc::lexer::TokenKind::Gt).then_some(idx))
        else {
            continue;
        };
        let width_tokens = &tokens[window_start + 3..close_idx];
        let width = if width_tokens.len() == 1 {
            width_literal_value(&width_tokens[0].kind)
        } else {
            None
        };
        let Some(width) = width else {
            let span = tokens[window_start + 1].span.merge(tokens[close_idx].span);
            let err = harc::diagnostics::CompileError::unsupported_syntax(
                &format!("C++ backend cannot lower `.{method}<N>()` with a non-constant width"),
                "supported width-method forms for `harc check` and C++ codegen use a literal width in 1..=64",
                span,
            );
            return Err(Report::new(err).with_source_code(NamedSource::new(
                path.display().to_string(),
                src.to_string(),
            )));
        };
        if width == 0 || width > 64 {
            let span = tokens[window_start + 1].span.merge(tokens[close_idx].span);
            let err = harc::diagnostics::CompileError::unsupported_syntax(
                &format!("C++ backend cannot lower `.{method}<{width}>()`"),
                "supported resize widths for this backend are 1..=64; split the value into <=64-bit limbs or use an extern helper",
                span,
            );
            return Err(Report::new(err).with_source_code(NamedSource::new(
                path.display().to_string(),
                src.to_string(),
            )));
        }
    }
    Ok(())
}

fn cmd_check(files: Vec<PathBuf>, ast: bool) -> Result<()> {
    let mut total_items = 0;
    for file in &files {
        let src = fs::read_to_string(file).into_diagnostic()?;
        let parsed = parse_file_source(file, &src)?;
        validate_check_backend_codegen_limitations(file, &src)?;
        total_items += parsed.items.len();
        if ast {
            println!("// {}", file.display());
            println!("{:#?}", parsed);
        }
    }
    if !ast {
        println!(
            "ok: {} file(s), {} top-level item(s)",
            files.len(),
            total_items
        );
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
/// Write `contents` to `path` ONLY if the existing file differs (by
/// byte-for-byte compare). Preserves mtime when the content is
/// unchanged, which is the structural prerequisite for Make's
/// incremental-rebuild path: an unchanged .cpp keeps its old mtime,
/// so the .o stays valid and Verilator's Make skips the recompile.
/// Returns `Ok(true)` when a write happened, `Ok(false)` when the
/// file already matched.
fn write_if_changed(path: &Path, contents: &[u8]) -> Result<bool> {
    if let Ok(existing) = fs::read(path) {
        if existing == contents {
            return Ok(false);
        }
    }
    fs::write(path, contents).into_diagnostic()?;
    Ok(true)
}

fn emit_probe_stub_if_needed(
    outdir: &Path,
    file: &harc::ast::SourceFile,
) -> Result<Option<PathBuf>> {
    let Some((dut_ty, probes)) = harc::codegen::cpp_tb::dut_probes(file) else {
        return Ok(None);
    };
    let stub_src = harc::codegen::sv_stub::emit_stub(&dut_ty, &probes)
        .map_err(|e| miette::miette!("probe stub emit failed: {e}"))?;
    let stub_path = outdir.join(format!("__harc_probe_{dut_ty}.sv"));
    if write_if_changed(&stub_path, stub_src.as_bytes())? {
        eprintln!("emitted {}", stub_path.display());
    } else {
        eprintln!("reused {} (unchanged)", stub_path.display());
    }
    Ok(Some(stub_path))
}

fn absolutize_trace_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir().into_diagnostic()?.join(path))
}

fn run_verilator(
    top: &str,
    sv: &[PathBuf],
    vlt: &[PathBuf],
    cpp: &PathBuf,
    outdir_abs: &PathBuf,
    sim_log_path: &PathBuf,
    seed: Option<u64>,
    coverage: bool,
    ref_src: &[PathBuf],
    z3_paths: &Z3Paths,
    test: Option<&str>,
    rebuild: bool,
    record_trace: Option<&PathBuf>,
    waves: &WaveOpts,
) -> Result<()> {
    let mdir = outdir_abs.join("obj_dir");
    // Build-reuse path (Phase 1c). When `--rebuild` is unset and the
    // emitted .cpp is byte-identical to the previous run's, Make's
    // mtime-based skip kicks in and Verilator finishes in ~0.1s
    // instead of ~5-10s. `--rebuild` (or a deleted outdir) forces
    // a fresh build — useful when Verilator was upgraded or when
    // verilator flags changed in a way the emitted .cpp doesn't
    // capture.
    if rebuild {
        let _ = fs::remove_dir_all(&mdir);
    }
    fs::create_dir_all(&mdir).into_diagnostic()?;

    let mut args: Vec<String> = vec![
        "--cc".into(),
        "--exe".into(),
        "--build".into(),
        "-Wno-fatal".into(),
        "-Wno-WIDTH".into(),
        // Tolerate SV quirks Xcelium accepts but Verilator escalates:
        //   BLKANDNBLK — same variable written by `=` in one block
        //                and `<=` in another. Common in CVDP DUTs
        //                (e.g. resets via `=` + clocked updates via
        //                `<=` on the same reg).
        //   UNOPTFLAT  — combinational signal in an `always_comb`
        //                that Verilator's optimizer flags as
        //                potentially looped (false positive on most
        //                CVDP DUTs).
        "-Wno-BLKANDNBLK".into(),
        "-Wno-UNOPTFLAT".into(),
        // Cycle-based TBs don't need delay semantics; tell Verilator
        // to elide `#N` delay statements rather than refusing to
        // elaborate. HARC's `wait N cycles` is always cycle-based
        // (handled by the runtime scheduler) — delays inside a DUT
        // are a property of the DUT author, not the TB, and CVDP
        // coverage scoring ignores delay semantics too.
        "--no-timing".into(),
        "--top-module".into(),
        top.into(),
        "--Mdir".into(),
        mdir.display().to_string(),
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
    // Waveform support (issue #209). When `--waves` is set we ask
    // Verilator to compile in trace support for the requested format
    // and we activate the trace codegen in the emitted TB via a
    // -D macro. The emitted .cpp itself stays byte-identical across
    // trace/no-trace builds (the scaffolding is always there, gated
    // by `#if defined(HARC_TRACE_*)`), so the rebuild-skip heuristic
    // in cmd_sim still works for the no-trace → no-trace case. When
    // *flipping* trace on/off, users should pass `--rebuild` because
    // Verilator's cached object files were compiled without the
    // trace defines and would silently link against the new .cpp.
    if waves.waves {
        let trace_flag = match waves.format.as_str() {
            "vcd" => "--trace-vcd",
            // Default plus the explicit `fst` selector.
            _ => "--trace-fst",
        };
        args.push(trace_flag.into());
        if waves.trace_structs {
            args.push("--trace-structs".into());
        }
        args.push("--trace-max-width".into());
        args.push(waves.trace_max_width.to_string());
        if let Some(max_array) = waves.trace_max_array {
            args.push("--trace-max-array".into());
            args.push(max_array.to_string());
        }
        let define_macro = match waves.format.as_str() {
            "vcd" => "-DHARC_TRACE_VCD",
            _ => "-DHARC_TRACE_FST",
        };
        args.push("-CFLAGS".into());
        args.push(define_macro.into());
    }
    // User-supplied Verilator flags (`--verilator-arg`). Appended
    // after HARC defaults but before SV inputs so the user can
    // override warnings, add `--public-flat-rw`, etc.
    for extra in &waves.verilator_args {
        args.push(extra.clone());
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
    if let Some(inc) = &z3_paths.include_dir {
        args.push("-CFLAGS".into());
        args.push(format!("-I{}", inc.display()));
    }
    if let Some(lib) = &z3_paths.lib_dir {
        args.push("-LDFLAGS".into());
        args.push(format!(
            "-L{} -Wl,-rpath,{} -lz3",
            lib.display(),
            lib.display()
        ));
    }
    for control in vlt {
        args.push(control.display().to_string());
    }
    for s in sv {
        args.push(s.display().to_string());
    }
    // Reference-model sources (spec §9 `extern function`). Passed
    // verbatim to verilator alongside the emitted TB so the C linker
    // can resolve `extern "C"` forward declarations. Verilator's
    // `--cc --exe --build` flow accepts arbitrary `.c` / `.cpp` files
    // on the command line — they compile + link with the same flags
    // as the TB.
    for r in ref_src {
        args.push(r.display().to_string());
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
        return Err(miette::miette!(
            "verilator build failed (status {})",
            output.status
        ));
    }

    let bin = mdir.join(format!("V{top}"));
    eprintln!("running: {}", bin.display());
    let mut cmd = Command::new(&bin);
    cmd.env("HARC_SIM_LOG", sim_log_path)
        .env("HARC_LOG_DIR", outdir_abs)
        .env("HARC_DUT_BACKEND", "sv");
    if let Some(path) = record_trace {
        cmd.env("HARC_TRACE", path);
    }
    if let Some(s) = seed {
        cmd.env("HARC_SEED", s.to_string());
    }
    // Per-test selection at runtime (Phase 1b of
    // docs/separate-compilation-plan.md). The binary now contains
    // every test in the source as a separate `run_<TestName>`
    // function; the dispatcher `main()` picks one based on this
    // flag (or the `HARC_TEST` env var). When unset, the dispatcher
    // runs the alphabetically-first test.
    if let Some(t) = test {
        cmd.args(&["--test", t]);
    }
    // Waveform runtime config. `HARC_WAVE_FILE` overrides the
    // emitted default (`<HARC_LOG_DIR>/waves.<ext>`); the emitted
    // TB picks the format from the `HARC_TRACE_*` compile-time
    // macro, so we only need to forward the path + depth.
    if waves.waves {
        let ext = if waves.format == "vcd" { "vcd" } else { "fst" };
        let wave_path = match &waves.file {
            Some(p) => {
                if p.is_absolute() {
                    p.clone()
                } else {
                    std::env::current_dir().into_diagnostic()?.join(p)
                }
            }
            None => {
                let stem = test.unwrap_or("waves");
                outdir_abs.join(format!("{stem}.{ext}"))
            }
        };
        if let Some(parent) = wave_path.parent() {
            fs::create_dir_all(parent).into_diagnostic()?;
        }
        cmd.env("HARC_WAVE_FILE", &wave_path);
        cmd.env("HARC_TRACE_DEPTH", waves.trace_depth.to_string());
        eprintln!("waveform output: {}", wave_path.display());
    }
    // `--sim-arg` pass-through. Forwarded verbatim after `--test`
    // so plusargs land in argv unmodified.
    for extra in &waves.sim_args {
        cmd.arg(extra);
    }
    if let Some(lib) = &z3_paths.lib_dir {
        prepend_env_path(&mut cmd, "LD_LIBRARY_PATH", lib)?;
        prepend_env_path(&mut cmd, "DYLD_LIBRARY_PATH", lib)?;
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
    vlt: Vec<PathBuf>,
    top: Option<String>,
    test: Option<String>,
    outdir: Option<PathBuf>,
    seed: Option<u64>,
    emit_only: bool,
    arch_bin: Option<PathBuf>,
    mt: bool,
    coverage: bool,
    ref_src: Vec<PathBuf>,
    z3_opts: Z3PathOpts,
    rebuild: bool,
    record_trace: Option<PathBuf>,
    waves: WaveOpts,
) -> Result<()> {
    if dut.is_empty() && sv.is_empty() {
        return Err(miette::miette!(
            "pass either --dut <file.arch> or --sv <file.sv>"
        ));
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

    let cpp =
        harc::codegen::cpp_tb::emit_with_opts(&merged, harc::codegen::cpp_tb::EmitOpts { mt })
            .map_err(|e| miette::miette!("{}", e))?;
    let uses_solver = harc::codegen::cpp_tb::uses_constraint_solver(&merged);
    let z3_paths = resolve_z3_paths(&z3_opts);

    let outdir = outdir.unwrap_or_else(|| PathBuf::from("harc_sim_build"));
    fs::create_dir_all(&outdir).into_diagnostic()?;
    let stem = files[0]
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("harc_tb");
    let cpp_path = outdir.join(format!("{stem}.cpp"));
    // Only rewrite the .cpp when its content actually changed. Phase
    // 1c: keeps mtime stable so Verilator's Make skips the rebuild
    // when the same source is re-emitted with a different `--test`
    // selection (the dispatcher's branch list is what changes; the
    // emitted code is byte-identical).
    let cpp_changed = write_if_changed(&cpp_path, cpp.as_bytes())?;
    if cpp_changed {
        eprintln!("emitted {}", cpp_path.display());
    } else {
        eprintln!("reused {} (unchanged)", cpp_path.display());
    }

    // Drop bundled runtime headers alongside the emitted .cpp so
    // verilator's standard `--Mdir`-relative include search picks it up
    // without needing an extra `-I` flag. The .cpp file `#include`s
    // them by basename. Bundled as baked-in strings via
    // `include_str!` so a binary install of `harc` ships the runtime
    // without a separate file dependency.
    let rt_header_path = outdir.join("harc_thread_rt.h");
    write_if_changed(
        &rt_header_path,
        harc::codegen::cpp_tb::THREAD_RT_HEADER.as_bytes(),
    )?;
    let random_rt_header_path = outdir.join("harc_random_rt.h");
    write_if_changed(
        &random_rt_header_path,
        harc::codegen::cpp_tb::RANDOM_RT_HEADER.as_bytes(),
    )?;
    let queue_rt_header_path = outdir.join("harc_queue_rt.h");
    write_if_changed(
        &queue_rt_header_path,
        harc::codegen::cpp_tb::QUEUE_RT_HEADER.as_bytes(),
    )?;
    let trace_rt_header_path = outdir.join("harc_trace_rt.h");
    write_if_changed(
        &trace_rt_header_path,
        harc::codegen::cpp_tb::TRACE_RT_HEADER.as_bytes(),
    )?;
    let log_rt_header_path = outdir.join("harc_log_rt.h");
    write_if_changed(
        &log_rt_header_path,
        harc::codegen::cpp_tb::LOG_RT_HEADER.as_bytes(),
    )?;
    let z3_rt_header_path = outdir.join("harc_z3_rt.h");
    write_if_changed(
        &z3_rt_header_path,
        harc::codegen::cpp_tb::Z3_RT_HEADER.as_bytes(),
    )?;

    // `--emit-only` must still emit every generated source artifact a
    // downstream Verilator build needs, including probe bind stubs.
    let probe_stub_path = emit_probe_stub_if_needed(&outdir, &merged)?;

    if emit_only {
        return Ok(());
    }

    let cpp_abs = fs::canonicalize(&cpp_path).into_diagnostic()?;
    let outdir_abs = fs::canonicalize(&outdir).into_diagnostic()?;
    let sim_log_path = outdir_abs.join("sim.log");
    let trace_abs = record_trace
        .as_ref()
        .map(|p| absolutize_trace_path(p))
        .transpose()?;

    if !sv.is_empty() {
        if uses_solver {
            ensure_z3_for_solver(&z3_paths)?;
        }
        // SV / Verilator path — no `arch sim` involvement. Resolves the top
        // module name from `--top` if given, else from the HARC `let dut : T`.
        let top_name = top
            .or_else(|| harc::codegen::cpp_tb::dut_type_name(&merged))
            .ok_or_else(|| {
                miette::miette!(
                    "could not determine SV top module — pass --top or declare `let dut : T`"
                )
            })?;
        let mut sv_abs = Vec::with_capacity(sv.len() + 1);
        for s in &sv {
            sv_abs.push(fs::canonicalize(s).into_diagnostic()?);
        }
        let mut vlt_abs = Vec::with_capacity(vlt.len());
        for control in &vlt {
            vlt_abs.push(fs::canonicalize(control).into_diagnostic()?);
        }

        // If the test's `let dut : T` carries `probe` declarations,
        // prepend the generated SV bind stub to the verilator inputs.
        // Probe-less tests skip the stub and public-flat-rd machinery.
        if let Some(stub_path) = &probe_stub_path {
            sv_abs.push(fs::canonicalize(stub_path).into_diagnostic()?);
        }

        // Canonicalize ref-src paths so verilator (running in obj_dir/)
        // can still find them. Missing files surface as a clear
        // canonicalize error before the verilator command runs,
        // which beats a "no such file" deep in the build log.
        let mut ref_src_abs = Vec::with_capacity(ref_src.len());
        for r in &ref_src {
            ref_src_abs.push(fs::canonicalize(r).into_diagnostic()?);
        }
        return run_verilator(
            &top_name,
            &sv_abs,
            &vlt_abs,
            &cpp_abs,
            &outdir_abs,
            &sim_log_path,
            seed,
            coverage,
            &ref_src_abs,
            &z3_paths,
            test.as_deref(),
            rebuild,
            trace_abs.as_ref(),
            &waves,
        );
    }

    // ARCH path: run `arch sim <dut...> --tb <cpp_path>`.
    let mut dut_abs = Vec::with_capacity(dut.len());
    for d in &dut {
        dut_abs.push(fs::canonicalize(d).into_diagnostic()?);
    }

    let (program, mut prefix_args, working_dir) = match &arch_bin {
        Some(p) => (
            p.clone(),
            Vec::<String>::new(),
            std::env::current_dir().into_diagnostic()?,
        ),
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
    // Coverage passthrough: `arch sim` supports both `--coverage`
    // (dumps `coverage.txt` keyed to .arch source lines) and
    // `--coverage-dat=<path>` (Verilator-compatible `coverage.dat`).
    // Pin the .dat output to `<outdir>/coverage.dat` so it lands
    // next to sim.log — same location the `--sv` Verilator path
    // writes its coverage.dat. Without this, --coverage on --dut
    // was silently a no-op (the existing `coverage: bool` was
    // consumed only inside run_verilator()).
    if coverage {
        prefix_args.push("--coverage".into());
        let cov_dat_path = outdir_abs.join("coverage.dat");
        prefix_args.push(format!("--coverage-dat={}", cov_dat_path.display()));
    }

    eprintln!("running: {} {}", program.display(), prefix_args.join(" "));
    let mut cmd = Command::new(&program);
    cmd.args(&prefix_args)
        .current_dir(&working_dir)
        .env("HARC_SIM_LOG", &sim_log_path)
        // Anchor relative `logf("foo.log", ...)` paths to the build dir so
        // per-component log files land next to sim.log instead of under
        // arch-com/ (where the binary actually runs from).
        .env("HARC_LOG_DIR", &outdir_abs)
        .env("HARC_DUT_BACKEND", "arch");
    if let Some(path) = &trace_abs {
        cmd.env("HARC_TRACE", path);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "harc-z3-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_include(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("z3++.h"), "").unwrap();
    }

    fn make_lib(dir: &Path, name: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(name), "").unwrap();
    }

    fn make_root(root: &Path, lib_subdir: &str, lib_name: &str) {
        make_include(&root.join("include"));
        make_lib(&root.join(lib_subdir), lib_name);
    }

    #[test]
    fn z3_resolver_prefers_cli_explicit_over_env() {
        let base = temp_dir("explicit");
        let cli_inc = base.join("cli/include");
        let cli_lib = base.join("cli/lib");
        let env_inc = base.join("env/include");
        let env_lib = base.join("env/lib");
        make_include(&cli_inc);
        make_lib(&cli_lib, "libz3.so");
        make_include(&env_inc);
        make_lib(&env_lib, "libz3.dylib");

        let paths = resolve_z3_paths_with(
            &Z3PathOpts {
                root: None,
                include_dir: Some(cli_inc.clone()),
                lib_dir: Some(cli_lib.clone()),
            },
            None,
            Some(&env_inc),
            Some(&env_lib),
            &base,
        );

        assert_eq!(paths.include_dir.as_deref(), Some(cli_inc.as_path()));
        assert_eq!(paths.lib_dir.as_deref(), Some(cli_lib.as_path()));
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn z3_resolver_uses_env_explicit_before_cli_root() {
        let base = temp_dir("env-before-root");
        let env_inc = base.join("env/include");
        let env_lib = base.join("env/lib");
        let cli_root = base.join("cli-root");
        make_include(&env_inc);
        make_lib(&env_lib, "libz3.a");
        make_root(&cli_root, "lib", "libz3.so");

        let paths = resolve_z3_paths_with(
            &Z3PathOpts {
                root: Some(cli_root),
                include_dir: None,
                lib_dir: None,
            },
            None,
            Some(&env_inc),
            Some(&env_lib),
            &base,
        );

        assert_eq!(paths.include_dir.as_deref(), Some(env_inc.as_path()));
        assert_eq!(paths.lib_dir.as_deref(), Some(env_lib.as_path()));
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn z3_resolver_checks_root_lib64_and_repo_local() {
        let base = temp_dir("root-lib64");
        let env_root = base.join("env-root");
        let local_root = base.join("third_party/z3");
        make_root(&env_root, "lib64", "libz3.so");
        make_root(&local_root, "lib", "libz3.dylib");

        let paths =
            resolve_z3_paths_with(&Z3PathOpts::default(), Some(&env_root), None, None, &base);

        assert_eq!(
            paths.include_dir.as_deref(),
            Some(env_root.join("include").as_path())
        );
        assert_eq!(
            paths.lib_dir.as_deref(),
            Some(env_root.join("lib64").as_path())
        );

        let local_paths = resolve_z3_paths_with(&Z3PathOpts::default(), None, None, None, &base);
        assert_eq!(
            local_paths.include_dir.as_deref(),
            Some(local_root.join("include").as_path())
        );
        assert_eq!(
            local_paths.lib_dir.as_deref(),
            Some(local_root.join("lib").as_path())
        );
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn z3_resolver_bad_explicit_does_not_fall_back() {
        let base = temp_dir("bad-explicit");
        let local_root = base.join("third_party/z3");
        make_root(&local_root, "lib", "libz3.so");

        let paths = resolve_z3_paths_with(
            &Z3PathOpts {
                root: None,
                include_dir: Some(base.join("missing-include")),
                lib_dir: Some(base.join("missing-lib")),
            },
            None,
            None,
            None,
            &base,
        );

        assert!(paths.include_dir.is_none());
        assert!(paths.lib_dir.is_none());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn detects_constraint_solver_for_keep_backed_randomize() {
        let src = r#"
            transaction Req
                len : uint<8>
                keep len in [1..16]
            end transaction Req

            test T
                let dut : Top

                run
                    let t : Req
                    randomize(t)
                end run
            end test
        "#;
        let file = harc::parser::parse_source(src).unwrap();
        assert!(harc::codegen::cpp_tb::uses_constraint_solver(&file));
    }

    #[test]
    fn emit_probe_stub_helper_writes_emit_only_artifact() {
        let src = r#"
            testbench ProbeDutTb
                let dut : CpuPipe
                    probe alu_a : uint<32> at alu0.a
                end let dut
            end testbench ProbeDutTb

            impl ProbeDutTest for ProbeDutTb
                run
                    assert dut.alu_a == 0
                end run
            end impl ProbeDutTest
        "#;
        let file = harc::parser::parse_source(src).unwrap();
        let base = temp_dir("probe-stub");
        let stub = emit_probe_stub_if_needed(&base, &file)
            .unwrap()
            .expect("probe-bearing source should produce a stub path");

        assert_eq!(
            stub.file_name().and_then(|s| s.to_str()),
            Some("__harc_probe_CpuPipe.sv")
        );
        let contents = fs::read_to_string(&stub).unwrap();
        assert!(contents.contains("bind CpuPipe __harc_probe_CpuPipe harc_probes ();"));
        assert!(contents.contains("assign alu_a = CpuPipe.alu0.a;"));
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn check_reports_backend_unsupported_wide_resize() {
        let src = r#"
            function wide_zext_128_repro(a: uint<48>, b: uint<16>) -> uint<64>
                let product : uint<128> = a.zext<128>() * b.zext<128>()
                return product.trunc<64>()
            end function wide_zext_128_repro
        "#;
        let path = PathBuf::from("wide_zext_128_repro.harc");
        let err = validate_check_backend_codegen_limitations(&path, src).unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("C++ backend cannot lower `.zext<128>()`")
                && msg.contains("supported resize widths"),
            "expected backend unsupported-width diagnostic; got:\n{msg}"
        );
    }

    #[test]
    fn check_accepts_backend_supported_resize_widths() {
        let src = r#"
            function narrow_ok(a: uint<48>, b: uint<16>) -> uint<64>
                let product : uint<64> = a.zext<64>() * b.zext<64>()
                return product.trunc<32>().zext<64>()
            end function narrow_ok
        "#;
        let path = PathBuf::from("narrow_ok.harc");
        validate_check_backend_codegen_limitations(&path, src).unwrap();
    }
}
