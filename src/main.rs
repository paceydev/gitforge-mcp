use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{ArgMatches, CommandFactory, FromArgMatches, Parser, Subcommand};
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use tracing_subscriber::EnvFilter;

use gitforge_mcp::auth;
use gitforge_mcp::cache::Cache;
use gitforge_mcp::config::{CONFIG_ENV, Config};
use gitforge_mcp::config_report::{self, Given, Process};
use gitforge_mcp::context::{Options, Status};
use gitforge_mcp::remote;
use gitforge_mcp::secret::Secret;
use gitforge_mcp::server::GitforgeServer;

/// An MCP server for CI, pull requests and repository settings on Gitea,
/// Forgejo and GitHub. Without a subcommand it serves MCP over stdio.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    target: Target,
}

#[derive(clap::Args, Clone)]
struct Target {
    /// The repository to bind to: any directory inside its checkout.
    #[arg(long, global = true, env = "GITFORGE_MCP_DIR", default_value = ".")]
    dir: PathBuf,
    /// The git remote that names the forge. Defaults to `origin`, or the
    /// only remote if there is one.
    #[arg(long, global = true, env = "GITFORGE_MCP_REMOTE")]
    remote: Option<String>,
    /// The config file. Defaults to ~/.config/gitforge-mcp/config.toml
    /// (%APPDATA%\gitforge-mcp\config.toml on Windows).
    #[arg(long, global = true, env = CONFIG_ENV)]
    config: Option<PathBuf>,
    /// Offer no tool or action that changes anything on the forge.
    #[arg(long, global = true, env = "GITFORGE_MCP_READ_ONLY")]
    read_only: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Serve MCP over stdio (the default).
    Serve,
    /// Print what forge_context would report for this repository, as JSON.
    Context,
    /// Run one tool and print its result as JSON, without an agent:
    /// `gitforge-mcp call ci_status '{"run": 12}'`.
    Call {
        tool: String,
        /// The tool's arguments as a JSON object.
        #[arg(default_value = "{}")]
        args: String,
    },
    /// Wait for CI on a commit to finish, printing a progress line to
    /// stderr whenever it moves on, then the result as JSON on stdout. Meant
    /// to run in the background, so it doesn't hold anything up. Exit code:
    /// 0 CI passed, 1 it failed, 2 not finished in time or no runs, 3
    /// couldn't check, 64 the command line was wrong.
    Wait {
        /// A commit sha, full or abbreviated. Defaults to the checkout's HEAD.
        #[arg(long, conflicts_with_all = ["branch", "run"])]
        sha: Option<String>,
        /// A branch on the forge: its latest commit.
        #[arg(long, conflicts_with = "run")]
        branch: Option<String>,
        /// A run number, as in the forge's run URLs (on GitHub, the run id).
        #[arg(long)]
        run: Option<u64>,
        /// Give up after this many seconds (at most 1800).
        #[arg(long, default_value_t = 1800)]
        timeout: u64,
        /// Keep waiting for the other jobs after one fails.
        #[arg(long)]
        no_fail_fast: bool,
        /// Which jobs the result lists: attention (failed, cancelled and
        /// unfinished ones), all, or none.
        #[arg(long, default_value = "attention", value_parser = ["attention", "all", "none"])]
        jobs: String,
    },
    /// How well run-duration estimates would have done on a workflow's
    /// history, as JSON.
    #[command(hide = true)]
    Backtest {
        /// Defaults to the workflow of the latest run on the branch.
        #[arg(long)]
        workflow: Option<String>,
        #[arg(long, default_value = "push")]
        event: String,
        /// Defaults to the repository's default branch.
        #[arg(long)]
        branch: Option<String>,
        /// Pages of 50 runs to read.
        #[arg(long, default_value_t = 6)]
        pages: u32,
    },
    /// Show every setting and where it came from: an option, an
    /// environment variable, the config file or a default. Reads no
    /// credential and asks the forge nothing. Exits 1 if the config file
    /// isn't valid or there's no repository to bind to.
    Config {
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Manage the token stored in the OS keyring.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
}

#[derive(Subcommand)]
enum AuthCommand {
    /// Store a token for HOST. Reads it from stdin, or prompts without echo.
    Set { host: String },
    /// Remove the stored token for HOST.
    Remove { host: String },
    /// Show which credential source this repository's host would use.
    Status,
}

#[tokio::main]
async fn main() -> ExitCode {
    // stdout carries the MCP protocol; diagnostics go to stderr only.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_env(config_report::LOG_ENV)
                .unwrap_or_else(|_| EnvFilter::new(config_report::DEFAULT_LOG)),
        )
        .init();

