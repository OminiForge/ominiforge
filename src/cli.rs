//! CLI: command parsing and dispatch.
//!
//! The command line is the operator's entry point, not a chat front-end:
//! `ominiforge serve` runs the gateway (the single backend every interactive
//! front-end talks to), `ominiforge init` scaffolds the config files
//! (`.omini/config/providers.toml` + `.omini/profiles/*.toml`, see
//! `doc/profile.md`), `ominiforge inspect` summarizes a session offline, and
//! `ominiforge eval` runs eval suites. API keys are never stored in config: a
//! provider names an env var via `api_key_env`, and the key is read from the
//! environment. See `doc/architecture.md` §3.1, §15.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};

use crate::app::{self, DEFAULT_PROFILE, SESSIONS_SUBDIR};
use crate::config::ConfigStore;
use crate::session::SessionStore;

/// Ominiforge command-line interface.
#[derive(Debug, Parser)]
#[command(
    name = "ominiforge",
    version,
    about = "A high-performance agent platform"
)]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Directory whose `.omini/` holds config (providers, profiles, mcp, hooks).
    /// Highest-priority config root: `--config-dir` → launch cwd → `~`. Config is
    /// independent of a session's workspace (`doc/architecture.md` §15).
    #[arg(long, global = true)]
    config_dir: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print a derived metrics summary for a session (offline, from its log).
    Inspect(InspectArgs),
    /// Scaffold `.omini/` config files (providers + a default profile).
    Init(InitArgs),
    /// Run the gateway server (HTTP/SSE/WebSocket) in the foreground.
    Serve(ServeArgs),
    /// Run an eval suite (TOML case files) and persist scores.
    Eval(EvalArgs),
}

/// Arguments for `ominiforge serve`.
#[derive(Debug, Parser)]
struct ServeArgs {
    /// Workspace the gateway's sessions operate in (default: current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,

    /// Profile new sessions are created with.
    #[arg(long, default_value = DEFAULT_PROFILE)]
    profile: String,

    /// Bind address, overriding `gateway.toml` (host:port).
    #[arg(long)]
    bind: Option<String>,

    /// Do not auto-load a `.env` file; use only the existing environment.
    #[arg(long)]
    no_dotenv: bool,
}

/// Arguments for `ominiforge init`.
/// Arguments for `ominiforge inspect`.
#[derive(Debug, Parser)]
struct InitArgs {
    /// Directory to scaffold `.omini/` under (default: current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,

    /// Overwrite existing config files instead of skipping them.
    #[arg(long)]
    force: bool,
}

/// Arguments for `ominiforge inspect`.
#[derive(Debug, Parser)]
struct InspectArgs {
    /// The session id to inspect (a directory under `.omini/sessions`).
    session_id: String,

    /// Workspace whose sessions to read (default: current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,

    /// Do not auto-load a `.env` file; use only the existing environment.
    #[arg(long)]
    no_dotenv: bool,
}

/// Arguments for `ominiforge eval`.
///
/// `eval <suite_path>` runs a suite (the default, no subcommand); `eval diff`
/// and `eval report` operate on already-persisted runs (`doc/eval.md` §7). The
/// clap attributes give git-stash-style dispatch: the bare form keeps its
/// positional `suite_path`, and naming a subcommand negates that requirement.
#[derive(Debug, Parser)]
#[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
struct EvalArgs {
    /// Analysis subcommand; absent means "run the suite".
    #[command(subcommand)]
    command: Option<EvalCommand>,

    /// Arguments for the default run form (`eval <suite_path> ...`).
    #[command(flatten)]
    run: EvalRunArgs,
}

/// The analysis subcommands over persisted runs (`doc/eval.md` §6–7).
#[derive(Debug, Subcommand)]
enum EvalCommand {
    /// Diff two runs (A2) and detect regressions (A3). Exit non-zero on any
    /// pass→fail regression, so it gates CI.
    Diff(EvalDiffArgs),
    /// Print the single-run aggregate report (A1) for one run.
    Report(EvalReportArgs),
    /// Load a public Q&A dataset (JSONL) as bootstrap cases and run them
    /// (`doc/eval.md` §8.2). Read from a local path; nothing is downloaded.
    Bootstrap(EvalBootstrapArgs),
}

