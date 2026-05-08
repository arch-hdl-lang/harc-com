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
    },
    // Future, mirroring ARCH:
    //   Build  — transpile to SystemVerilog + UVM (spec §10.2, phase 5)
    //   Formal — emit BTOR2 / SMT-LIB2 (spec §10.3, phase 4)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Check { files, ast } => cmd_check(files, ast),
        Cmd::Fmt { file, write } => cmd_fmt(file, write),
        Cmd::Sim { files, dut, sv, top, test, outdir, seed, emit_only, arch_bin } =>
            cmd_sim(files, dut, sv, top, test, outdir, seed, emit_only, arch_bin),
    }
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
        "--top-module".into(), top.into(),
        "--Mdir".into(), mdir.display().to_string(),
    ];
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
    let merged = harc::codegen::merge::merge_for_sim(&parsed_files, test.as_deref())
        .map_err(|e| miette::miette!("{}", e))?;

    let cpp = harc::codegen::cpp_tb::emit(&merged)
        .map_err(|e| miette::miette!("{}", e))?;

    let outdir = outdir.unwrap_or_else(|| PathBuf::from("harc_sim_build"));
    fs::create_dir_all(&outdir).into_diagnostic()?;
    let stem = files[0].file_stem().and_then(|s| s.to_str()).unwrap_or("harc_tb");
    let cpp_path = outdir.join(format!("{stem}.cpp"));
    fs::write(&cpp_path, &cpp).into_diagnostic()?;
    eprintln!("emitted {}", cpp_path.display());

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
        return run_verilator(&top_name, &sv_abs, &cpp_abs, &outdir_abs, &sim_log_path, seed);
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