    let matches = match Cli::command().try_get_matches() {
        Ok(matches) => matches,
        Err(e) => return usage(&e),
    };
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(e) => return usage(&e),
    };
    let result = match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve(&cli.target).await,
        Command::Context => print_context(&cli.target).await,
        Command::Call { tool, args } => call(&cli.target, &tool, &args).await,
        Command::Wait {
            sha,
            branch,
            run,
            timeout,
            no_fail_fast,
            jobs,
        } => {
            let args = serde_json::json!({
                "sha": sha,
                "branch": branch,
                "run": run,
                "timeout_seconds": timeout,
                "fail_fast": !no_fail_fast,
                "jobs": jobs,
            });
            wait(&cli.target, args).await
        }
        Command::Backtest {
            workflow,
            event,
            branch,
            pages,
        } => backtest(&cli.target, workflow, event, branch, pages).await,
        Command::Config { json } => show_config(&cli.target, &matches, json).await,
        Command::Auth { command } => auth_command(command, &cli.target).await,
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("gitforge-mcp: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// clap's own exit for a bad command line is 2, which `wait` uses for "not
/// finished": a background reader seeing only the exit code would re-run a
/// command that can never work. --help and --version still exit 0.
fn usage(e: &clap::Error) -> ExitCode {
    let _ = e.print();
    ExitCode::from(if e.use_stderr() { USAGE_ERROR } else { 0 })
}

fn options(target: &Target) -> Options {
    Options {
        dir: target.dir.clone(),
        remote: target.remote.clone(),
        config_path: target.config.clone().or_else(Config::default_path),
        read_only: target.read_only,
    }
}

async fn serve(target: &Target) -> anyhow::Result<ExitCode> {
    let server = GitforgeServer::new(options(target), Cache::default_location());
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(ExitCode::SUCCESS)
}

async fn print_context(target: &Target) -> anyhow::Result<ExitCode> {
    let server = GitforgeServer::new(options(target), Cache::default_location());
    let report = server.report().await;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(match report.status {
        Status::Live | Status::Cached => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    })
}

async fn call(target: &Target, tool: &str, args: &str) -> anyhow::Result<ExitCode> {
    let args: serde_json::Value =
        serde_json::from_str(args).map_err(|e| anyhow::anyhow!("arguments are not JSON: {e}"))?;
    let server = GitforgeServer::new(options(target), Cache::default_location());
    match server.call_tool_directly(tool, args, None).await {
        Ok(value) => {
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(ExitCode::SUCCESS)
        }
        Err(message) => {
            eprintln!("gitforge-mcp: {message}");
            Ok(ExitCode::FAILURE)
        }
    }
}

/// A bad command line, for every subcommand: EX_USAGE from sysexits.h,
/// clear of `wait`'s 0 to 3.
const USAGE_ERROR: u8 = 64;

// `wait`'s exit codes, as the server's instructions describe them.
const WAIT_PASSED: u8 = 0;
const WAIT_FAILED: u8 = 1;
const WAIT_UNFINISHED: u8 = 2;
const WAIT_COULD_NOT_CHECK: u8 = 3;

async fn wait(target: &Target, args: serde_json::Value) -> anyhow::Result<ExitCode> {
    let server = GitforgeServer::new(options(target), Cache::default_location());
    // Print a line only when something changed, so a background reader
    // sees where CI has got to without a line per poll.
    let last = std::sync::Mutex::new(String::new());
    let report = |waited: u64,
                  progress: String|
     -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let mut last = last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *last != progress {
            eprintln!("[{:>4}s] {progress}", waited);
            *last = progress;
        }
        Box::pin(std::future::ready(()))
    };
    let result = match server
        .call_tool_directly("ci_wait", args, Some(&report))
        .await
    {
        Ok(result) => result,
        Err(message) => {
            eprintln!("gitforge-mcp: {message}");
            return Ok(ExitCode::from(WAIT_COULD_NOT_CHECK));
        }
    };
    let verdict = result["verdict"].as_str().unwrap_or_default();
    // After a pass the summary says more than the progress line: which jobs
    // were skipped, not only how many. Otherwise the progress line does:
    // where a job failed, or what is still running.
    let finished_well = matches!(verdict, "success" | "nothing_ran");
    let last_line = if finished_well { "summary" } else { "progress" };
    eprintln!(
        "{verdict}: {}",
        result[last_line].as_str().unwrap_or_default()
    );
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(ExitCode::from(match verdict {
        "success" => WAIT_PASSED,
        "failure" | "failing" | "nothing_ran" => WAIT_FAILED,
        _ => WAIT_UNFINISHED,
    }))
}

