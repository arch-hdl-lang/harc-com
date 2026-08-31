use clap::{Parser, Subcommand, ValueEnum};
use miette::{IntoDiagnostic, NamedSource, Report, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::time::Instant;

mod trace_merge;

#[derive(Parser, Debug)]
#[command(name = "harc", version, about = "HARC verification language compiler")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
enum CompileScope {
    /// Emit all tests into the generated build artifact.
    #[default]
    Suite,
    /// Require `--test` and emit only that selected test.
    Test,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
enum CodegenKind {
    /// Legacy direct AST → C++ emitter (`codegen/cpp_tb.rs`). Escape hatch
    /// kept reachable via `--codegen v1` while the TB-IR pipeline soaks as
    /// the default (phase 5); slated for removal in phase 6.
    V1,
    /// Typed TB-IR pipeline: AST → IR (lower + verify) → C++ loop-switch
    /// emitter (`src/ir/` + `codegen/tbir/`). The default backend: it
    /// covers the full equivalence-proven fixture corpus
    /// (`tests/tbir_equiv_fixtures.txt`), trace-identical to v1. A
    /// construct outside its subset fails with a structured error naming
    /// the construct, and suggests `--codegen v1` only when v1 actually
    /// implements it — a construct no backend implements says so instead,
    /// rather than sending you to a dead end.
    #[default]
    Tbir,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
enum CppSplit {
    /// Preserve the current single generated C++ translation unit.
    #[default]
    Off,
    /// Emit one dispatcher C++ file plus one C++ translation unit per test.
    Tests,
}

/// Layout for generated C++ split shards (`--cpp-split-layout`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
enum CppSplitLayout {
    /// Historical behavior: every shard is a self-contained translation
    /// unit (shared scaffolding re-emitted per shard, internal linkage).
    #[default]
    SelfContained,
    /// Common-object layout: reusable infra compiled once into common
    /// objects; one stable capsule per test; explicit registry +
    /// dispatcher; interface-ABI link anchor (issue #643).
    Common,
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
    /// Print the exact native C++ source list owned by a trusted
    /// common-object manifest, one path per line.
    ManifestSources {
        /// Generated `*artifacts.json` manifest.
        manifest: PathBuf,
        /// Print the manifest-owned SystemVerilog probe stub instead.
        #[arg(long)]
        probe_stub: bool,
        /// Print every manifest-owned build input, including generated
        /// headers and the probe stub, instead of only native C++ sources.
        #[arg(long, conflicts_with = "probe_stub")]
        all_artifacts: bool,
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
        /// shared bus definitions. Conflicts with `--sv` unless
        /// `--check-backends` is set (in which case both are required).
        #[arg(long)]
        dut: Vec<PathBuf>,
        /// SystemVerilog DUT source file(s). Drives Verilator directly,
        /// bypassing `arch sim`. Conflicts with `--dut` unless
        /// `--check-backends` is set (in which case both are required).
        #[arg(long)]
        sv: Vec<PathBuf>,
        /// Verilator control file(s), typically `.vlt` waivers or coverage
        /// controls. Forwarded to Verilator before the SV DUT files.
        #[arg(long)]
        vlt: Vec<PathBuf>,
        /// DUT parameter override(s), repeatable as `--param NAME=VALUE`.
        /// Lowered to Verilator `-GNAME=VALUE` on `--sv` and forwarded as
        /// `arch sim --param NAME=VALUE` on `--dut`.
        #[arg(long = "param")]
        params: Vec<String>,
        /// SV top-module name (Verilator `--top-module`). Defaults to the
        /// type of `let dut : <Type>` in the HARC source.
        #[arg(long)]
        top: Option<String>,
        /// Pick a specific test by name (when input contains more than one).
        #[arg(long)]
        test: Option<String>,
        /// Codegen scope. `suite` keeps the current build-once-run-many
        /// binary; `test` requires `--test` and compiles only that test.
        #[arg(long, value_enum, default_value_t = CompileScope::Suite)]
        compile_scope: CompileScope,
        /// C++ emitter selection. Defaults to `tbir` for normal sim/build
        /// and to `v1` for `--check-backends` until that mode supports
        /// TB-IR. `v1` remains the legacy direct AST → C++ escape hatch.
        /// `tbir` covers the MVP statement subset and rejects everything
        /// else with a structured error. Not combinable with `--mt`,
        /// `--cpp-split tests`, or `--check-backends`.
        #[arg(long, value_enum)]
        codegen: Option<CodegenKind>,
        /// Split generated C++ output. `tests` writes one dispatcher plus
        /// grouped C++ translation units for the tests so Verilator can
        /// compile generated HARC test objects independently.
        #[arg(long, value_enum, default_value_t = CppSplit::Off)]
        cpp_split: CppSplit,
        /// Number of tests per generated split C++ shard. Higher values
        /// reduce compiler startup/header parse overhead; lower values
        /// improve per-test incremental granularity. Ignored (rejected)
        /// under `--cpp-split-layout common`, which always emits one
        /// stable capsule per test.
        #[arg(long, default_value_t = 4)]
        cpp_split_group_size: usize,
        /// Generated C++ split layout for `--cpp-split tests`.
        /// `self-contained` keeps the historical
        /// per-shard self-contained translation units; `common` compiles
        /// the shared runtime/testbench/component infrastructure once
        /// and emits one small stable capsule per test plus an explicit
        /// registry (issue #643).
        #[arg(long, value_enum, default_value_t = CppSplitLayout::SelfContained)]
        cpp_split_layout: CppSplitLayout,
        /// Parallel workers for HARC's OWN split C++ emission (the
        /// frontend). `1` is the deterministic serial path; `0` picks
        /// `min(available CPUs, shard count, 4)`. The cap is a memory
        /// knob as much as a speed one — each in-flight shard holds a
        /// whole generated translation unit in memory.
        ///
        /// Distinct from `--jobs`, which controls only the downstream
        /// Verilator/native build. Applies to `--codegen tbir
        /// --cpp-split tests`; ignored elsewhere.
        #[arg(long, default_value_t = 0)]
        emit_jobs: usize,
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
        /// Additional native-build identity entry supplied by an external
        /// build driver. Repeatable; values are fingerprinted but do not
        /// otherwise affect code generation or runtime selection.
        #[arg(long)]
        build_profile_input: Vec<String>,
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
        /// the same source skips Verilator entirely. Native-build
        /// identity changes (including Verilator, flags, backend, and
        /// layout) invalidate the object directory automatically; use
        /// `--rebuild` to force the same cleanup while investigating a
        /// suspected stale-`.o` problem. See
        /// docs/separate-compilation-plan.md §1c.
        #[arg(long)]
        rebuild: bool,
        /// Record a semantic execution trace as JSONL. The generated
        /// testbench writes one metadata header followed by runtime
        /// events such as logs, failures, and randomization results.
        #[arg(long)]
        record_trace: Option<PathBuf>,
        /// Write non-lossy functional coverage JSONL from covergroup report()
        /// calls. The text report remains unchanged; this sidecar includes
        /// every coverpoint bin and every declared/auto cross bin without the
        /// stdout missing-bin cap.
        #[arg(long)]
        coverage_json: Option<PathBuf>,
        /// Enable Verilator VCD/FST waveform dumping. Implies trace
        /// codegen in the emitted C++ TB and `--trace` /
        /// `--trace-fst` on the Verilator command. Default format is
        /// FST (smaller + faster for large regressions); override
        /// with `--wave-format vcd`. Wave file lands in `<outdir>`
        /// unless `--wave-file` is given. Trace configuration changes
        /// invalidate incompatible cached native objects automatically.
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
        /// Simulator-owned-time co-simulation (spec §10). `dpi` lowers
        /// the TB into a passive DPI-C runtime: a generated
        /// `HarcCosimTop.sv` harness instantiates the DUT, owns the
        /// clock via a timed master process, and calls the TB through
        /// `harc_cosim_init` / `harc_cosim_step`; DUT port access
        /// crosses the boundary through generated typed accessors
        /// (scalar, 32-bit-word for wide ports, element for
        /// unpacked-array ports; probes route through the bound probe
        /// stub). Requires `--sv`. v0 limitations: no `--mt`, no
        /// `--waves`/`--coverage` (they belong to the simulator on
        /// this path), no `--param`, no split builds; probes and
        /// unpacked-array elements are limited to 64 bits. See
        /// docs/2026-07-24-dpi-cosim-exploration.md.
        #[arg(long, value_parser = ["dpi"])]
        cosim: Option<String>,
        /// Verilator/native build parallelism. Forwarded as `-j N`; use
        /// `0` to let Verilator choose based on available CPUs. Controls
        /// only the backend build — see `--emit-jobs` for HARC's own C++
        /// emission.
        #[arg(long)]
        jobs: Option<u32>,
        /// Additional argument for the generated simulation binary
        /// (e.g. `+plusarg=value`). Repeatable. Forwarded verbatim
        /// after the `--test` selector.
        #[arg(long = "sim-arg")]
        sim_args: Vec<String>,
        /// Run the test under BOTH backends (Verilator from `--sv` and
        /// ARCH native sim from `--dut`) with the same seed and diff
        /// their per-cycle semantic traces. Requires both `--dut` and
        /// `--sv` to be specified. Exits non-zero on any divergence.
        ///
        /// This is the regression net described in
        /// `docs/2026-05-28-backend-equivalence-gap.md`: catches the
        /// class of bug where one backend silently disagrees with the
        /// other (arch-com#437) before it reaches users.
        ///
        /// REQUIRES: backends must emit trace events in a deterministic,
        /// stable order. The diff compares line-by-line; any reordering
        /// across backends (even of semantically equivalent events on
        /// the same cycle) reports as divergence.
        #[arg(long)]
        check_backends: bool,
    },
    /// Parse + merge HARC source file(s), lower to the typed TB-IR,
    /// verify it, and print the textual IR form. Exits 1 with a
    /// structured error when a construct is outside the TB-IR subset.
    DumpIr {
        /// Input .harc file(s) — base test plus any extension files.
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// Run a TB-IR pass after lowering + verify and print its
        /// result after the regular IR dump. Available:
        /// `lower-coroutine` (CFG → tagged-FSM metadata),
        /// `placement` (per-block tier + timing class, capability-
        /// checked against `--profile`).
        #[arg(long)]
        pass: Option<String>,
        /// Target profile for `--pass placement`. Built-ins:
        /// `single-site` (default; the cpp_tb profile — never
        /// diagnoses) and `split-strict` (constrained demo profile
        /// that surfaces capability diagnostics).
        #[arg(long)]
        profile: Option<String>,
    },
    /// Diff two semantic JSONL traces (e.g. v1 vs tbir backends, or
    /// arch vs Verilator) after normalizing backend-specific noise.
    /// Prints divergences and exits 1 if any are found.
    TraceDiff {
        /// First trace (reported in the `arch:` column).
        a: PathBuf,
        /// Second trace (reported in the `sv:` column).
        b: PathBuf,
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
    /// Build and query a compiler-native JSONL code graph.
    Graph {
        #[command(subcommand)]
        cmd: GraphCmd,
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

#[derive(Subcommand, Debug)]
enum GraphCmd {
    /// Index .harc source and DUT files/directories into JSONL files.
    Index {
        /// Input .harc/.sv/.arch files or directories.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Output directory for files.jsonl, nodes.jsonl, and edges.jsonl.
        #[arg(long, default_value = ".harcgraph")]
        out: PathBuf,
    },
    /// Search graph nodes and edges for a symbol or text query.
    Query {
        /// Symbol or text query.
        query: String,
        /// Graph index directory.
        #[arg(long, default_value = ".harcgraph")]
        index: PathBuf,
        /// Maximum result lines.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// List tests that reference a DUT, type, or symbol.
    TestsFor {
        /// DUT/type/symbol name.
        symbol: String,
        /// Graph index directory.
        #[arg(long, default_value = ".harcgraph")]
        index: PathBuf,
        /// Maximum result lines.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Return a bounded dependency/impact slice around a symbol.
    Impact {
        /// Symbol name.
        symbol: String,
        /// Graph index directory.
        #[arg(long, default_value = ".harcgraph")]
        index: PathBuf,
        /// Edge traversal depth.
        #[arg(long, default_value_t = 2)]
        depth: usize,
        /// Maximum result lines.
        #[arg(long, default_value_t = 40)]
        limit: usize,
    },
    /// Return compact graph context for a task description.
    Context {
        /// Natural-language task description.
        task: String,
        /// Graph index directory.
        #[arg(long, default_value = ".harcgraph")]
        index: PathBuf,
        /// Maximum result lines per section.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Render an indexed graph as a standalone clickable HTML file.
    Html {
        /// Input graph directory.
        #[arg(long, default_value = ".harcgraph")]
        index: PathBuf,
        /// Output HTML file.
        #[arg(long, default_value = "harc-graph.html")]
        out: PathBuf,
        /// Page title.
        #[arg(long)]
        title: Option<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Check { files, ast } => learn_wrap(&files, || cmd_check(files.clone(), ast)),
        Cmd::Fmt { file, write } => cmd_fmt(file, write),
        Cmd::ManifestSources {
            manifest,
            probe_stub,
            all_artifacts,
        } => cmd_manifest_sources(&manifest, probe_stub, all_artifacts),
        Cmd::Sim {
            files,
            dut,
            sv,
            vlt,
            params,
            cpp_split_layout,
            top,
            test,
            compile_scope,
            codegen,
            cpp_split,
            cpp_split_group_size,
            emit_jobs,
            outdir,
            seed,
            emit_only,
            arch_bin,
            mt,
            coverage,
            ref_src,
            build_profile_input,
            z3_root,
            z3_include_dir,
            z3_lib_dir,
            rebuild,
            record_trace,
            coverage_json,
            waves,
            wave_format,
            wave_file,
            trace_depth,
            no_trace_structs,
            trace_max_width,
            trace_max_array,
            verilator_args,
            jobs,
            sim_args,
            check_backends,
            cosim,
        } => {
            let captured = files.clone();
            learn_wrap(&captured, || {
                let wave_opts = WaveOpts {
                    waves,
                    format: wave_format.clone(),
                    file: wave_file.clone(),
                    trace_depth,
                    trace_structs: !no_trace_structs,
                    trace_max_width,
                    trace_max_array,
                    verilator_args: verilator_args.clone(),
                    jobs,
                    sim_args: sim_args.clone(),
                };
                let z3_opts = Z3PathOpts {
                    root: z3_root.clone(),
                    include_dir: z3_include_dir.clone(),
                    lib_dir: z3_lib_dir.clone(),
                };
                let codegen = effective_codegen(codegen);
                if check_backends && coverage_json.is_some() {
                    return Err(miette::miette!(
                        "--coverage-json is not supported with --check-backends"
                    ));
                }
                if check_backends && cosim.is_some() {
                    return Err(miette::miette!(
                        "--cosim is not supported with --check-backends"
                    ));
                }
                if check_backends {
                    cmd_sim_check_backends(
                        files.clone(),
                        dut.clone(),
                        sv.clone(),
                        vlt.clone(),
                        params.clone(),
                        top.clone(),
                        test.clone(),
                        outdir.clone(),
                        seed,
                        arch_bin.clone(),
                        mt,
                        coverage,
                        ref_src.clone(),
                        build_profile_input.clone(),
                        z3_opts,
                        rebuild,
                        wave_opts,
                        codegen,
                    )
                } else {
                    cmd_sim(
                        files.clone(),
                        dut.clone(),
                        sv.clone(),
                        vlt.clone(),
                        params.clone(),
                        top.clone(),
                        test.clone(),
                        compile_scope,
                        codegen,
                        SplitOpts {
                            mode: cpp_split,
                            group_size: cpp_split_group_size,
                            layout: cpp_split_layout,
                            emit_jobs,
                        },
                        outdir.clone(),
                        seed,
                        emit_only,
                        arch_bin.clone(),
                        mt,
                        coverage,
                        ref_src.clone(),
                        build_profile_input.clone(),
                        z3_opts,
                        rebuild,
                        record_trace.clone(),
                        coverage_json.clone(),
                        wave_opts,
                        // Plain `--dut`/`--sv` path: the DUT `.arch` inputs are
                        // the interface source for port-override ingestion.
                        dut.clone(),
                        Vec::new(),
                        cosim.clone(),
                    )
                }
            })
        }
        Cmd::DumpIr {
            files,
            pass,
            profile,
        } => cmd_dump_ir(files, pass, profile),
        Cmd::TraceDiff { a, b } => cmd_trace_diff(&a, &b),
        Cmd::TraceMerge {
            vcd,
            trace,
            out,
            map_out,
        } => trace_merge::cmd_trace_merge(&vcd, &trace, &out, map_out.as_deref()),
        Cmd::Graph { cmd } => cmd_graph(cmd),
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

fn cmd_manifest_sources(manifest_path: &Path, probe_stub: bool, all_artifacts: bool) -> Result<()> {
    let manifest = harc::codegen::common_artifacts::read_manifest(manifest_path)
        .map_err(|error| miette::miette!(error.to_string()))?;
    let outdir = manifest_path.parent().unwrap_or_else(|| Path::new("."));
    let sources = if probe_stub {
        manifest.probe_stub().into_iter().collect::<Vec<_>>()
    } else if all_artifacts {
        manifest.build_inputs().collect::<Vec<_>>()
    } else {
        manifest.native_sources().collect::<Vec<_>>()
    };
    if sources.is_empty() {
        return Err(miette::miette!(
            "common-object manifest contains no {} source",
            if probe_stub {
                "probe-stub"
            } else if all_artifacts {
                "build-input"
            } else {
                "native"
            }
        ));
    }
    for source in sources {
        let path = outdir.join(source);
        if !path.is_file() {
            return Err(miette::miette!(
                "common-object manifest references missing native source {}",
                path.display()
            ));
        }
        println!("{}", path.display());
    }
    Ok(())
}

fn cmd_graph(cmd: GraphCmd) -> Result<()> {
    match cmd {
        GraphCmd::Index { paths, out } => {
            let stats = harc::graph::index_paths(&paths, &out).into_diagnostic()?;
            println!(
                "indexed: {} file(s), {} node(s), {} edge(s), {} skipped -> {}",
                stats.files,
                stats.nodes,
                stats.edges,
                stats.skipped,
                out.display()
            );
            Ok(())
        }
        GraphCmd::Query {
            query,
            index,
            limit,
        } => {
            println!(
                "{}",
                harc::graph::query(&index, &query, limit).into_diagnostic()?
            );
            Ok(())
        }
        GraphCmd::TestsFor {
            symbol,
            index,
            limit,
        } => {
            println!(
                "{}",
                harc::graph::tests_for(&index, &symbol, limit).into_diagnostic()?
            );
            Ok(())
        }
        GraphCmd::Impact {
            symbol,
            index,
            depth,
            limit,
        } => {
            println!(
                "{}",
                harc::graph::impact(&index, &symbol, depth, limit).into_diagnostic()?
            );
            Ok(())
        }
        GraphCmd::Context { task, index, limit } => {
            println!(
                "{}",
                harc::graph::context(&index, &task, limit).into_diagnostic()?
            );
            Ok(())
        }
        GraphCmd::Html { index, out, title } => {
            let graph = harc::graph::load_index(&index).into_diagnostic()?;
            let title = title.as_deref().unwrap_or("HARC graph");
            let html = harc::graph::render_html(&graph, title).into_diagnostic()?;
            if let Some(parent) = out.parent() {
                if !parent.as_os_str().is_empty() {
                    fs::create_dir_all(parent).into_diagnostic()?;
                }
            }
            fs::write(&out, html).into_diagnostic()?;
            eprintln!("Wrote {}", out.display());
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
    jobs: Option<u32>,
    sim_args: Vec<String>,
}

/// Split-C++-emission options carried through `cmd_sim`. Groups the
/// `--cpp-split` family with `--emit-jobs` so the long `cmd_sim` signature
/// shrinks rather than grows. See `Cmd::Sim` argument docs for the
/// per-field semantics. Mirrors `WaveOpts`.
#[derive(Debug, Clone, Copy)]
struct SplitOpts {
    mode: CppSplit,
    group_size: usize,
    layout: CppSplitLayout,
    /// Requested frontend emission workers; `0` means automatic. Resolved
    /// against the actual shard count by `tbir::resolve_emit_jobs`.
    emit_jobs: usize,
}

impl Default for SplitOpts {
    fn default() -> Self {
        SplitOpts {
            mode: CppSplit::Off,
            group_size: 1,
            layout: CppSplitLayout::SelfContained,
            emit_jobs: 1,
        }
    }
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

fn validate_param_overrides(params: &[String]) -> Result<()> {
    for param in params {
        let Some((name, value)) = param.split_once('=') else {
            return Err(miette::miette!(
                "invalid --param {param:?}: expected NAME=VALUE"
            ));
        };
        if name.is_empty() {
            return Err(miette::miette!(
                "invalid --param {param:?}: parameter name is empty"
            ));
        }
        if value.is_empty() {
            return Err(miette::miette!(
                "invalid --param {param:?}: parameter value is empty"
            ));
        }
        if name.chars().any(char::is_whitespace) {
            return Err(miette::miette!(
                "invalid --param {param:?}: parameter name must not contain whitespace"
            ));
        }
    }
    Ok(())
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
        let parsed = match harc::parser::parse_source_named(path.display().to_string(), &src) {
            Ok(p) => p,
            Err(_) => continue, // skip files we can't parse
        };
        let harc::ast::SourceFile {
            items: parsed_items,
            item_sources: parsed_item_sources,
            sources: parsed_sources,
            ..
        } = parsed;
        let mut bus_only = Vec::new();
        let mut item_sources = Vec::new();
        assert_eq!(parsed_items.len(), parsed_item_sources.len());
        for (item, source_id) in parsed_items.into_iter().zip(parsed_item_sources) {
            if matches!(item, Item::Bus(_)) {
                bus_only.push(item);
                item_sources.push(source_id);
            }
        }
        if !bus_only.is_empty() {
            for it in &bus_only {
                if let Item::Bus(b) = it {
                    already.insert(b.name.name.clone());
                }
            }
            imported.push(harc::ast::SourceFile {
                items: bus_only,
                item_sources,
                sources: parsed_sources,
                inner_doc: None,
                frontmatter: None,
            });
        }
    }
    imported
}

fn parse_file(path: &PathBuf) -> Result<harc::ast::SourceFile> {
    let src = fs::read_to_string(path).into_diagnostic()?;
    harc::parser::parse_source_named(path.display().to_string(), &src).map_err(|e| {
        Report::new(e).with_source_code(NamedSource::new(path.display().to_string(), src))
    })
}

fn parse_file_source(path: &PathBuf, src: &str) -> Result<harc::ast::SourceFile> {
    harc::parser::parse_source_named(path.display().to_string(), src).map_err(|e| {
        Report::new(e).with_source_code(NamedSource::new(
            path.display().to_string(),
            src.to_string(),
        ))
    })
}

fn lower_tbir(file: &harc::ast::SourceFile) -> Result<harc::ir::TbProgram> {
    harc::ir::lower::lower_program_diagnostic(file).map_err(|error| {
        let source_id = error.source_id();
        let report = Report::new(error);
        match file.source_for_id(source_id) {
            Some(source) => report.with_source_code(NamedSource::new(
                source.name.to_string(),
                source.text.to_string(),
            )),
            None => report,
        }
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
                &format!(
                    "supported width-method forms use a literal width in 1..={}",
                    harc::MAX_WIDTH_METHOD_BITS
                ),
                span,
            );
            return Err(Report::new(err).with_source_code(NamedSource::new(
                path.display().to_string(),
                src.to_string(),
            )));
        };
        if width == 0 || width > harc::MAX_WIDTH_METHOD_BITS {
            let span = tokens[window_start + 1].span.merge(tokens[close_idx].span);
            let err = harc::diagnostics::CompileError::unsupported_syntax(
                &format!("width method `.{method}<{width}>()` is outside the language limit"),
                &format!(
                    "width-method destinations must be in 1..={}",
                    harc::MAX_WIDTH_METHOD_BITS
                ),
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

/// `harc dump-ir [--pass <name>] <files...>` — parse, fold extends
/// (merge_for_sim), lower to TB-IR, verify, and print the textual IR
/// form. With `--pass`, additionally run the named TB-IR pass and
/// print its result after the IR dump.
fn cmd_dump_ir(files: Vec<PathBuf>, pass: Option<String>, profile: Option<String>) -> Result<()> {
    use harc::ir::passes::placement::TargetProfile;
    // Validate the pass name up front so a typo fails before the dump.
    enum DumpPass {
        None,
        LowerCoroutine,
        Placement,
    }
    let dump_pass = match pass.as_deref() {
        None => DumpPass::None,
        Some("lower-coroutine") | Some("lower_coroutine") => DumpPass::LowerCoroutine,
        Some("placement") => DumpPass::Placement,
        Some(other) => {
            return Err(miette::miette!(
                "unknown pass `{other}` (available: lower-coroutine, placement)"
            ));
        }
    };
    // Validate the profile up front too; it only applies to placement.
    let target_profile = match (&dump_pass, profile.as_deref()) {
        (_, None) => TargetProfile::single_site(),
        (DumpPass::Placement, Some(name)) => TargetProfile::by_name(name).ok_or_else(|| {
            miette::miette!("unknown profile `{name}` (built-ins: single-site, split-strict)")
        })?,
        (_, Some(_)) => {
            return Err(miette::miette!(
                "--profile only applies to `--pass placement`"
            ));
        }
    };
    let mut parsed_files = Vec::with_capacity(files.len());
    for f in &files {
        parsed_files.push(parse_file(f)?);
    }
    let extra_files = resolve_use_imports(&parsed_files, files.first());
    let mut all_files = parsed_files;
    all_files.extend(extra_files);
    let merged = harc::codegen::merge::merge_for_sim(all_files, None)
        .map_err(|e| miette::miette!("{}", e))?;
    let prog = lower_tbir(&merged)?;
    harc::ir::verify::verify_program(&prog).map_err(|errs| {
        let lines: Vec<String> = errs.iter().map(|e| format!("  - {e}")).collect();
        miette::miette!(
            "internal error: TB-IR failed verification after lowering:\n{}",
            lines.join("\n")
        )
    })?;
    print!("{prog}");
    match dump_pass {
        DumpPass::None => {}
        DumpPass::LowerCoroutine => {
            let meta = harc::ir::passes::lower_coroutine::run(&prog)
                .map_err(|e| miette::miette!("{}", e))?;
            println!();
            print!("{}", meta.display(&prog));
        }
        DumpPass::Placement => {
            let table = harc::ir::passes::placement::run(&prog, &target_profile);
            println!();
            print!("{}", table.display(&prog, &target_profile));
        }
    }
    Ok(())
}

/// `harc trace-diff <a.jsonl> <b.jsonl>` — normalize + diff two
/// semantic traces; exit 1 on any divergence.
fn cmd_trace_diff(a: &Path, b: &Path) -> Result<()> {
    let divergences =
        harc::check_backends::diff_traces(a, b).map_err(|e| miette::miette!("{}", e))?;
    if divergences.is_empty() {
        println!("traces match: {} == {}", a.display(), b.display());
        return Ok(());
    }
    eprintln!(
        "{} divergence(s) between {} and {}:",
        divergences.len(),
        a.display(),
        b.display()
    );
    for d in &divergences {
        eprintln!("  {}", d.fmt());
    }
    Err(miette::miette!(
        "traces diverge ({} difference(s) shown)",
        divergences.len()
    ))
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
    harc::codegen::common_artifacts::atomic_write_if_changed(path, contents)
        .map(|status| status == harc::codegen::common_artifacts::WriteStatus::Written)
        .map_err(|error| miette::miette!(error.to_string()))
}

/// Wall-clock for a build phase, in the one-decimal seconds the rest of
/// the emit progress output uses.
fn fmt_secs(d: std::time::Duration) -> String {
    format!("{:.1}s", d.as_secs_f64())
}

/// Generated-source size in decimal units — these numbers are read next
/// to `ls`/`du` output, not against memory page counts.
fn fmt_bytes(n: usize) -> String {
    const K: f64 = 1_000.0;
    let n = n as f64;
    if n >= K * K * K {
        format!("{:.1} GB", n / (K * K * K))
    } else if n >= K * K {
        format!("{:.1} MB", n / (K * K))
    } else if n >= K {
        format!("{:.1} kB", n / K)
    } else {
        format!("{n} B")
    }
}

fn sanitize_file_component(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "test".to_string()
    } else {
        out
    }
}

/// Generate the `--cosim dpi` SV harness (spec §10): instantiates the
/// DUT, exports the id-keyed signal accessors the TB shim calls, and
/// runs the timed master process that owns the clock and steps the HARC
/// runtime through `harc_cosim_step()`'s time protocol.
///
/// The DPI import/step call lives in ONE timed `initial` process, never
/// a separate `always @(edge)` block: Verilator evaluates a context
/// import's call expression in more than one scheduling region when it
/// sits in an `always` process, firing the import twice per event (see
/// docs/2026-07-24-dpi-cosim-exploration.md, finding 2). A `--timing`
/// process is a coroutine resumed exactly once per delay.
fn emit_cosim_harness(top: &str, co: &harc::codegen::cpp_tb::CosimOpts) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(s, "// Auto-generated by harc — do not edit.");
    let _ = writeln!(s, "// `--cosim dpi` harness for DUT `{top}` (spec §10).");
    // Native picosecond timebase: the step protocol's delays are integer
    // picosecond counts, so a 1 ps timeunit makes `#(rc)` exact with no
    // real-number conversion anywhere in the timing path. A 1ns/1ps unit
    // with fractional delays (`#(rc * 0.001)`) also simulates correctly —
    // Verilator scales real delays by the unit and rounds at the declared
    // precision, verified empirically — but integer-native is the
    // simplest thing that cannot drift, and it keeps `$time`/`%0t`
    // output (which IEEE rounds to the module's timeUNIT) aligned with
    // the protocol's ps ticks instead of quantizing displayed times to
    // whole nanoseconds.
    let _ = writeln!(s, "`timescale 1ps / 1ps");
    let _ = writeln!(s);
    let _ = writeln!(s, "module HarcCosimTop;");
    for p in &co.ports {
        if let Some(n) = p.unpacked_elems {
            if p.width_bits == 1 {
                let _ = writeln!(s, "  logic {} [{n}];", p.name);
            } else {
                let _ = writeln!(s, "  logic [{}:0] {} [{n}];", p.width_bits - 1, p.name);
            }
        } else if p.width_bits == 1 {
            let _ = writeln!(s, "  logic {};", p.name);
        } else if p.width_bits > 64 {
            // Wide ports get a word-rounded wire so the variable-base
            // 32-bit part-selects in the word accessors below never
            // reach past the top bit. The width mismatch on the DUT
            // connection is benign: inputs take the low bits, outputs
            // zero-extend (two-state), and -Wno-WIDTH covers the lint.
            let rounded = p.width_bits.div_ceil(32) * 32;
            let _ = writeln!(s, "  logic [{}:0] {};", rounded - 1, p.name);
        } else {
            let _ = writeln!(s, "  logic [{}:0] {};", p.width_bits - 1, p.name);
        }
    }
    let _ = writeln!(s);
    let _ = writeln!(s, "  {top} dut (");
    for (i, p) in co.ports.iter().enumerate() {
        let comma = if i + 1 == co.ports.len() { "" } else { "," };
        let _ = writeln!(s, "      .{}({}){}", p.name, p.name, comma);
    }
    let _ = writeln!(s, "  );");
    let _ = writeln!(s);
    s.push_str(
        "  import \"DPI-C\" context function void harc_cosim_init();\n\
         \x20 import \"DPI-C\" context function longint harc_cosim_step();\n\
         \x20 import \"DPI-C\" function void harc_cosim_shutdown();\n\
         \n\
         \x20 export \"DPI-C\" function harc_sv_get;\n\
         \x20 export \"DPI-C\" function harc_sv_set;\n\
         \x20 export \"DPI-C\" function harc_sv_get_word;\n\
         \x20 export \"DPI-C\" function harc_sv_set_word;\n\
         \x20 export \"DPI-C\" function harc_sv_get_elem;\n\
         \x20 export \"DPI-C\" function harc_sv_set_elem;\n\n",
    );
    // Typed accessors, id order == `CosimOpts::ports` order == the TB
    // shim's proxy parameters. Scalar (<= 64-bit packed) ports live in
    // these two tables; wider ports use the word accessors and
    // unpacked-array ports the element accessors below, with the same
    // shared id space.
    let _ = writeln!(s, "  function longint harc_sv_get(input int _harc_sig_id);");
    let _ = writeln!(s, "    case (_harc_sig_id)");
    for (id, p) in co.ports.iter().enumerate() {
        if p.width_bits > 64 || p.unpacked_elems.is_some() {
            continue;
        }
        let _ = writeln!(s, "      {id}: return longint'({});", p.name);
    }
    // Probe reads: the bound stub instance's read-side alias,
    // hierarchically (`bind <Top> __harc_probe_<Top> harc_probes ()`).
    for (probe, (read_id, _)) in co.probes.iter().zip(co.probe_ids()) {
        let _ = writeln!(
            s,
            "      {read_id}: return longint'(dut.harc_probes.{});",
            probe.name
        );
    }
    let _ = writeln!(s, "      default: return 0;");
    let _ = writeln!(s, "    endcase");
    let _ = writeln!(s, "  endfunction");
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "  function void harc_sv_set(input int _harc_sig_id, input longint _harc_value);"
    );
    let _ = writeln!(s, "    case (_harc_sig_id)");
    for (id, p) in co.ports.iter().enumerate() {
        // Outputs are read-only from the TB; the shim's proxy write would
        // be a codegen bug, so just omit them (falls to default).
        if p.width_bits > 64 || !p.is_input || p.unpacked_elems.is_some() {
            continue;
        }
        if p.width_bits == 1 {
            let _ = writeln!(s, "      {id}: {} = _harc_value[0];", p.name);
        } else {
            let _ = writeln!(
                s,
                "      {id}: {} = _harc_value[{}:0];",
                p.name,
                p.width_bits - 1
            );
        }
    }
    // Force-probe drive/enable writes into the bound stub; its
    // always_comb applies/releases the SV `force` on the target.
    for (probe, (_, force_ids)) in co.probes.iter().zip(co.probe_ids()) {
        let Some((drv_id, en_id)) = force_ids else {
            continue;
        };
        if probe.width_bits == 1 {
            let _ = writeln!(
                s,
                "      {drv_id}: dut.harc_probes.{}_drv = _harc_value[0];",
                probe.name
            );
        } else {
            let _ = writeln!(
                s,
                "      {drv_id}: dut.harc_probes.{}_drv = _harc_value[{}:0];",
                probe.name,
                probe.width_bits - 1
            );
        }
        let _ = writeln!(
            s,
            "      {en_id}: dut.harc_probes.{}_en = _harc_value[0];",
            probe.name
        );
    }
    let _ = writeln!(s, "      default: ;");
    let _ = writeln!(s, "    endcase");
    let _ = writeln!(s, "  endfunction");
    let _ = writeln!(s);
    // Word-granular accessors for >64-bit ports (LSB-first 32-bit
    // words, matching VlWide word order). The wide wires above are
    // word-rounded, so `word * 32 +: 32` stays in range.
    let _ = writeln!(
        s,
        "  function longint harc_sv_get_word(input int _harc_sig_id, input int _harc_word);"
    );
    let _ = writeln!(s, "    case (_harc_sig_id)");
    for (id, p) in co.ports.iter().enumerate() {
        if p.width_bits <= 64 || p.unpacked_elems.is_some() {
            continue;
        }
        if p.width_bits % 32 != 0 {
            // The wire is word-rounded; a signed wide OUTPUT sign-
            // extends into the pad bits at the port connection, so the
            // top word masks down to the port's real bits (matching
            // the direct backend's zero-filled VlWide top word).
            let top_word = p.width_bits / 32;
            let mask = (1u64 << (p.width_bits % 32)) - 1;
            let _ = writeln!(
                s,
                "      {id}: return (_harc_word == {top_word}) ?                  longint'({name}[{base} +: 32] & 32'h{mask:x}) :                  longint'({name}[_harc_word * 32 +: 32]);",
                name = p.name,
                base = top_word * 32,
            );
        } else {
            let _ = writeln!(
                s,
                "      {id}: return longint'({}[_harc_word * 32 +: 32]);",
                p.name
            );
        }
    }
    let _ = writeln!(s, "      default: return 0;");
    let _ = writeln!(s, "    endcase");
    let _ = writeln!(s, "  endfunction");
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "  function void harc_sv_set_word(input int _harc_sig_id, input int _harc_word, input longint _harc_value);"
    );
    let _ = writeln!(s, "    case (_harc_sig_id)");
    for (id, p) in co.ports.iter().enumerate() {
        if p.width_bits <= 64 || !p.is_input || p.unpacked_elems.is_some() {
            continue;
        }
        let _ = writeln!(
            s,
            "      {id}: {}[_harc_word * 32 +: 32] = _harc_value[31:0];",
            p.name
        );
    }
    let _ = writeln!(s, "      default: ;");
    let _ = writeln!(s, "    endcase");
    let _ = writeln!(s, "  endfunction");
    let _ = writeln!(s);
    // Element accessors for unpacked-array ports.
    let _ = writeln!(
        s,
        "  function longint harc_sv_get_elem(input int _harc_sig_id, input int _harc_idx);"
    );
    let _ = writeln!(s, "    case (_harc_sig_id)");
    for (id, p) in co.ports.iter().enumerate() {
        if p.unpacked_elems.is_none() {
            continue;
        }
        let _ = writeln!(s, "      {id}: return longint'({}[_harc_idx]);", p.name);
    }
    let _ = writeln!(s, "      default: return 0;");
    let _ = writeln!(s, "    endcase");
    let _ = writeln!(s, "  endfunction");
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "  function void harc_sv_set_elem(input int _harc_sig_id, input int _harc_idx, input longint _harc_value);"
    );
    let _ = writeln!(s, "    case (_harc_sig_id)");
    for (id, p) in co.ports.iter().enumerate() {
        if p.unpacked_elems.is_none() || !p.is_input {
            continue;
        }
        if p.width_bits == 1 {
            let _ = writeln!(s, "      {id}: {}[_harc_idx] = _harc_value[0];", p.name);
        } else {
            let _ = writeln!(
                s,
                "      {id}: {}[_harc_idx] = _harc_value[{}:0];",
                p.name,
                p.width_bits - 1
            );
        }
    }
    let _ = writeln!(s, "      default: ;");
    let _ = writeln!(s, "    endcase");
    let _ = writeln!(s, "  endfunction");
    s.push_str(
        r#"
  // Master process: owns simulated time, steps the HARC runtime.
  // Protocol (see runtime/harc_cosim_rt.h):
  //   rc > 0  → advance rc picoseconds
  //   rc == 0 → settle: advance 1 ps so NBA/comb updates land
  //   rc == -1 → done, pass ($finish; exit 0)
  //   rc <= -2 → done, fail ($fatal; nonzero exit)
  // The `break` after $finish/$fatal matters: Verilator's --timing
  // runtime defers $finish to the end of the timestep, so without it
  // this process would loop and step the finished HARC runtime again.
  // Locals are _harc_-prefixed: the surrounding module scope declares
  // one wire per DUT port, and DUTs legitimately have ports named
  // `rc`, `done`, etc.
  initial begin
    longint _harc_rc;
    bit _harc_done;
    harc_cosim_init();
    _harc_done = 0;
    while (!_harc_done) begin
      _harc_rc = harc_cosim_step();
      if (_harc_rc == -1) begin
        _harc_done = 1;
        $finish;
      end else if (_harc_rc <= -2) begin
        _harc_done = 1;
        $fatal(1, "HARC test failed");
      end else if (_harc_rc == 0) begin
        #1;
      end else begin
        #(_harc_rc);
      end
    end
  end

  // Runs on every simulation end, including a DUT-initiated $finish
  // the HARC runtime never sees. harc_cosim_shutdown() reports (and
  // exits nonzero) when the test had not finished — without this, an
  // early $finish would end the process with exit 0 and no test
  // summary, indistinguishable from silence.
  final begin
    harc_cosim_shutdown();
  end

endmodule
"#,
    );
    s
}

fn emit_probe_stub_if_needed(
    outdir: &Path,
    file: &harc::ast::SourceFile,
) -> Result<Option<PathBuf>> {
    let Some((dut_ty, probes)) = harc::codegen::cpp_tb::dut_probes(file)
        .map_err(|e| miette::miette!("probe catalog validation failed: {e}"))?
    else {
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

struct CommonManifestInputs {
    cpp: Vec<PathBuf>,
    probe_stub: Option<PathBuf>,
}

fn push_build_input_file(
    profile: &mut Vec<String>,
    kind: &str,
    index: usize,
    path: &Path,
) -> Result<()> {
    let canonical = fs::canonicalize(path).into_diagnostic()?;
    let contents = fs::read(&canonical).into_diagnostic()?;
    profile.push(format!(
        "{kind}:{index:08}:{}:{}",
        canonical.display(),
        harc::codegen::common_artifacts::stable_hash_hex(&contents)
    ));
    Ok(())
}

fn embedded_runtime_abi_fingerprint(include_cosim: bool) -> String {
    let mut canonical = String::new();
    for (name, contents) in [
        ("thread", harc::codegen::cpp_tb::THREAD_RT_HEADER),
        ("random", harc::codegen::cpp_tb::RANDOM_RT_HEADER),
        ("queue", harc::codegen::cpp_tb::QUEUE_RT_HEADER),
        ("trace", harc::codegen::cpp_tb::TRACE_RT_HEADER),
        ("log", harc::codegen::cpp_tb::LOG_RT_HEADER),
        ("z3", harc::codegen::cpp_tb::Z3_RT_HEADER),
    ] {
        canonical.push_str(name);
        canonical.push('=');
        canonical.push_str(&harc::codegen::common_artifacts::stable_hash_hex(
            contents.as_bytes(),
        ));
        canonical.push('\n');
    }
    if include_cosim {
        canonical.push_str("cosim=");
        canonical.push_str(&harc::codegen::common_artifacts::stable_hash_hex(
            harc::codegen::cpp_tb::COSIM_RT_HEADER.as_bytes(),
        ));
        canonical.push('\n');
    }
    harc::codegen::common_artifacts::stable_hash_hex(canonical.as_bytes())
}

fn common_abi_identity_inputs(waves: &WaveOpts, runtime_abi: &str) -> Vec<String> {
    vec![
        format!("runtime_abi={runtime_abi}"),
        format!(
            "trace_mode={}",
            if waves.waves {
                waves.format.as_str()
            } else {
                "disabled"
            }
        ),
    ]
}

fn tool_version_identity(program: &str) -> String {
    match Command::new(program).arg("--version").output() {
        Ok(output) => {
            let version = if output.stdout.is_empty() {
                output.stderr.as_slice()
            } else {
                output.stdout.as_slice()
            };
            let digest = harc::codegen::common_artifacts::stable_hash_hex(version);
            if output.status.success() {
                digest
            } else {
                format!("status={};{digest}", output.status)
            }
        }
        Err(error) => format!("unavailable:{:?}", error.kind()),
    }
}

fn verilator_version_identity() -> String {
    tool_version_identity("verilator")
}

fn common_manifest_inputs(
    manifest_path: &Path,
    expected_plan: &harc::codegen::common_artifacts::CommonArtifactPlan,
    expected_backend: harc::codegen::common_artifacts::CodegenBackend,
    expected_interface_abi: &str,
    expected_build_profile: &str,
    expected_placement: &harc::codegen::common_artifacts::PlacementMetrics,
) -> Result<CommonManifestInputs> {
    let manifest = harc::codegen::common_artifacts::read_manifest(manifest_path)
        .map_err(|error| miette::miette!(error.to_string()))?;
    if manifest.schema_version() != harc::codegen::common_artifacts::MANIFEST_SCHEMA_V2
        || manifest.backend() != Some(expected_backend)
        || manifest.layout() != Some(harc::codegen::common_artifacts::CppLayout::Common)
        || manifest.interface_abi() != expected_interface_abi
        || manifest.build_profile() != expected_build_profile
        || manifest
            .tests()
            .iter()
            .map(String::as_str)
            .ne(expected_plan.tests().iter().map(|test| test.name()))
        || manifest.placement() != Some(expected_placement)
        || manifest.artifacts()
            != expected_plan
                .artifacts()
                .iter()
                .map(|artifact| artifact.filename().to_string())
                .collect::<Vec<_>>()
    {
        return Err(miette::miette!(
            "common-object manifest identity does not match the generated suite: {}",
            manifest_path.display()
        ));
    }
    let outdir = manifest_path.parent().ok_or_else(|| {
        miette::miette!(
            "common-object manifest has no output directory: {}",
            manifest_path.display()
        )
    })?;
    let cpp = manifest
        .native_sources()
        .map(|filename| outdir.join(filename))
        .collect::<Vec<_>>();
    if cpp.is_empty() || cpp.iter().any(|path| !path.is_file()) {
        return Err(miette::miette!(
            "common-object manifest references a missing native source: {}",
            manifest_path.display()
        ));
    }
    let probe_stub = manifest.probe_stub().map(|filename| outdir.join(filename));
    if probe_stub.as_ref().is_some_and(|path| !path.is_file()) {
        return Err(miette::miette!(
            "common-object manifest references a missing probe stub: {}",
            manifest_path.display()
        ));
    }
    Ok(CommonManifestInputs { cpp, probe_stub })
}

fn absolutize_trace_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir().into_diagnostic()?.join(path))
}

fn prepare_native_build_directory(mdir: &Path, rebuild: bool, build_identity: &str) -> Result<()> {
    let identity_path = mdir.join(".harc_build_identity");
    let expected = format!("{build_identity}\n");
    let identity_matches = fs::read_to_string(&identity_path).is_ok_and(|value| value == expected);
    if mdir.exists() && (rebuild || !identity_matches) {
        fs::remove_dir_all(mdir).into_diagnostic()?;
    }
    fs::create_dir_all(mdir).into_diagnostic()?;
    harc::codegen::common_artifacts::atomic_write_if_changed(&identity_path, expected.as_bytes())
        .map_err(|error| miette::miette!(error.to_string()))?;
    Ok(())
}

fn run_verilator(
    top: &str,
    sv: &[PathBuf],
    vlt: &[PathBuf],
    cpp: &[PathBuf],
    outdir_abs: &PathBuf,
    sim_log_path: &PathBuf,
    seed: Option<u64>,
    coverage: bool,
    ref_src: &[PathBuf],
    z3_paths: &Z3Paths,
    test: Option<&str>,
    rebuild: bool,
    record_trace: Option<&PathBuf>,
    coverage_json: Option<&PathBuf>,
    waves: &WaveOpts,
    params: &[String],
    cosim: bool,
    build_identity: &str,
) -> Result<()> {
    let mdir = outdir_abs.join("obj_dir");
    // Build-reuse path (Phase 1c). When `--rebuild` is unset and the
    // emitted .cpp is byte-identical to the previous run's, Make's
    // mtime-based skip kicks in and Verilator finishes in ~0.1s
    // instead of ~5-10s. `--rebuild` (or a deleted outdir) forces
    // a fresh build — useful when Verilator was upgraded or when
    // verilator flags changed in a way the emitted .cpp doesn't
    // capture.
    prepare_native_build_directory(&mdir, rebuild, build_identity)?;

    let mut args: Vec<String> = if cosim {
        // `--cosim dpi`: the simulator owns time. `--binary` =
        // `--main --exe --build --timing`: Verilator supplies `main()`,
        // the generated harness's timed master process owns the clock,
        // and the emitted TB is a passive DPI-C library. `--timing`
        // replaces the direct path's `--no-timing` — the harness needs
        // real delays, and DUT-internal `#delay` statements are
        // scheduled instead of elided.
        vec!["--binary".into(), "--timing".into()]
    } else {
        vec!["--cc".into(), "--exe".into(), "--build".into()]
    };
    args.extend([
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
    ]);
    if !cosim {
        args.extend([
            // Cycle-based TBs don't need delay semantics; tell Verilator
            // to elide `#N` delay statements rather than refusing to
            // elaborate. HARC's `wait N cycles` is always cycle-based
            // (handled by the runtime scheduler) — delays inside a DUT
            // are a property of the DUT author, not the TB, and CVDP
            // coverage scoring ignores delay semantics too.
            "--no-timing".into(),
        ]);
    }
    args.extend([
        "--top-module".into(),
        top.into(),
        "--Mdir".into(),
        mdir.display().to_string(),
    ]);
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
    // *flipping* trace on/off changes the native build identity below,
    // so cached objects compiled with the other trace configuration are
    // discarded before Verilator runs.
    if waves.waves {
        let trace_flag = match waves.format.as_str() {
            // `--trace` (not `--trace-vcd`) is the portable spelling for
            // VCD: it enables VCD tracing in every Verilator release,
            // whereas `--trace-vcd` only exists in >= 5.036 and errors out
            // on the pinned CI Verilator (5.034). Both map to the same
            // `VerilatedVcdC` backend the emitted TB uses under
            // `HARC_TRACE_VCD`.
            "vcd" => "--trace",
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
    for param in params {
        args.push(format!("-G{param}"));
    }
    if let Some(jobs) = waves.jobs {
        args.push("-j".into());
        args.push(jobs.to_string());
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
        // GCC has the same class of miscompile. Named-lambda fix
        // (2026-06-22): each coroutine is stored in a named local
        // (`auto _foo_lambda = [&](){...}; slot.thread =
        // _foo_lambda(&slot);`) so the closure lives for the full
        // run_<Test> scope, not as a temporary freed at the IIFE
        // semicolon. This makes GCC work without HARC_CXX=clang++.
        // The `#pragma clang optimize off` stays as extra defence.
        "-MAKEFLAGS".into(),
        // `CXX=${HARC_CXX:-c++}` lets CI override the compiler
        // without changing harc-com source. On macOS, `c++` aliases
        // clang and the existing `#pragma clang optimize off` does
        // its thing. The named-lambda fix also covers GCC, so
        // HARC_CXX=clang++ is no longer strictly required on Linux.
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
    for c in cpp {
        args.push(c.display().to_string());
    }

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
    if let Some(path) = coverage_json {
        cmd.env("HARC_COVERAGE_JSONL", path);
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
        if cosim {
            // Co-sim binaries have Verilator's generated `main()`, not
            // the HARC dispatcher; `harc_cosim_init` reads HARC_TEST.
            cmd.env("HARC_TEST", t);
        } else {
            cmd.args(&["--test", t]);
        }
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

#[allow(clippy::too_many_arguments)]
fn cmd_sim(
    files: Vec<PathBuf>,
    dut: Vec<PathBuf>,
    sv: Vec<PathBuf>,
    vlt: Vec<PathBuf>,
    params: Vec<String>,
    top: Option<String>,
    test: Option<String>,
    compile_scope: CompileScope,
    codegen: CodegenKind,
    split: SplitOpts,
    outdir: Option<PathBuf>,
    seed: Option<u64>,
    emit_only: bool,
    arch_bin: Option<PathBuf>,
    mt: bool,
    coverage: bool,
    ref_src: Vec<PathBuf>,
    external_build_profile_inputs: Vec<String>,
    z3_opts: Z3PathOpts,
    rebuild: bool,
    record_trace: Option<PathBuf>,
    coverage_json: Option<PathBuf>,
    waves: WaveOpts,
    // DUT `.arch`/`.archi` interface files used ONLY to ingest port-level bus
    // param overrides (`port s: target BusRw<WRITE=0>`) for `generate_if`-gate
    // modeling. Distinct from `dut` (which also selects the ARCH-sim backend):
    // on the `--check-backends` SV backend run `dut` is empty but the override
    // still must be applied so both backends model the same flattened port set.
    // On the plain `--dut` path this equals `dut`.
    dut_iface: Vec<PathBuf>,
    // Extra SV sources scanned into the DUT interface catalog WITHOUT
    // selecting the Verilator backend. `--check-backends` uses this to give
    // the ARCH-sim run the same flattened bus-port set `arch build` produced
    // (the native `.arch` catalog does not flatten bus perspective ports).
    catalog_sv: Vec<PathBuf>,
    // `--cosim dpi`: simulator-owned-time DPI-C co-sim (spec §10).
    cosim: Option<String>,
) -> Result<()> {
    validate_param_overrides(&params)?;

    if cosim.is_some() {
        if sv.is_empty() {
            return Err(miette::miette!("--cosim dpi requires --sv <file.sv>"));
        }
        if codegen != CodegenKind::Tbir {
            return Err(miette::miette!(
                "--cosim dpi requires the default TB-IR codegen path"
            ));
        }
        if split.mode == CppSplit::Tests {
            return Err(miette::miette!(
                "--cosim dpi does not support --cpp-split tests yet"
            ));
        }
        if waves.waves {
            return Err(miette::miette!(
                "--cosim dpi does not support --waves yet; on the co-sim path \
                 waveforms belong to the simulator (a future revision will \
                 plumb $dumpvars through the generated harness)"
            ));
        }
        if coverage {
            return Err(miette::miette!(
                "--cosim dpi does not support --coverage yet (Verilator \
                 coverage belongs to the simulator process on this path)"
            ));
        }
        if mt {
            return Err(miette::miette!("--cosim dpi does not support --mt yet"));
        }
        if !params.is_empty() {
            return Err(miette::miette!(
                "--cosim dpi does not support --param yet: the -G override \
                 would target the generated HarcCosimTop harness (which has \
                 no parameters), and the accessor widths are folded from the \
                 DUT's default parameter values"
            ));
        }
    }

    if coverage_json.is_some() && codegen != CodegenKind::Tbir {
        return Err(miette::miette!(
            "--coverage-json requires the default TB-IR codegen path"
        ));
    }

    if dut.is_empty() && sv.is_empty() {
        return Err(miette::miette!(
            "pass either --dut <file.arch> or --sv <file.sv>"
        ));
    }
    if !dut.is_empty() && !sv.is_empty() {
        return Err(miette::miette!(
            "pass either --dut <file.arch> or --sv <file.sv>, not both \
             (use --check-backends to run under both backends)"
        ));
    }
    if !dut.is_empty() && split.mode == CppSplit::Tests {
        return Err(miette::miette!(
            "--cpp-split tests is currently supported only with --sv / Verilator builds"
        ));
    }
    if split.layout == CppSplitLayout::Common {
        if split.mode != CppSplit::Tests {
            return Err(miette::miette!(
                "--cpp-split-layout common requires --cpp-split tests"
            ));
        }
        if split.group_size != 4 {
            return Err(miette::miette!(
                "--cpp-split-group-size does not apply to --cpp-split-layout common \
                 (it always emits one stable capsule per test); drop the flag"
            ));
        }
        if cosim.is_some() {
            return Err(miette::miette!(
                "--cpp-split-layout common does not support split DPI co-simulation yet"
            ));
        }
        if codegen == CodegenKind::Tbir {
            if compile_scope != CompileScope::Suite {
                return Err(miette::miette!(
                    "TB-IR common layout currently requires --compile-scope suite"
                ));
            }
            if !vlt.is_empty() || !waves.verilator_args.is_empty() {
                return Err(miette::miette!(
                    "TB-IR common layout does not yet support custom Verilator inputs/arguments; use \
                     --cpp-split-layout self-contained (ticket 09)"
                ));
            }
        }
    }

    // Parse every input file, then fold `extend test T` blocks into their
    // matching base test before codegen.
    let parse_started = Instant::now();
    let mut parsed_files = Vec::with_capacity(files.len());
    for f in &files {
        parsed_files.push(parse_file(f)?);
    }
    let parse_elapsed = parse_started.elapsed();
    // Resolve `use Name` declarations against the search path. For each
    // unresolved `use`, look for `<Name>.arch` (or `<Name>.harc`) in
    // a small set of conventional locations, parse it, and append any
    // `bus` items it declares to the synthetic file list. Unresolved
    // uses silently no-op (back-compat — many existing fixtures
    // include `use arc.stdlib.X` lines that don't resolve to anything
    // yet).
    // `resolve_use_imports` probes the search path and PARSES whatever it
    // resolves, so it is timed with parse rather than merge — folding it
    // into "merge" hid an imported file's parse cost under the wrong label.
    let imports_started = Instant::now();
    let extra_files = resolve_use_imports(&parsed_files, files.first());
    let mut all_files = parsed_files;
    all_files.extend(extra_files);
    let parse_elapsed = parse_elapsed + imports_started.elapsed();

    let merge_started = Instant::now();
    let merged = harc::codegen::merge::merge_for_sim(all_files, test.as_deref())
        .map_err(|e| miette::miette!("{}", e))?;
    let merge_elapsed = merge_started.elapsed();
    // Suite scope hands the merged file straight to codegen. It used to
    // `.clone()` here, which on a large suite is a deep copy of the whole
    // merged AST — 9.4s of a 46s frontend on the 352-test benchmark — and
    // `merged` is not read again either way. Test scope still borrows it
    // to build a filtered copy, which is why the move lives in the arm
    // rather than above the match.
    let codegen_source = match compile_scope {
        CompileScope::Suite => merged,
        CompileScope::Test => {
            let selected = test
                .as_deref()
                .ok_or_else(|| miette::miette!("--compile-scope test requires --test <name>"))?;
            harc::codegen::merge::filter_tests_for_codegen(&merged, selected)
                .map_err(|e| miette::miette!("{}", e))?
        }
    };

    let mut uses_solver = harc::codegen::cpp_tb::uses_constraint_solver(&codegen_source);
    let z3_paths = resolve_z3_paths(&z3_opts);

    let outdir = outdir.unwrap_or_else(|| PathBuf::from("harc_sim_build"));
    fs::create_dir_all(&outdir).into_diagnostic()?;
    let stem = files[0]
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("harc_tb");
    let top_for_scan = top
        .clone()
        .or_else(|| harc::codegen::cpp_tb::dut_type_name(&codegen_source));
    // On the `--sv` (Verilator) path, scan the SV DUT for ports that
    // flatten to a packed multi-lane vector (`Vec<Bus, N>` / multi-lane
    // bus ports) and record their per-lane bit-width. The TB codegen
    // uses this to lower `dut.<port>[i]` lane accesses through the
    // backend-neutral bit-extract / lane-read helper.
    let mut vec_lane_widths = if !sv.is_empty() {
        match top_for_scan.as_deref() {
            Some(t) => harc::codegen::cpp_tb::vec_lane_widths_from_sv(&sv, t),
            None => std::collections::HashMap::new(),
        }
    } else {
        std::collections::HashMap::new()
    };
    // Native ARCH scalar ports are C++ integers, not arrays. Treat an
    // indexed scalar (`UInt<N>[i]`) as packed one-bit lanes so both v1 and
    // TBIR route the read through `harc_vec_lane_read<1>` instead of emitting
    // invalid `dut->port[i]` C++. True unpacked Vec ports are intentionally
    // absent from this scalar-only table and retain direct array indexing.
    let arch_scalar_port_widths = match top_for_scan.as_deref() {
        Some(t) => harc::codegen::cpp_tb::dut_port_widths_from_files(&dut_iface, t),
        None => std::collections::HashMap::new(),
    };
    harc::codegen::cpp_tb::add_arch_scalar_bit_lanes(
        &mut vec_lane_widths,
        &arch_scalar_port_widths,
    );
    let mut dut_port_widths = if !sv.is_empty() {
        match top_for_scan.as_deref() {
            Some(t) => harc::codegen::cpp_tb::dut_port_widths_from_sv(&sv, t),
            None => std::collections::HashMap::new(),
        }
    } else {
        std::collections::HashMap::new()
    };
    for (name, width) in arch_scalar_port_widths {
        dut_port_widths.entry(name).or_insert(width);
    }
    // Ingest DUT-port-level bus param overrides from the DUT `.arch`/`.archi`
    // interface (the authoritative source post arch#567). When a DUT module
    // declares `port s: target BusRw<WRITE=0>`, `arch build` flattens `s`
    // without the WRITE-gated channels; folding the override into the bus bind's
    // effective param env makes harc model the same port set as both backends.
    let dut_bus_port_overrides =
        harc::codegen::cpp_tb::dut_bus_port_overrides_from_files(&dut_iface);
    // `--cosim dpi`: discover the DUT's full port table for the accessor
    // shim + generated harness. Same tolerant textual scan as
    // `vec_lane_widths_from_sv`; a top module we can't find is a hard
    // error here (the harness can't be generated without ports).
    let cosim_opts = if cosim.is_some() {
        let top_for_scan = top
            .clone()
            .or_else(|| harc::codegen::cpp_tb::dut_type_name(&codegen_source))
            .ok_or_else(|| {
                miette::miette!("--cosim dpi: cannot determine the DUT top module (pass --top)")
            })?;
        let ports =
            harc::codegen::cpp_tb::cosim_ports_from_sv(&sv, &top_for_scan).ok_or_else(|| {
                miette::miette!(
                    "--cosim dpi: could not scan the port list of module `{}` from the \
                     --sv sources (ANSI-style port declarations required)",
                    top_for_scan
                )
            })?;
        // Probes route through the same bound SV stub as the direct
        // backend; the harness reaches its signals hierarchically. Only
        // the accessor width is constrained (<= 64 bits, like ports).
        let mut probes = Vec::new();
        if let Some((_, probe_decls)) = harc::codegen::cpp_tb::dut_probes(&codegen_source)
            .map_err(|e| miette::miette!("probe catalog validation failed: {e}"))?
        {
            for pr in &probe_decls {
                let width = harc::codegen::sv_stub::probe_width_bits(&pr.ty).unwrap_or(1);
                if width > 64 {
                    return Err(miette::miette!(
                        "--cosim dpi: probe `{}` is {} bits wide; probes wider than \
                         64 bits are not supported yet on the co-sim path",
                        pr.name.name,
                        width
                    ));
                }
                probes.push(harc::codegen::cpp_tb::CosimProbe {
                    name: pr.name.name.clone(),
                    width_bits: width,
                    force: pr.force,
                });
            }
        }
        Some(harc::codegen::cpp_tb::CosimOpts {
            ports,
            probes,
            // 5 ns half period (100 MHz) — arbitrary but stable; HARC test
            // semantics are cycle-based, not wall-time-based.
            half_period_ps: 5000,
        })
    } else {
        None
    };
    let top_for_interface = top
        .clone()
        .or_else(|| harc::codegen::cpp_tb::dut_type_name(&codegen_source));
    let mut interface_sv = sv.clone();
    interface_sv.extend(catalog_sv.iter().cloned());
    let dut_interface = top_for_interface
        .as_deref()
        .map(|top_name| {
            harc::codegen::cpp_tb::dut_interface_catalog_with_parameter_overrides(
                &interface_sv,
                &dut_iface,
                top_name,
                &vec_lane_widths,
                &params,
            )
        })
        .transpose()
        .map_err(|error| miette::miette!(error))?
        .flatten();
    if let Some(interface) = &dut_interface {
        for port in interface.ports() {
            if let Some(width) = port.resolved_width() {
                dut_port_widths.insert(port.name().to_string(), width);
            }
            if let Some(width) = port.packed_lane_width() {
                vec_lane_widths.insert(port.name().to_string(), width);
            }
        }
    }
    let codegen_identity = match codegen {
        CodegenKind::V1 => "v1",
        CodegenKind::Tbir => "tbir",
    };
    let layout_identity = match (split.mode, split.layout) {
        (CppSplit::Off, _) => "single",
        (CppSplit::Tests, CppSplitLayout::SelfContained) => "self-contained",
        (CppSplit::Tests, CppSplitLayout::Common) => "common",
    };
    let mut build_profile_inputs = params
        .iter()
        .enumerate()
        .map(|(index, param)| format!("param:{index:08}:{param}"))
        .collect::<Vec<_>>();
    build_profile_inputs.push(format!("harc_version={}", env!("CARGO_PKG_VERSION")));
    build_profile_inputs.push(format!("backend={codegen_identity}"));
    build_profile_inputs.push(format!("layout={layout_identity}"));
    build_profile_inputs.push(format!(
        "top={}",
        top_for_interface.as_deref().unwrap_or_default()
    ));
    if let Some((dut_type, probes)) = harc::codegen::cpp_tb::dut_probes(&codegen_source)
        .map_err(|error| miette::miette!("probe catalog validation failed: {error}"))?
    {
        let probe_stub = harc::codegen::sv_stub::emit_stub(&dut_type, &probes)
            .map_err(|error| miette::miette!("probe stub emit failed: {error}"))?;
        build_profile_inputs.push(format!(
            "probes={}",
            harc::codegen::common_artifacts::stable_hash_hex(probe_stub.as_bytes())
        ));
    }
    build_profile_inputs.push(format!("cosim={}", cosim_opts.is_some()));
    build_profile_inputs.push(format!("coverage={coverage}"));
    build_profile_inputs.push(format!(
        "waves={}",
        waves
            .waves
            .then(|| waves.format.clone())
            .unwrap_or_default()
    ));
    build_profile_inputs.push(format!("trace_structs={}", waves.trace_structs));
    build_profile_inputs.push(format!("trace_max_width={}", waves.trace_max_width));
    build_profile_inputs.push(format!(
        "trace_max_array={}",
        waves
            .trace_max_array
            .map_or_else(String::new, |value| value.to_string())
    ));
    for (index, argument) in waves.verilator_args.iter().enumerate() {
        build_profile_inputs.push(format!("verilator_arg:{index:08}:{argument}"));
    }
    build_profile_inputs.push(
        "native_flags=gnu++20;-Wno-fatal;-Wno-WIDTH;-Wno-BLKANDNBLK;-Wno-UNOPTFLAT".to_string(),
    );
    let runtime_abi = embedded_runtime_abi_fingerprint(cosim_opts.is_some());
    build_profile_inputs.push(format!("runtime_abi={runtime_abi}"));
    let cxx = std::env::var("HARC_CXX").unwrap_or_else(|_| "c++".to_string());
    build_profile_inputs.push(format!("cxx={cxx}"));
    build_profile_inputs.push(format!("cxx_version={}", tool_version_identity(&cxx)));
    if !sv.is_empty() {
        build_profile_inputs.push(format!("verilator={}", verilator_version_identity()));
    }
    build_profile_inputs.push(format!(
        "z3_inc={}",
        z3_paths
            .include_dir
            .as_deref()
            .map_or_else(String::new, |path| path.display().to_string())
    ));
    build_profile_inputs.push(format!(
        "z3_lib={}",
        z3_paths
            .lib_dir
            .as_deref()
            .map_or_else(String::new, |path| path.display().to_string())
    ));
    for (index, path) in ref_src.iter().enumerate() {
        push_build_input_file(&mut build_profile_inputs, "ref_src", index, path)?;
    }
    for (index, path) in vlt.iter().enumerate() {
        push_build_input_file(&mut build_profile_inputs, "vlt", index, path)?;
    }
    for (index, path) in sv.iter().enumerate() {
        push_build_input_file(&mut build_profile_inputs, "sv", index, path)?;
    }
    for (index, value) in external_build_profile_inputs.iter().enumerate() {
        build_profile_inputs.push(format!("external:{index:08}:{value}"));
    }
    let mut native_build_identity =
        harc::codegen::common_artifacts::build_profile_fingerprint(mt, &build_profile_inputs);
    let emit_opts = harc::codegen::cpp_tb::EmitOpts {
        mt,
        vec_lane_widths,
        dut_port_widths,
        dut_interface,
        build_profile_inputs,
        common_abi_inputs: common_abi_identity_inputs(&waves, &runtime_abi),
        dut_bus_port_overrides,
        cosim: cosim_opts.clone(),
    };
    let mut cpp_paths = Vec::new();
    let mut probe_stub_path = None;
    let mut runtime_headers_published = false;
    match split.mode {
        CppSplit::Off => {
            let cpp = match codegen {
                CodegenKind::V1 => {
                    harc::codegen::cpp_tb::emit_with_opts(&codegen_source, emit_opts)
                        .map_err(|e| miette::miette!("{}", e))?
                }
                CodegenKind::Tbir => {
                    let prog = lower_tbir(&codegen_source)?;
                    uses_solver |= !prog.constraint_sites.is_empty();
                    harc::ir::verify::verify_program(&prog).map_err(|errs| {
                        let lines: Vec<String> = errs.iter().map(|e| format!("  - {e}")).collect();
                        miette::miette!(
                            "internal error: TB-IR failed verification after lowering:\n{}",
                            lines.join("\n")
                        )
                    })?;
                    harc::codegen::tbir::emit(&prog, &codegen_source, &emit_opts)
                        .map_err(|e| miette::miette!("{}", e))?
                }
            };
            let cpp_filename = match compile_scope {
                CompileScope::Suite => format!("{stem}.cpp"),
                CompileScope::Test => {
                    let selected = test.as_deref().unwrap_or("test");
                    format!("{stem}__test_{}.cpp", sanitize_file_component(selected))
                }
            };
            let cpp_path = outdir.join(cpp_filename);
            // Only rewrite generated sources when content actually changed.
            // Phase 1c: keeps mtimes stable so Verilator's Make skips
            // recompilation when runtime-only selectors change.
            let cpp_changed = write_if_changed(&cpp_path, cpp.as_bytes())?;
            if cpp_changed {
                eprintln!("emitted {}", cpp_path.display());
            } else {
                eprintln!("reused {} (unchanged)", cpp_path.display());
            }
            cpp_paths.push(cpp_path);
        }
        CppSplit::Tests => match codegen {
            CodegenKind::V1 if split.layout == CppSplitLayout::Common => {
                // Common-object layout (issue #643): reusable infra
                // compiles once; each test is a small stable capsule;
                // an explicit registry dispatches by name. Artifacts
                // are written via write_if_changed in deterministic
                // order so Verilator's incremental Make path stays
                // exact.
                let profile_extra: Vec<String> = Vec::new();
                let started = Instant::now();
                let prefix = format!("{stem}__");
                let suite = harc::codegen::cpp_tb::emit_common_split(
                    &codegen_source,
                    emit_opts.clone(),
                    &prefix,
                    &profile_extra,
                )
                .map_err(|e| miette::miette!("{}", e))?;
                let manifest_identity = harc::codegen::common_artifacts::ManifestIdentity::new(
                    harc::codegen::common_artifacts::CodegenBackend::V1,
                    harc::codegen::common_artifacts::CppLayout::Common,
                    &suite.interface_abi,
                    &suite.build_profile,
                    suite.placement.clone(),
                );
                let publication = suite
                    .artifact_plan
                    .begin_publication_v2(&outdir, &manifest_identity)
                    .map_err(|error| miette::miette!(error.to_string()))?;
                for generated in &suite.files {
                    let cpp_path = outdir.join(&generated.filename);
                    let status = publication
                        .write(&generated.filename, generated.contents.as_bytes())
                        .map_err(|error| miette::miette!(error.to_string()))?;
                    if status == harc::codegen::common_artifacts::WriteStatus::Written {
                        eprintln!("emitted {}", cpp_path.display());
                    } else {
                        eprintln!("reused {} (unchanged)", cpp_path.display());
                    }
                }
                for (filename, contents) in harc::codegen::cpp_tb::RUNTIME_HEADERS {
                    publication
                        .write(filename, contents.as_bytes())
                        .map_err(|error| miette::miette!(error.to_string()))?;
                }
                let publication = publication
                    .commit()
                    .map_err(|error| miette::miette!(error.to_string()))?;
                for stale in publication.removed() {
                    eprintln!("removed stale {}", stale.display());
                }
                eprintln!(
                    "HARC common split: {} artifacts ({} rewritten), interface abi {}, profile {}, in {:?}",
                    suite.files.len(),
                    publication.rewritten_artifacts(),
                    suite.interface_abi,
                    suite.build_profile,
                    started.elapsed()
                );
                let inputs = common_manifest_inputs(
                    &outdir.join(suite.artifact_plan.manifest_filename()),
                    &suite.artifact_plan,
                    harc::codegen::common_artifacts::CodegenBackend::V1,
                    &suite.interface_abi,
                    &suite.build_profile,
                    &suite.placement,
                )?;
                cpp_paths = inputs.cpp;
                probe_stub_path = inputs.probe_stub;
                native_build_identity =
                    format!("common:v1:{}:{}", suite.interface_abi, suite.build_profile);
                runtime_headers_published = true;
            }
            CodegenKind::V1 => {
                let batch = harc::codegen::cpp_tb::emit_split_tests_with_file_prefix(
                    &codegen_source,
                    emit_opts,
                    &format!("{stem}__"),
                    split.group_size,
                )
                .map_err(|e| miette::miette!("{}", e))?;
                for generated in batch.files {
                    let cpp_path = outdir.join(generated.filename);
                    let cpp_changed = write_if_changed(&cpp_path, generated.contents.as_bytes())?;
                    if cpp_changed {
                        eprintln!("emitted {}", cpp_path.display());
                    } else {
                        eprintln!("reused {} (unchanged)", cpp_path.display());
                    }
                    if cpp_path.extension().is_some_and(|ext| ext == "cpp") {
                        cpp_paths.push(cpp_path);
                    }
                }
            }
            CodegenKind::Tbir if split.layout == CppSplitLayout::Common => {
                eprintln!(
                    "TBIR parse: {} | merge: {}",
                    fmt_secs(parse_elapsed),
                    fmt_secs(merge_elapsed),
                );
                let lower_started = Instant::now();
                let prog = lower_tbir(&codegen_source)?;
                let lower_elapsed = lower_started.elapsed();
                uses_solver |= !prog.constraint_sites.is_empty();
                let verify_started = Instant::now();
                harc::ir::verify::verify_program(&prog).map_err(|errs| {
                    let lines: Vec<String> = errs.iter().map(|e| format!("  - {e}")).collect();
                    miette::miette!(
                        "internal error: TB-IR failed verification after lowering:\n{}",
                        lines.join("\n")
                    )
                })?;
                let verify_elapsed = verify_started.elapsed();
                eprintln!(
                    "TBIR lower: {} | verify: {}",
                    fmt_secs(lower_elapsed),
                    fmt_secs(verify_elapsed),
                );

                let plan_started = Instant::now();
                let prefix = format!("{stem}__");
                harc::codegen::tbir::check_gated_bus_access(&prog, &codegen_source, &emit_opts)
                    .map_err(|error| miette::miette!(error.to_string()))?;
                let plan = harc::codegen::tbir::common::plan_common_tests_with_source(
                    &prog,
                    &codegen_source,
                    &emit_opts,
                    &prefix,
                )
                .map_err(|error| miette::miette!(error.to_string()))?;
                if top.as_deref().is_some_and(|top| top != plan.dut_type()) {
                    return Err(miette::miette!(
                        "TB-IR common layout requires --top to match the verified DUT type `{}`",
                        plan.dut_type()
                    ));
                }
                let jobs =
                    harc::codegen::tbir::resolve_emit_jobs(split.emit_jobs, plan.capsules().len());
                let common_callables = plan
                    .callables()
                    .iter()
                    .filter(|callable| {
                        matches!(
                            callable.placement(),
                            harc::codegen::tbir::common::CallablePlacement::Common
                        )
                    })
                    .count();
                let mut capsule_reasons = std::collections::BTreeMap::<String, usize>::new();
                for callable in plan.callables() {
                    match callable.placement() {
                        harc::codegen::tbir::common::CallablePlacement::CapsuleLocal {
                            reason,
                            ..
                        }
                        | harc::codegen::tbir::common::CallablePlacement::CapsuleScoped {
                            reason,
                        } => {
                            *capsule_reasons.entry(format!("{reason:?}")).or_default() += 1;
                        }
                        _ => {}
                    }
                }
                let capsule_callables = capsule_reasons.values().sum::<usize>();
                let capsule_summary = capsule_reasons
                    .iter()
                    .map(|(reason, count)| format!("{reason}={count}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                eprintln!(
                    "TBIR common plan: {} tests, {} common unit(s), {} capsule-local test-body unit(s), {common_callables} common callable(s), {capsule_callables} capsule-owned callable(s) [{}], emit jobs {jobs}, planned in {}",
                    plan.artifact_plan().tests().len(),
                    plan.artifact_plan().common_units().len(),
                    plan.capsules().len(),
                    capsule_summary,
                    fmt_secs(plan_started.elapsed()),
                );

                let rendered = plan
                    .publication()
                    .map_err(|error| miette::miette!(error.to_string()))?;
                let interface_cpp = rendered.interface();
                let runtime_cpp = rendered
                    .runtime()
                    .map_err(|error| miette::miette!(error.to_string()))?;
                let registry_cpp = rendered.registry();
                let probe_stub = plan
                    .artifact_plan()
                    .probe_stub()
                    .map(|artifact| {
                        harc::codegen::sv_stub::emit_stub_from_plan(plan.dut_access())
                            .map(|contents| (artifact.filename().to_string(), contents))
                    })
                    .transpose()
                    .map_err(|error| miette::miette!("probe stub emit failed: {error}"))?;

                let publication = rendered
                    .begin_publication(&outdir)
                    .map_err(|error| miette::miette!(error.to_string()))?;
                let interface_artifact = plan.artifact_plan().interface();
                let interface_path = outdir.join(interface_artifact.filename());
                let interface_status = publication
                    .write(interface_artifact.filename(), interface_cpp.as_bytes())
                    .map_err(|error| miette::miette!(error.to_string()))?;
                eprintln!(
                    "{} {} (interface)",
                    if interface_status == harc::codegen::common_artifacts::WriteStatus::Written {
                        "emitted"
                    } else {
                        "reused"
                    },
                    interface_path.display(),
                );

                let runtime_unit = &plan.artifact_plan().common_units()[0];
                let runtime_artifact = plan.artifact_plan().artifact(runtime_unit.artifact_index());
                let runtime_path = outdir.join(runtime_artifact.filename());
                let runtime_status = publication
                    .write(runtime_artifact.filename(), runtime_cpp.as_bytes())
                    .map_err(|error| miette::miette!(error.to_string()))?;
                eprintln!(
                    "{} {} (common runtime)",
                    if runtime_status == harc::codegen::common_artifacts::WriteStatus::Written {
                        "emitted"
                    } else {
                        "reused"
                    },
                    runtime_path.display(),
                );

                let emit_started = Instant::now();
                let total_bytes = AtomicUsize::new(interface_cpp.len() + runtime_cpp.len());
                let delivered = harc::codegen::tbir::common::emit_common_publication_capsules(
                    &rendered,
                    jobs,
                    |capsule, cpp, elapsed| {
                        let artifact = plan.artifact_plan().artifact(capsule.artifact_index());
                        let path = outdir.join(artifact.filename());
                        let bytes = cpp.len();
                        let status = publication
                            .write(artifact.filename(), cpp.as_bytes())
                            .map_err(|error| harc::codegen::cpp_tb::EmitError(error.to_string()))?;
                        total_bytes.fetch_add(bytes, AtomicOrdering::Relaxed);
                        eprintln!(
                            "TBIR common capsule {}/{}: {}, {} test(s), {}, {}, {}",
                            capsule.index() + 1,
                            plan.capsules().len(),
                            path.display(),
                            capsule.test_bodies().len(),
                            fmt_bytes(bytes),
                            fmt_secs(elapsed),
                            if status == harc::codegen::common_artifacts::WriteStatus::Written {
                                "emitted"
                            } else {
                                "reused"
                            },
                        );
                        Ok(())
                    },
                )
                .map_err(|error| miette::miette!(error.to_string()))?;

                let registry_artifact = plan.artifact_plan().registry();
                let registry_path = outdir.join(registry_artifact.filename());
                let registry_status = publication
                    .write(registry_artifact.filename(), registry_cpp.as_bytes())
                    .map_err(|error| miette::miette!(error.to_string()))?;
                total_bytes.fetch_add(registry_cpp.len(), AtomicOrdering::Relaxed);
                eprintln!(
                    "{} {} (registry)",
                    if registry_status == harc::codegen::common_artifacts::WriteStatus::Written {
                        "emitted"
                    } else {
                        "reused"
                    },
                    registry_path.display(),
                );

                if let Some((filename, contents)) = probe_stub {
                    let path = outdir.join(&filename);
                    let status = publication
                        .write(&filename, contents.as_bytes())
                        .map_err(|error| miette::miette!(error.to_string()))?;
                    total_bytes.fetch_add(contents.len(), AtomicOrdering::Relaxed);
                    eprintln!(
                        "{} {} (probe stub)",
                        if status == harc::codegen::common_artifacts::WriteStatus::Written {
                            "emitted"
                        } else {
                            "reused"
                        },
                        path.display(),
                    );
                }

                for (filename, contents) in harc::codegen::cpp_tb::RUNTIME_HEADERS {
                    publication
                        .write(filename, contents.as_bytes())
                        .map_err(|error| miette::miette!(error.to_string()))?;
                    total_bytes.fetch_add(contents.len(), AtomicOrdering::Relaxed);
                }

                let publication = publication
                    .commit()
                    .map_err(|error| miette::miette!(error.to_string()))?;
                for stale in publication.removed() {
                    eprintln!("removed stale {}", stale.display());
                }
                eprintln!(
                    "TBIR common emit: {}/{} capsules, {}, {} rewritten, interface abi {}, profile {}, {}",
                    delivered.len(),
                    plan.capsules().len(),
                    fmt_bytes(total_bytes.load(AtomicOrdering::Relaxed)),
                    publication.rewritten_artifacts(),
                    rendered.interface_abi(),
                    plan.build_profile(),
                    fmt_secs(emit_started.elapsed()),
                );

                let _ = delivered;
                let inputs = common_manifest_inputs(
                    &outdir.join(plan.artifact_plan().manifest_filename()),
                    plan.artifact_plan(),
                    harc::codegen::common_artifacts::CodegenBackend::Tbir,
                    rendered.interface_abi(),
                    plan.build_profile(),
                    plan.placement(),
                )?;
                cpp_paths = inputs.cpp;
                probe_stub_path = inputs.probe_stub;
                native_build_identity = format!(
                    "common:tbir:{}:{}",
                    rendered.interface_abi(),
                    plan.build_profile()
                );
                runtime_headers_published = true;
            }
            // TB-IR streams: the suite is lowered and verified once, the
            // dispatcher lands before any shard work starts, and each
            // shard is written and dropped as it completes — so a long
            // emit is visible while it runs and peak memory is bounded by
            // the worker count rather than the shard count.
            CodegenKind::Tbir => {
                // Phase timings for the whole frontend, not just emission:
                // once split emission stopped being the long pole, "where
                // did the other 40 seconds go" needed an answer that did not
                // require a profiler (harc#538 goal 6, harc#546 §1b).
                eprintln!(
                    "TBIR parse: {} | merge: {}",
                    fmt_secs(parse_elapsed),
                    fmt_secs(merge_elapsed),
                );
                let lower_started = Instant::now();
                let prog = lower_tbir(&codegen_source)?;
                let lower_elapsed = lower_started.elapsed();
                uses_solver |= !prog.constraint_sites.is_empty();
                let verify_started = Instant::now();
                harc::ir::verify::verify_program(&prog).map_err(|errs| {
                    let lines: Vec<String> = errs.iter().map(|e| format!("  - {e}")).collect();
                    miette::miette!(
                        "internal error: TB-IR failed verification after lowering:\n{}",
                        lines.join("\n")
                    )
                })?;
                eprintln!(
                    "TBIR lower: {} | verify: {}",
                    fmt_secs(lower_elapsed),
                    fmt_secs(verify_started.elapsed()),
                );

                // Planning builds the suite-global scaffold every shard
                // reuses (solver problem table, randomize snippets, gated-bus
                // check), which on a large suite costs more than parsing —
                // so it gets its own number rather than hiding between the
                // lower and emit lines.
                let plan_started = Instant::now();
                let plan = harc::codegen::tbir::plan_split_tests(
                    &prog,
                    &codegen_source,
                    &emit_opts,
                    &format!("{stem}__"),
                    split.group_size,
                )
                .map_err(|e| miette::miette!("{}", e))?;
                let shard_count = plan.shards.len();
                let jobs = harc::codegen::tbir::resolve_emit_jobs(split.emit_jobs, shard_count);
                eprintln!(
                    "TBIR split plan: {} tests, {shard_count} shards, group size {}, \
                     emit jobs {jobs}, planned in {}",
                    plan.test_names.len(),
                    split.group_size,
                    fmt_secs(plan_started.elapsed()),
                );

                let dispatcher_path = outdir.join(&plan.dispatcher.filename);
                if write_if_changed(&dispatcher_path, plan.dispatcher.contents.as_bytes())? {
                    eprintln!("emitted {}", dispatcher_path.display());
                } else {
                    eprintln!("reused {} (unchanged)", dispatcher_path.display());
                }
                cpp_paths.push(dispatcher_path);

                let emit_started = Instant::now();
                // Each shard's write happens on the worker that produced
                // it, so this is only a running total, not shared state
                // guarding a critical section.
                let total_bytes = AtomicUsize::new(0);

                let delivered = harc::codegen::tbir::emit_split_shards(
                    &prog,
                    &codegen_source,
                    &emit_opts,
                    &plan,
                    jobs,
                    |shard, cpp, elapsed| {
                        let path = outdir.join(&shard.filename);
                        let bytes = cpp.len();
                        // `cpp` is dropped when this closure returns, so
                        // peak retained generated C++ stays bounded by the
                        // worker count.
                        match write_if_changed(&path, cpp.as_bytes()) {
                            Ok(changed) => {
                                total_bytes.fetch_add(bytes, AtomicOrdering::Relaxed);
                                // One `eprintln!` per shard: it locks
                                // stderr internally, so concurrent workers
                                // interleave whole lines, never partial
                                // ones. Order is completion order.
                                eprintln!(
                                    "TBIR shard {}/{shard_count}: {} tests, {}, {}, {}",
                                    shard.index + 1,
                                    shard.test_indices.len(),
                                    fmt_bytes(bytes),
                                    fmt_secs(elapsed),
                                    if changed { "emitted" } else { "reused" },
                                );
                                Ok(())
                            }
                            // Carry the OS reason into the emitter's error
                            // type: it is what comes back out of
                            // `emit_split_shards`, so anything left behind
                            // here is lost to the user.
                            Err(e) => Err(harc::codegen::cpp_tb::EmitError(format!(
                                "write {}: {e}",
                                path.display()
                            ))),
                        }
                    },
                )
                .map_err(|e| miette::miette!("{}", e))?;

                eprintln!(
                    "TBIR split emit: {}/{shard_count} shards, {}, {}",
                    delivered.len(),
                    fmt_bytes(total_bytes.load(AtomicOrdering::Relaxed)),
                    fmt_secs(emit_started.elapsed()),
                );
                // `delivered` is ascending, so the Verilator command line
                // stays byte-stable run to run even though shards complete
                // out of order (its own incremental build keys on that).
                cpp_paths.extend(
                    delivered
                        .into_iter()
                        .map(|i| outdir.join(&plan.shards[i].filename)),
                );
            }
        },
    }

    // Drop bundled runtime headers alongside the emitted .cpp so
    // verilator's standard `--Mdir`-relative include search picks it up
    // without needing an extra `-I` flag. The .cpp file `#include`s
    // them by basename. Bundled as baked-in strings via
    // `include_str!` so a binary install of `harc` ships the runtime
    // without a separate file dependency.
    if !runtime_headers_published {
        for (filename, contents) in harc::codegen::cpp_tb::RUNTIME_HEADERS {
            write_if_changed(&outdir.join(filename), contents.as_bytes())?;
        }
    }
    // `--cosim dpi`: the co-sim runtime header + the generated SV harness
    // (DUT instantiation, DPI accessor exports, timed master process).
    let cosim_harness_path = if let Some(co) = &cosim_opts {
        let cosim_rt_header_path = outdir.join("harc_cosim_rt.h");
        write_if_changed(
            &cosim_rt_header_path,
            harc::codegen::cpp_tb::COSIM_RT_HEADER.as_bytes(),
        )?;
        let top_name = top
            .clone()
            .or_else(|| harc::codegen::cpp_tb::dut_type_name(&codegen_source))
            .expect("cosim: top resolved during port discovery");
        let harness_path = outdir.join("HarcCosimTop.sv");
        write_if_changed(&harness_path, emit_cosim_harness(&top_name, co).as_bytes())?;
        Some(harness_path)
    } else {
        None
    };

    // `--emit-only` must still emit every generated source artifact a
    // downstream Verilator build needs, including probe bind stubs.
    if probe_stub_path.is_none() {
        probe_stub_path = emit_probe_stub_if_needed(&outdir, &codegen_source)?;
    }

    if emit_only {
        return Ok(());
    }

    let mut cpp_abs = Vec::with_capacity(cpp_paths.len());
    for cpp_path in &cpp_paths {
        cpp_abs.push(fs::canonicalize(cpp_path).into_diagnostic()?);
    }
    let outdir_abs = fs::canonicalize(&outdir).into_diagnostic()?;
    let sim_log_path = outdir_abs.join("sim.log");
    let trace_abs = record_trace
        .as_ref()
        .map(|p| absolutize_trace_path(p))
        .transpose()?;
    let coverage_json_abs = coverage_json
        .as_ref()
        .map(|p| absolutize_trace_path(p))
        .transpose()?;
    if let Some(path) = &coverage_json_abs {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).into_diagnostic()?;
        }
    }

    if !sv.is_empty() {
        if uses_solver {
            ensure_z3_for_solver(&z3_paths)?;
        }
        // SV / Verilator path — no `arch sim` involvement. Resolves the top
        // module name from `--top` if given, else from the HARC `let dut : T`.
        let top_name = top
            .or_else(|| harc::codegen::cpp_tb::dut_type_name(&codegen_source))
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

        // `--cosim dpi`: the generated harness joins the SV inputs and
        // becomes the Verilator top module (it instantiates the DUT).
        if let Some(harness) = &cosim_harness_path {
            sv_abs.push(fs::canonicalize(harness).into_diagnostic()?);
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
            if cosim_harness_path.is_some() {
                "HarcCosimTop"
            } else {
                &top_name
            },
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
            coverage_json_abs.as_ref(),
            &waves,
            &params,
            cosim_harness_path.is_some(),
            &native_build_identity,
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
    for param in &params {
        prefix_args.push("--param".into());
        prefix_args.push(param.clone());
    }
    prefix_args.push("--tb".into());
    let cpp_tb = cpp_abs
        .first()
        .ok_or_else(|| miette::miette!("internal error: no generated C++ testbench emitted"))?;
    prefix_args.push(cpp_tb.display().to_string());
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
    if let Some(path) = &coverage_json_abs {
        cmd.env("HARC_COVERAGE_JSONL", path);
    }
    if let Some(s) = seed {
        cmd.env("HARC_SEED", s.to_string());
    }
    // `arch sim` compiles the generated C++ testbench itself. When the TB
    // uses the runtime constraint solver it `#include`s `harc_z3_rt.h`
    // (→ `<z3++.h>`) and must link libz3 — but `arch sim` has no -I/-L
    // flag and no knowledge of Z3. It DOES split `ARCH_OPT` into args of
    // its single compile+link g++ invocation, so carry the Z3 include/lib
    // flags through `ARCH_OPT` here. Gated on `uses_solver` so non-solver
    // `--dut` runs neither require nor link Z3. (The `--sv` path supplies
    // the same flags via Verilator's `-CFLAGS`/`-LDFLAGS`.) The "-O2 -flto"
    // base mirrors `arch sim`'s own `ARCH_OPT` default so we don't drop its
    // optimization flags when `ARCH_OPT` is unset; an env-set `ARCH_OPT` is
    // preserved and extended. On GNU linkers, force `-lz3` live even though
    // `arch sim` places `ARCH_OPT` before the generated objects.
    if uses_solver {
        ensure_z3_for_solver(&z3_paths)?;
        if let (Some(inc), Some(lib)) = (&z3_paths.include_dir, &z3_paths.lib_dir) {
            let base = std::env::var("ARCH_OPT").unwrap_or_else(|_| "-O2 -flto".to_string());
            cmd.env(
                "ARCH_OPT",
                arch_opt_with_solver_z3(&base, inc, lib, cfg!(target_os = "linux")),
            );
        }
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

/// Per-backend build sub-directories for a `--check-backends` run.
///
/// The two backends MUST NOT share a build directory: the ARCH (`arch
/// sim`) backend drops a stub `V<Top>.h` plus an arch-sim compatibility
/// shim `verilated.h` / `verilated.cpp` into its outdir root, and the
/// Verilator backend puts the outdir root on its C++ `-I` include path
/// (so the emitted TB's `harc_*_rt.h` includes resolve). If they share a
/// dir, those ARCH stubs shadow Verilator's real generated header and
/// runtime — `#include "V<Top>.h"` / `#include "verilated.h"` pick up the
/// stubs and the link fails with unresolved `Verilated::s_lastContextp`,
/// `V<Top>::eval()`, etc. Returning sibling sub-dirs keeps each backend's
/// headers off the other's include path. The parent `outdir` still holds
/// the two shared trace files for the diff.
fn check_backends_subdirs(outdir: &Path) -> (PathBuf, PathBuf) {
    (outdir.join("arch_backend"), outdir.join("sv_backend"))
}

/// `harc sim --check-backends`: run the test under BOTH the ARCH native
/// sim (`--dut`) and Verilator (`--sv`) using the same seed, then diff
/// their semantic JSONL traces. Surfaces backend divergence early — the
/// regression net described in `docs/2026-05-28-backend-equivalence-gap.md`.
#[allow(clippy::too_many_arguments)]
fn cmd_sim_check_backends(
    files: Vec<PathBuf>,
    dut: Vec<PathBuf>,
    sv: Vec<PathBuf>,
    vlt: Vec<PathBuf>,
    params: Vec<String>,
    top: Option<String>,
    test: Option<String>,
    outdir: Option<PathBuf>,
    seed: Option<u64>,
    arch_bin: Option<PathBuf>,
    mt: bool,
    coverage: bool,
    ref_src: Vec<PathBuf>,
    external_build_profile_inputs: Vec<String>,
    z3_opts: Z3PathOpts,
    rebuild: bool,
    waves: WaveOpts,
    codegen: CodegenKind,
) -> Result<()> {
    if dut.is_empty() || sv.is_empty() {
        return Err(miette::miette!(
            "--check-backends requires BOTH --dut <file.arch> and --sv <file.sv>"
        ));
    }
    // Both backends emit the testbench through the SAME codegen so the diff
    // isolates DUT-backend divergence (ARCH sim vs Verilator), not emitter
    // differences. Defaults to the global default (tbir); `--codegen v1`
    // remains selectable for A/B during the v1 deprecation soak.

    // Resolve outdir up front so both runs land under the same place and the
    // two trace files sit side-by-side for the diff.
    let outdir = outdir.unwrap_or_else(|| PathBuf::from("harc_sim_build"));
    fs::create_dir_all(&outdir).into_diagnostic()?;
    let outdir_abs = fs::canonicalize(&outdir).into_diagnostic()?;
    let arch_trace = outdir_abs.join("trace_arch.jsonl");
    let sv_trace = outdir_abs.join("trace_sv.jsonl");

    // Give each backend its OWN build sub-directory so the ARCH-sim stub
    // `V<Top>.h` / `verilated.*` shim cannot shadow Verilator's real
    // generated header + runtime on the shared include path. See
    // `check_backends_subdirs` for the full failure mode.
    let (arch_outdir, sv_outdir) = check_backends_subdirs(&outdir);
    fs::create_dir_all(&arch_outdir).into_diagnostic()?;
    fs::create_dir_all(&sv_outdir).into_diagnostic()?;

    // Same seed for both runs — randomize() output must match for the
    // diff to be meaningful. Default matches cmd_sim's default.
    let resolved_seed = seed
        .or_else(|| std::env::var("HARC_SEED").ok().and_then(|v| v.parse().ok()))
        .or(Some(1));

    eprintln!("--check-backends: running ARCH (`arch sim`) backend...");
    cmd_sim(
        files.clone(),
        dut.clone(),
        Vec::new(),
        Vec::new(),
        params.clone(),
        top.clone(),
        test.clone(),
        CompileScope::Suite,
        codegen,
        SplitOpts::default(),
        Some(arch_outdir.clone()),
        resolved_seed,
        false,
        arch_bin.clone(),
        mt,
        coverage,
        ref_src.clone(),
        external_build_profile_inputs.clone(),
        z3_opts.clone(),
        rebuild,
        Some(arch_trace.clone()),
        None,
        waves.clone(),
        dut.clone(),
        sv.clone(),
        None,
    )?;

    eprintln!("--check-backends: running Verilator (`--sv`) backend...");
    cmd_sim(
        files.clone(),
        Vec::new(),
        sv.clone(),
        vlt.clone(),
        params.clone(),
        top.clone(),
        test.clone(),
        CompileScope::Suite,
        codegen,
        SplitOpts::default(),
        Some(sv_outdir.clone()),
        resolved_seed,
        false,
        arch_bin.clone(),
        mt,
        coverage,
        ref_src.clone(),
        external_build_profile_inputs,
        z3_opts.clone(),
        rebuild,
        Some(sv_trace.clone()),
        None,
        waves.clone(),
        // SV backend run: `dut` is intentionally empty (selects Verilator), but
        // the DUT `.arch` interface still supplies the port-level override so
        // BOTH backends model the same flattened bus port set.
        dut.clone(),
        Vec::new(),
        None,
    )?;

    eprintln!(
        "--check-backends: diffing {} against {}...",
        arch_trace.display(),
        sv_trace.display()
    );
    let divs = harc::check_backends::diff_traces(&arch_trace, &sv_trace)
        .map_err(|e| miette::miette!("{}", e))?;
    if divs.is_empty() {
        eprintln!("--check-backends: traces match across backends (no divergence)");
        Ok(())
    } else {
        eprintln!("--check-backends: {} divergence(s) detected:", divs.len());
        for d in &divs {
            eprintln!("  {}", d.fmt());
        }
        Err(miette::miette!(
            "backends diverge: see {} and {} for full traces",
            arch_trace.display(),
            sv_trace.display()
        ))
    }
}

fn effective_codegen(codegen: Option<CodegenKind>) -> CodegenKind {
    // Every path — including `--check-backends`, which used to force v1 —
    // now defaults to the TB-IR backend. An explicit `--codegen v1` still
    // selects v1 for A/B during the v1 deprecation soak.
    codegen.unwrap_or_default()
}

fn arch_opt_with_solver_z3(base: &str, inc: &Path, lib: &Path, force_no_as_needed: bool) -> String {
    let z3_link = if force_no_as_needed {
        "-Wl,--no-as-needed -lz3 -Wl,--as-needed"
    } else {
        "-lz3"
    };
    format!(
        "{base} -I{} -L{} -Wl,-rpath,{} {z3_link}",
        inc.display(),
        lib.display(),
        lib.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression guard for the `--check-backends` self-poisoning bug:
    /// both backends used to share one outdir, so the ARCH-sim stub
    /// `V<Top>.h` / `verilated.*` shadowed Verilator's real header +
    /// runtime on the shared `-I` include path and the link failed
    /// (unresolved `Verilated::s_lastContextp`, `V<Top>::eval()`, ...).
    /// The two backends MUST resolve to distinct sub-dirs, and both MUST
    /// live UNDER the parent outdir (where the shared trace files sit).
    #[test]
    fn check_backends_isolates_backend_outdirs() {
        let parent = PathBuf::from("/tmp/some_outdir");
        let (arch_dir, sv_dir) = check_backends_subdirs(&parent);
        assert_ne!(
            arch_dir, sv_dir,
            "the two backends must not share a build directory"
        );
        assert!(
            arch_dir.starts_with(&parent),
            "arch backend dir must live under the parent outdir"
        );
        assert!(
            sv_dir.starts_with(&parent),
            "sv backend dir must live under the parent outdir"
        );
        // Neither backend's dir is the parent itself — the parent holds
        // the shared trace_*.jsonl files and must stay free of either
        // backend's shadowing headers.
        assert_ne!(arch_dir, parent);
        assert_ne!(sv_dir, parent);
    }

    #[test]
    fn native_build_directory_reuses_only_an_exact_identity() {
        let base = temp_dir("native-build-identity");
        let obj_dir = base.join("obj_dir");
        prepare_native_build_directory(&obj_dir, false, "v1:common:abi-a:profile-a").unwrap();
        let retained = obj_dir.join("retained.o");
        fs::write(&retained, "object").unwrap();

        prepare_native_build_directory(&obj_dir, false, "v1:common:abi-a:profile-a").unwrap();
        assert!(
            retained.is_file(),
            "exact build identity should reuse objects"
        );

        prepare_native_build_directory(&obj_dir, false, "tbir:common:abi-b:profile-a").unwrap();
        assert!(
            !retained.exists(),
            "backend/layout/ABI identity changes must isolate stale objects"
        );
        assert_eq!(
            fs::read_to_string(obj_dir.join(".harc_build_identity")).unwrap(),
            "tbir:common:abi-b:profile-a\n"
        );

        let forced = obj_dir.join("forced.o");
        fs::write(&forced, "object").unwrap();
        prepare_native_build_directory(&obj_dir, true, "tbir:common:abi-b:profile-a").unwrap();
        assert!(!forced.exists(), "--rebuild must discard matching objects");
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn common_abi_identity_includes_runtime_layout_and_trace_mode() {
        let disabled = WaveOpts::default();
        assert_eq!(
            common_abi_identity_inputs(&disabled, "runtime-a"),
            vec![
                "runtime_abi=runtime-a".to_string(),
                "trace_mode=disabled".to_string(),
            ]
        );

        let vcd = WaveOpts {
            waves: true,
            format: "vcd".to_string(),
            ..WaveOpts::default()
        };
        assert_eq!(
            common_abi_identity_inputs(&vcd, "runtime-b"),
            vec![
                "runtime_abi=runtime-b".to_string(),
                "trace_mode=vcd".to_string(),
            ]
        );
    }

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
    fn check_accepts_harcwide_resize_widths_through_language_limit() {
        let src = r#"
            function wide_zext_repro(a: uint<48>, b: uint<16>) -> uint<128>
                let first_wide : uint<129> = a.zext<129>()
                let product : uint<256> = a.zext<256>() * b.zext<256>()
                let ceiling : uint<1024> = first_wide.zext<1024>()
                return ceiling.trunc<128>()
            end function wide_zext_repro
        "#;
        let path = PathBuf::from("wide_zext_repro.harc");
        validate_check_backend_codegen_limitations(&path, src).unwrap();
    }

    #[test]
    fn check_rejects_resize_above_language_limit() {
        let src = r#"
            function too_wide(a: uint<64>) -> uint<64>
                let value = a.zext<1025>()
                return value.trunc<64>()
            end function too_wide
        "#;
        let path = PathBuf::from("too_wide.harc");
        let err = validate_check_backend_codegen_limitations(&path, src).unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("`.zext<1025>()`") && msg.contains("1..=1024"),
            "expected language-limit diagnostic; got:\n{msg}"
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

    #[test]
    fn check_accepts_65_to_128_bit_resize_widths() {
        // 65..128-bit casts now flow through the `_harc_u128` model in
        // both the v1 and TB-IR C++ backends, so the check-phase gate
        // must accept them.
        let src = r#"
            function wide_ok(a: uint<64>) -> uint<128>
                let widened : uint<128> = a.zext<128>()
                let narrowed : uint<96> = widened.trunc<96>()
                return narrowed.sext<128>()
            end function wide_ok
        "#;
        let path = PathBuf::from("wide_ok.harc");
        validate_check_backend_codegen_limitations(&path, src).unwrap();
    }

    #[test]
    fn sim_cli_defaults_to_tbir_codegen() {
        let cli = Cli::parse_from(["harc", "sim", "--dut", "dut.arch", "tb.harc"]);
        let Cmd::Sim { codegen, .. } = cli.cmd else {
            panic!("expected sim command");
        };
        assert_eq!(effective_codegen(codegen), CodegenKind::Tbir);
    }

    #[test]
    fn sim_cli_check_backends_defaults_to_tbir_codegen() {
        let cli = Cli::parse_from([
            "harc",
            "sim",
            "--check-backends",
            "--dut",
            "dut.arch",
            "--sv",
            "dut.sv",
            "tb.harc",
        ]);
        let Cmd::Sim { codegen, .. } = cli.cmd else {
            panic!("expected sim command");
        };
        assert_eq!(effective_codegen(codegen), CodegenKind::Tbir);
    }

    #[test]
    fn sim_cli_keeps_explicit_v1_override() {
        let cli = Cli::parse_from([
            "harc",
            "sim",
            "--dut",
            "dut.arch",
            "--codegen",
            "v1",
            "tb.harc",
        ]);
        let Cmd::Sim { codegen, .. } = cli.cmd else {
            panic!("expected sim command");
        };
        assert_eq!(effective_codegen(codegen), CodegenKind::V1);
    }

    #[test]
    fn sim_cli_accepts_repeated_param_overrides() {
        let cli = Cli::parse_from([
            "harc",
            "sim",
            "--dut",
            "dut.arch",
            "--param",
            "CounterWidth=64",
            "--param",
            "ProvideValUpd=0",
            "tb.harc",
        ]);
        let Cmd::Sim { params, .. } = cli.cmd else {
            panic!("expected sim command");
        };
        assert_eq!(params, vec!["CounterWidth=64", "ProvideValUpd=0"]);
        validate_param_overrides(&params).unwrap();
    }

    #[test]
    fn sim_param_validation_rejects_malformed_overrides() {
        assert!(validate_param_overrides(&["WIDTH".to_string()]).is_err());
        assert!(validate_param_overrides(&["=32".to_string()]).is_err());
        assert!(validate_param_overrides(&["WIDTH=".to_string()]).is_err());
        assert!(validate_param_overrides(&["BAD NAME=1".to_string()]).is_err());
    }

    #[test]
    fn sim_cli_check_backends_honors_explicit_tbir() {
        let cli = Cli::parse_from([
            "harc",
            "sim",
            "--check-backends",
            "--dut",
            "dut.arch",
            "--sv",
            "dut.sv",
            "--codegen",
            "tbir",
            "tb.harc",
        ]);
        let Cmd::Sim { codegen, .. } = cli.cmd else {
            panic!("expected sim command");
        };
        assert_eq!(effective_codegen(codegen), CodegenKind::Tbir);
    }

    #[test]
    fn arch_opt_solver_flags_force_live_z3_when_requested() {
        let inc = Path::new("/opt/z3/include");
        let lib = Path::new("/opt/z3/lib");
        let flags = arch_opt_with_solver_z3("-O2 -flto", inc, lib, true);
        assert!(flags.contains("-Wl,--no-as-needed -lz3 -Wl,--as-needed"));
        assert!(flags.contains("-I/opt/z3/include"));
        assert!(flags.contains("-L/opt/z3/lib"));
        assert!(flags.contains("-Wl,-rpath,/opt/z3/lib"));
    }

    #[test]
    fn arch_opt_solver_flags_keep_plain_link_on_non_gnu_path() {
        let inc = Path::new("/opt/z3/include");
        let lib = Path::new("/opt/z3/lib");
        let flags = arch_opt_with_solver_z3("-O2 -flto", inc, lib, false);
        assert!(flags.ends_with(" -lz3"));
        assert!(!flags.contains("--no-as-needed"));
    }
}