/// Arguments for the default `ominiforge eval <suite_path>` run form.
#[derive(Debug, Parser)]
struct EvalRunArgs {
    /// Path to eval suite directory (contains case TOML files).
    suite_path: PathBuf,

    /// Profile to run (default: coding).
    #[arg(long, default_value = "coding")]
    profile: String,

    /// Optional model override.
    #[arg(long)]
    model: Option<String>,

    /// Optional temperature override.
    #[arg(long)]
    temperature: Option<f32>,

    /// Do not auto-load a `.env` file; use only the existing environment.
    #[arg(long)]
    no_dotenv: bool,
}

/// Arguments for `ominiforge eval diff <baseline> <candidate>`.
#[derive(Debug, Parser)]
struct EvalDiffArgs {
    /// Baseline (older/reference) run id.
    baseline: String,

    /// Candidate (newer) run id being gated.
    candidate: String,
}

/// Arguments for `ominiforge eval report <run_id>`.
#[derive(Debug, Parser)]
struct EvalReportArgs {
    /// Run id to report on (a directory under `.omini/eval/runs`).
    run_id: String,
}

/// Arguments for `ominiforge eval bootstrap <dataset>`.
#[derive(Debug, Parser)]
struct EvalBootstrapArgs {
    /// Path to a local JSONL dataset (one `{input, target}` object per line).
    dataset: PathBuf,

    /// Match checker applied to every case: `exact` or `fuzzy`.
    #[arg(long, default_value = "exact")]
    checker: String,

    /// Tag added to every loaded case (for later dimension slicing).
    #[arg(long, default_value = "bootstrap")]
    tag: String,

    /// Profile to run (default: coding).
    #[arg(long, default_value = "coding")]
    profile: String,

    /// Optional model override.
    #[arg(long)]
    model: Option<String>,

    /// Optional temperature override.
    #[arg(long)]
    temperature: Option<f32>,

    /// Do not auto-load a `.env` file; use only the existing environment.
    #[arg(long)]
    no_dotenv: bool,
}