async fn backtest(
    target: &Target,
    workflow: Option<String>,
    event: String,
    branch: Option<String>,
    pages: u32,
) -> anyhow::Result<ExitCode> {
    let server = GitforgeServer::new(options(target), Cache::default_location());
    let session = server
        .session_for_cli()
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    let result = gitforge_mcp::tools::backtest(
        &session,
        workflow.as_deref(),
        &event,
        branch.as_deref(),
        pages,
    )
    .await
    .map_err(|e| anyhow::anyhow!(e.0))?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(ExitCode::SUCCESS)
}

async fn show_config(
    target: &Target,
    matches: &ArgMatches,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let process = Process {
        dir: (target.dir.clone(), Given::of(matches, "dir")),
        remote: (target.remote.clone(), Given::of(matches, "remote")),
        config: (target.config.clone(), Given::of(matches, "config")),
        read_only: (target.read_only, Given::of(matches, "read_only")),
    };
    let report = config_report::gather(&process).await;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", config_report::render(&report));
    }
    Ok(if report.problems.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

async fn auth_command(command: AuthCommand, target: &Target) -> anyhow::Result<ExitCode> {
    match command {
        AuthCommand::Set { host } => {
            let token = read_token(&host)?;
            let shown = host.clone();
            tokio::task::spawn_blocking(move || auth::keyring_set(&host, &token)).await??;
            eprintln!("Stored a token for {shown} in the OS keyring.");
        }
        AuthCommand::Remove { host } => {
            let shown = host.clone();
            if tokio::task::spawn_blocking(move || auth::keyring_delete(&host)).await?? {
                eprintln!("Removed the stored token for {shown}.");
            } else {
                eprintln!("No token was stored for {shown}.");
            }
        }
        AuthCommand::Status => {
            let opts = options(target);
            let binding = remote::bind(&opts.dir, opts.remote.as_deref()).await?;
            let config = match &opts.config_path {
                Some(path) => Config::load(path)?,
                None => Config::default(),
            };
            let host = &binding.repo.host;
            let host_config = config.host(host);
            let scheme = host_config
                .base_url
                .as_ref()
                .unwrap_or(&binding.repo.base_url)
                .scheme()
                .to_owned();
            let (resolved, tried) = auth::resolve(host, &scheme, &host_config).await;
            println!("host: {host}");
            for attempt in tried {
                println!("  {:<32} {}", attempt.source, attempt.outcome);
            }
            match resolved {
                Some(r) => println!("using: {}", r.source),
                None => {
                    println!("no credential found");
                    return Ok(ExitCode::FAILURE);
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn read_token(host: &str) -> anyhow::Result<Secret> {
    let token = if std::io::stdin().is_terminal() {
        rpassword::prompt_password(format!("Token for {host}: "))?
    } else {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        text
    };
    let token = Secret::new(token.trim());
    anyhow::ensure!(!token.is_empty(), "no token given");
    Ok(token)
}