/// Parse arguments and dispatch. The binary entry point calls this.
///
/// # Errors
/// Surfaces configuration, provider, and session errors to the process exit.
pub async fn run() -> Result<()> {
    // Operator diagnostics to stderr. Business/agent events never go here —
    // they belong to `events.jsonl` (doc/architecture.md). Default to
    // `info` for our own crate; RUST_LOG overrides (e.g. `RUST_LOG=debug`).
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("ominiforge=info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let config_dir = cli.config_dir;
    match cli.command {
        None => {
            // The command line is not a chat front-end; with no subcommand,
            // show what the operator commands are.
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
        Some(Command::Inspect(args)) => inspect(config_dir.as_deref(), &args),
        Some(Command::Init(args)) => init(&args),
        Some(Command::Serve(args)) => serve_cmd(config_dir, args).await,
        Some(Command::Eval(args)) => eval_cmd(config_dir, args).await,
    }
}

/// Run the gateway server in the foreground (`doc/architecture.md` §18.1). A
/// systemd user service wraps this; for development it runs directly.
async fn serve_cmd(config_dir: Option<PathBuf>, args: ServeArgs) -> Result<()> {
    use crate::gateway::{GatewayConfig, SessionDefaults, SessionRegistry, serve};

    let workspace = match args.workspace {
        Some(path) => path,
        None => std::env::current_dir().context("cannot determine current directory")?,
    };
    let workspace = app::resolve_workspace(&workspace)?;

    // Config roots come from --config-dir / launch cwd / home — NOT the
    // workspace (`doc/architecture.md` §15). Launch cwd is the directory the
    // server was started in.
    let launch_cwd = std::env::current_dir().context("cannot determine current directory")?;
    let config_store = ConfigStore::discover_with(config_dir.as_deref(), &launch_cwd);
    if !args.no_dotenv {
        app::load_dotenv(config_store.roots(), &workspace);
    }

    let mut gateway_config =
        GatewayConfig::load(config_store.roots()).context("failed to load gateway.toml")?;
    if let Some(bind) = args.bind {
        gateway_config.bind = bind;
    }

    let authenticated = gateway_config.resolve_api_key().is_some();
    tracing::info!(bind = %gateway_config.bind, "ominiforge gateway listening");
    if authenticated {
        tracing::info!(
            "auth: bearer token required (from ${})",
            gateway_config.api_key_env.as_deref().unwrap_or("?")
        );
    } else {
        tracing::warn!(
            "auth: DISABLED — no api_key_env configured. Only safe behind \
             loopback + a trusted reverse proxy (doc/architecture.md §18)."
        );
    }

    let defaults = SessionDefaults {
        config: config_store,
        workspace,
        profile: args.profile,
        no_dotenv: args.no_dotenv,
    };
    let registry = SessionRegistry::new(defaults, &gateway_config)?;
    serve(registry, &gateway_config).await
}

/// Dispatch `ominiforge eval`: no subcommand runs a suite; `diff`/`report`
/// operate on already-persisted runs (`doc/eval.md` §6–7).
async fn eval_cmd(config_dir: Option<PathBuf>, args: EvalArgs) -> Result<()> {
    match args.command {
        None => eval_run(config_dir, args.run).await,
        Some(EvalCommand::Diff(diff_args)) => eval_diff(&diff_args),
        Some(EvalCommand::Report(report_args)) => eval_report(&report_args),
        Some(EvalCommand::Bootstrap(bootstrap_args)) => {
            eval_bootstrap(config_dir, bootstrap_args).await
        }
    }
}

/// Resolve a run id to its directory under `.omini/eval/runs/<run_id>/`, rooted
/// at the launch cwd (where the session data lives).
fn eval_run_dir(run_id: &str) -> Result<PathBuf> {
    let launch_cwd = std::env::current_dir().context("cannot determine current directory")?;
    Ok(launch_cwd
        .join(".omini")
        .join("eval")
        .join("runs")
        .join(run_id))
}

/// The default form: load a suite, run its approved cases, persist scores +
/// manifest, and gate on the aggregate (`doc/eval.md` §4, §7). Exits non-zero if
/// any case failed.
async fn eval_run(config_dir: Option<PathBuf>, args: EvalRunArgs) -> Result<()> {
    use crate::eval::case::load_suite;

    let launch_cwd = std::env::current_dir().context("cannot determine current directory")?;
    let config = ConfigStore::discover_with(config_dir.as_deref(), &launch_cwd);

    // Load suite (only approved cases by default).
    let all_cases = load_suite(&args.suite_path)
        .with_context(|| format!("failed to load suite: {}", args.suite_path.display()))?;
    let cases: Vec<_> = all_cases
        .into_iter()
        .filter(|c| matches!(c.status, crate::eval::CaseStatus::Approved))
        .collect();

    if cases.is_empty() {
        eprintln!(
            "eval: no approved cases found in {}",
            args.suite_path.display()
        );
        return Ok(());
    }

    run_and_persist(
        &config,
        &cases,
        &args.suite_path.display().to_string(),
        &args.profile,
        args.model.as_deref(),
        args.temperature,
    )
    .await
}

/// Run a set of cases through the full scorer stack, persist scores + manifest
/// under `.omini/eval/runs/<run_id>/`, and gate on the aggregate. Shared by the
/// suite runner (`eval_run`) and the bootstrap loader (`eval_bootstrap`) so both
/// use one scorer set and one persistence path. Exits non-zero if any case
/// failed (`doc/eval.md` §7).
async fn run_and_persist(
    config: &ConfigStore,
    cases: &[crate::eval::EvalCase],
    suite_label: &str,
    profile: &str,
    model: Option<&str>,
    temperature: Option<f32>,
) -> Result<()> {
    use std::sync::Arc;

    use crate::eval::analysis::RunReport;
    use crate::eval::runner::RunConfig;
    use crate::eval::{
        ExactMatch, FuzzyMatch, NoToolError, Scorer, TestsPass, TurnCompleted, WorkspaceDiff,
    };

    eprintln!("eval: running {} case(s) from {suite_label}", cases.len());

    let scorers: Vec<Arc<dyn Scorer>> = vec![
        Arc::new(TurnCompleted),
        Arc::new(NoToolError),
        Arc::new(ExactMatch),
        Arc::new(FuzzyMatch),
        Arc::new(TestsPass),
        Arc::new(WorkspaceDiff),
    ];

    let run_config = RunConfig {
        config,
        profile,
        model,
        temperature,
        scorers: &scorers,
    };

    // Run cases sequentially and collect results.
    let results = run_eval_cases(cases, &run_config).await;

    // Flatten to persisted score rows, then aggregate (A1).
    let rows = to_score_rows(&results);
    let report = RunReport::from_rows(&rows);

    // Persist run to .omini/eval/runs/<run_id>/
    let run_id = ulid::Ulid::new().to_string();
    let run_dir = eval_run_dir(&run_id)?;
    std::fs::create_dir_all(&run_dir)
        .with_context(|| format!("failed to create run dir: {}", run_dir.display()))?;

    persist_run(
        &run_dir,
        &rows,
        &manifest_json(&run_id, suite_label, profile, &report),
    )?;

    eprintln!(
        "\neval: {}/{} passed, {} skipped — run_id: {run_id}\n  manifest: {}",
        report.passed,
        report.passed + report.failed,
        report.skipped,
        run_dir.join("manifest.json").display()
    );

    if !report.all_passed() {
        std::process::exit(1);
    }

    Ok(())
}

/// The `bootstrap` form: load a public Q&A dataset (JSONL) into bootstrap cases
/// (`doc/eval.md` §8.2) and run them through the same scorer + persistence path
/// as a normal suite. Entries needing an attachment are reported and skipped.
async fn eval_bootstrap(config_dir: Option<PathBuf>, args: EvalBootstrapArgs) -> Result<()> {
    use crate::eval::bootstrap::{MatchKind, load_bootstrap};

    let launch_cwd = std::env::current_dir().context("cannot determine current directory")?;
    let config = ConfigStore::discover_with(config_dir.as_deref(), &launch_cwd);

    let match_kind = MatchKind::parse(&args.checker, &args.dataset)?;
    let load = load_bootstrap(&args.dataset, match_kind, &args.tag).with_context(|| {
        format!(
            "failed to load bootstrap dataset: {}",
            args.dataset.display()
        )
    })?;

    if !load.skipped.is_empty() {
        eprintln!("eval bootstrap: skipped {} entr(ies):", load.skipped.len());
        for reason in &load.skipped {
            eprintln!("  {reason}");
        }
    }

    if load.cases.is_empty() {
        eprintln!(
            "eval bootstrap: no runnable cases in {}",
            args.dataset.display()
        );
        return Ok(());
    }

    run_and_persist(
        &config,
        &load.cases,
        &format!("bootstrap:{}", args.dataset.display()),
        &args.profile,
        args.model.as_deref(),
        args.temperature,
    )
    .await
}

/// Print the single-run aggregate report (A1) for one persisted run.
fn eval_report(args: &EvalReportArgs) -> Result<()> {
    use crate::eval::analysis::{RunReport, load_scores};

    let run_dir = eval_run_dir(&args.run_id)?;
    let rows = load_scores(&run_dir)
        .with_context(|| format!("failed to load scores for run `{}`", args.run_id))?;
    let report = RunReport::from_rows(&rows);

    println!("run {}", args.run_id);
    println!(
        "  cases:     {} passed, {} failed, {} skipped",
        report.passed, report.failed, report.skipped
    );
    println!("  pass rate: {:.1}%", report.pass_rate * 100.0);
    if !report.scorers.is_empty() {
        println!("  per scorer:");
        for (name, tally) in &report.scorers {
            println!(
                "    {name:<16} pass {} fail {} partial {} skip {}",
                tally.pass, tally.fail, tally.partial, tally.skip
            );
        }
    }
    Ok(())
}

/// Diff two persisted runs (A2) and gate on regressions (A3). Exits non-zero
/// when any case regressed (pass→fail).
fn eval_diff(args: &EvalDiffArgs) -> Result<()> {
    use crate::eval::analysis::{RunDiff, load_scores};

    let baseline_dir = eval_run_dir(&args.baseline)?;
    let candidate_dir = eval_run_dir(&args.candidate)?;
    let baseline = load_scores(&baseline_dir)
        .with_context(|| format!("failed to load baseline run `{}`", args.baseline))?;
    let candidate = load_scores(&candidate_dir)
        .with_context(|| format!("failed to load candidate run `{}`", args.candidate))?;

    let diff = RunDiff::from_rows(&baseline, &candidate);

    println!("diff {} -> {}", args.baseline, args.candidate);
    print_case_list("regressions (pass->fail)", &diff.regressions);
    print_case_list("fixes (fail->pass)", &diff.fixes);
    print_case_list("added", &diff.added);
    print_case_list("removed", &diff.removed);
    for change in &diff.other_changes {
        println!(
            "  changed: {} {} -> {}",
            change.case_id,
            change.from.label(),
            change.to.label()
        );
    }

    if diff.has_regression() {
        eprintln!(
            "\neval diff: {} regression(s) — gate FAILED",
            diff.regressions.len()
        );
        std::process::exit(1);
    }
    eprintln!("\neval diff: no regressions — gate passed");
    Ok(())
}

/// Print a labeled list of case ids, omitting the section when empty.
fn print_case_list(label: &str, ids: &[String]) {
    if ids.is_empty() {
        return;
    }
    println!("  {label}:");
    for id in ids {
        println!("    {id}");
    }
}

/// Flatten case results into the persisted flat row form (`doc/eval.md` §5.3):
/// one row per (case, scorer), sharing the case's wall-clock duration.
fn to_score_rows(
    results: &[crate::eval::runner::CaseResult],
) -> Vec<crate::eval::analysis::ScoreRow> {
    use crate::eval::analysis::ScoreRow;

    let mut rows = Vec::new();
    for result in results {
        for (scorer_name, score) in &result.scores {
            rows.push(ScoreRow::new(
                &result.case_id,
                &result.session_id,
                scorer_name,
                score,
                result.duration_ms,
            ));
        }
    }
    rows
}

/// Build the run manifest (`doc/eval.md` §5.2) from the run metadata and
/// aggregate.
fn manifest_json(
    run_id: &str,
    suite_label: &str,
    profile: &str,
    report: &crate::eval::analysis::RunReport,
) -> serde_json::Value {
    use chrono::Utc;
    use serde_json::json;

    json!({
        "run_id": run_id,
        "created_at": Utc::now().to_rfc3339(),
        "suite": suite_label,
        "profile": profile,
        "total_cases": report.passed + report.failed + report.skipped,
        "passed": report.passed,
        "failed": report.failed,
        "skipped": report.skipped,
        "pass_rate": report.pass_rate,
    })
}

async fn run_eval_cases(
    cases: &[crate::eval::EvalCase],
    run_config: &crate::eval::runner::RunConfig<'_>,
) -> Vec<crate::eval::runner::CaseResult> {
    use crate::eval::runner::run_case;

    let mut results = Vec::with_capacity(cases.len());
    for case in cases {
        eprint!("  {} ... ", case.id);
        match run_case(case, run_config).await {
            Ok(result) => {
                let pass = result
                    .scores
                    .values()
                    .filter(|s| matches!(s.value, crate::eval::ScoreValue::Pass))
                    .count();
                let fail = result
                    .scores
                    .values()
                    .filter(|s| matches!(s.value, crate::eval::ScoreValue::Fail))
                    .count();
                if fail == 0 {
                    eprintln!("PASS ({pass} scorer(s))");
                } else {
                    eprintln!("FAIL ({fail} failed, {pass} passed)");
                }
                results.push(result);
            }
            Err(e) => {
                eprintln!("ERROR: {e:#}");
            }
        }
    }
    results
}

/// Write `scores.jsonl` (one [`ScoreRow`](crate::eval::analysis::ScoreRow) per
/// line) and `manifest.json` into `run_dir`. The rows share their type with the
/// analysis-layer reader ([`load_scores`](crate::eval::analysis::load_scores)),
/// so writer and reader can never drift.
fn persist_run(
    run_dir: &std::path::Path,
    rows: &[crate::eval::analysis::ScoreRow],
    manifest: &serde_json::Value,
) -> Result<()> {
    use std::io::Write;

    let manifest_path = run_dir.join("manifest.json");
    std::fs::write(&manifest_path, serde_json::to_string_pretty(manifest)?)
        .context("failed to write manifest.json")?;

    let scores_path = run_dir.join("scores.jsonl");
    let mut scores_file =
        std::fs::File::create(&scores_path).context("failed to create scores.jsonl")?;

    for row in rows {
        writeln!(scores_file, "{}", serde_json::to_string(row)?)
            .context("failed to write scores.jsonl")?;
    }

    Ok(())
}

/// Print a derived metrics summary for a session, computed offline by replaying
/// its `events.jsonl` through the monitor (`doc/monitor.md` §8).
fn inspect(config_dir: Option<&Path>, args: &InspectArgs) -> Result<()> {
    let requested = match args.workspace.clone() {
        Some(path) => path,
        None => std::env::current_dir().context("cannot determine current directory")?,
    };
    let workspace = app::resolve_workspace(&requested)?;
    // Config comes from --config-dir / launch cwd / home, independent of the
    // session's workspace (`doc/architecture.md` §15).
    let launch_cwd = std::env::current_dir().context("cannot determine current directory")?;
    let config = ConfigStore::discover_with(config_dir, &launch_cwd);
    if !args.no_dotenv {
        app::load_dotenv(config.roots(), &workspace);
    }

    let store = SessionStore::new(workspace.join(SESSIONS_SUBDIR));
    let sid = crate::core::SessionId(args.session_id.clone());
    let events = store
        .read_events(&sid)
        .with_context(|| format!("failed to read session `{}`", args.session_id))?;

    let summary = crate::monitor::summarize(&events);
    print_summary(&args.session_id, &summary);
    Ok(())
}

/// Render a [`SessionSummary`](crate::monitor::SessionSummary) to stdout.
fn print_summary(session_id: &str, s: &crate::monitor::SessionSummary) {
    println!("session {session_id}");
    println!("  turns:          {}", s.total_turns);
    println!("  model requests: {}", s.total_model_requests);
    println!(
        "  tool calls:     {} ({} failed)",
        s.total_tool_calls, s.total_tool_failures
    );
    println!(
        "  tokens:         {} in / {} out",
        s.total_input_tokens, s.total_output_tokens
    );
    println!(
        "  cache hit rate: {:.1}% ({} read tokens)",
        s.cache_hit_rate * 100.0,
        s.total_cache_read_tokens
    );
    println!("  context:        {} tokens (est.)", s.context_tokens);
    if !s.tools_used.is_empty() {
        let mut tools: Vec<_> = s.tools_used.iter().collect();
        tools.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        let rendered: Vec<String> = tools.iter().map(|(n, c)| format!("{n}×{c}")).collect();
        println!("  tools used:     {}", rendered.join(", "));
    }
    if !s.errors.is_empty() {
        let mut errors: Vec<_> = s.errors.iter().collect();
        errors.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        let rendered: Vec<String> = errors.iter().map(|(c, n)| format!("{c}×{n}")).collect();
        println!("  errors:         {}", rendered.join(", "));
    }
}

/// Scaffold `.omini/config/providers.toml` and `.omini/profiles/default.toml`.
fn init(args: &InitArgs) -> Result<()> {
    let base = match &args.workspace {
        Some(path) => path.clone(),
        None => std::env::current_dir().context("cannot determine current directory")?,
    };
    let omini = base.join(".omini");
    let config_dir = omini.join("config");
    let profiles_dir = omini.join("profiles");
    std::fs::create_dir_all(&config_dir)
        .with_context(|| format!("failed to create {}", config_dir.display()))?;
    std::fs::create_dir_all(&profiles_dir)
        .with_context(|| format!("failed to create {}", profiles_dir.display()))?;

    write_scaffold(
        &config_dir.join("providers.toml"),
        PROVIDERS_TEMPLATE,
        args.force,
    )?;
    write_scaffold(
        &profiles_dir.join("default.toml"),
        PROFILE_TEMPLATE,
        args.force,
    )?;

    write_scaffold(
        &config_dir.join("gateway.toml"),
        GATEWAY_TEMPLATE,
        args.force,
    )?;

    eprintln!(
        "scaffolded {}\n  edit config/providers.toml, set the api_key_env vars, then:\n  ominiforge serve",
        omini.display()
    );
    Ok(())
}

/// Write `contents` to `path`, skipping (unless `force`) if it already exists.
fn write_scaffold(path: &Path, contents: &str, force: bool) -> Result<()> {
    if path.exists() && !force {
        eprintln!("skip (exists): {}", path.display());
        return Ok(());
    }
    std::fs::write(path, contents)
        .with_context(|| format!("failed to write {}", path.display()))?;
    eprintln!("wrote: {}", path.display());
    Ok(())
}

/// Starter `providers.toml`. Keys are referenced by env-var name, never inlined.
const PROVIDERS_TEMPLATE: &str = r#"# Provider + model definitions. See doc/profile.md §2.
# API keys are NOT stored here: `api_key_env` names an environment variable
# that holds the key (set it in your shell or a git-ignored .env file).

[[providers]]
name = "openai-main"
type = "openai-chat"                  # openai-chat is the only wired type today
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"

[[providers.models]]
id = "gpt-4o"
context_window = 128000
max_output_tokens = 16384
default_temperature = 0.0

# Any OpenAI-compatible endpoint works (local servers, third parties, Xiaomi
# MiMo via an OpenAI-shaped gateway, ...). Example:
#
# [[providers]]
# name = "xiaomi-local"
# type = "openai-chat"
# base_url = "http://localhost:8080/v1"
# api_key_env = "XIAOMI_MIMO_API_KEY"
#
# [[providers.models]]
# id = "mimo-7b"
# context_window = 32000
# max_output_tokens = 8192
# default_temperature = 0.7
"#;

/// Starter `default.toml` profile. Points at the example provider/model above.
const PROFILE_TEMPLATE: &str = r#"# The default agent profile. See doc/profile.md §3.

[profile]
name = "default"
description = "Default agent profile"

[prompt]
system = """
You are Ominiforge, a capable software agent. Use the available tools to
accomplish the user's task, and explain what you did.
"""

[model]
default = "openai-main/gpt-4o"        # provider_name/model_id

[tools]
builtin = ["read", "write", "edit", "shell"]
"#;

/// Starter `gateway.toml`. Auth is off until you uncomment `api_key_env` and set
/// the named env var; the gateway binds loopback so it is only reachable behind a
/// reverse proxy. See doc/gateway.md.
const GATEWAY_TEMPLATE: &str = r#"# Gateway server config. See doc/gateway.md.
# The gateway is the backend every interactive front-end (Web/desktop/mobile)
# talks to; the command line itself is operator tooling, not a chat front-end.

bind = "127.0.0.1:7878"            # loopback 示例；默认见 gateway/config.rs DEFAULT_BIND，反向代理终结 TLS

# Bearer-token auth. Uncomment and set the named env var to require a token on
# every route except /healthz. Left unset, the gateway is UNAUTHENTICATED —
# only safe behind loopback + a trusted reverse proxy.
# api_key_env = "OMINI_GATEWAY_KEY"

idle_timeout_secs = 1800           # evict an idle session actor after 30 min
"#;
