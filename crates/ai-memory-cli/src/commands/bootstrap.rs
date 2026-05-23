//! `ai-memory bootstrap` — ingest an existing project's history.
//!
//! Thin HTTP client wrapper. Source collection (git log, README, docs/,
//! Rust module headers, project-rules files) happens locally via
//! `ai_memory_consolidate::collect_sources`; the resulting bundle is
//! POSTed to `POST /admin/bootstrap` on the running server, which does
//! the LLM call and wiki writes. The CLI never opens a `Store` or `Wiki`
//! directly.
//!
//! Required environment variables (see "Configuring the CLI" in README):
//! - `AI_MEMORY_SERVER_URL` — base URL of the running server.
//! - `AI_MEMORY_AUTH_TOKEN` — bearer token if the server has auth enabled.

use std::path::PathBuf;

use ai_memory_consolidate::{BootstrapOutcome, collect_sources};
use anyhow::{Context, Result, bail};
use tracing::info;

use crate::cli::BootstrapArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

/// Run the `bootstrap` subcommand.
///
/// Collects sources locally from the project repo, then POSTs the
/// bundle to the server's `POST /admin/bootstrap` endpoint.
///
/// # Errors
/// Bails when `AI_MEMORY_SERVER_URL` is unset, when the resolved repo
/// path is not a git repo, when source collection fails, or when the
/// server returns a non-2xx response.
pub async fn run(_config: &Config, args: BootstrapArgs) -> Result<()> {
    let ep = ServerEndpoint::from_env();
    info!(server = %ep.url, auth = ep.auth_token.is_some(), "bootstrap CLI configured");

    // ---- repo path — auto-detect if absent -------------------------
    let repo_path = match args.repo_path {
        Some(p) => p,
        None => resolve_repo_root().context("auto-detecting --repo-path via git rev-parse")?,
    };
    if !repo_path.join(".git").exists() {
        bail!(
            "repo path {} is not a git repository (looked for {}/.git)",
            repo_path.display(),
            repo_path.display()
        );
    }

    // ---- collect sources locally ----------------------------------
    let sources = collect_sources(
        &repo_path,
        args.since.as_deref(),
        !args.exclude_git,
        !args.exclude_readme,
        !args.exclude_docs,
        !args.exclude_code,
    )?;
    info!(sources = sources.len(), "collected sources from repo");

    // ---- POST to server -------------------------------------------
    let body = serde_json::json!({
        "workspace": args.workspace,
        "project": args.project,
        "sources": sources,
        "max_input_tokens": args.max_input_tokens,
        "dry_run": args.dry_run,
        "force": args.force,
    });
    let outcome: BootstrapOutcome = post_json(&ep, "/admin/bootstrap", &body).await?;

    print_human_report(&outcome, &args.workspace, &args.project);
    let report = serde_json::to_string_pretty(&outcome)?;
    println!("\n--- machine-readable ---\n{report}");
    Ok(())
}

/// Render the bootstrap outcome as a human-friendly summary. Lists
/// each source kind separately + every page written + an explicit
/// "what ai-memory knows now" footer so the operator doesn't assume
/// the wiki has 100% coverage of the project.
fn print_human_report(outcome: &BootstrapOutcome, workspace: &str, project: &str) {
    let kind = if outcome.dry_run {
        "Dry-run"
    } else {
        "Bootstrap"
    };
    println!("\n{kind} complete for {workspace}/{project}\n");

    println!("Sources loaded into the LLM:");
    let c = &outcome.sources_by_kind;
    if c.git_commits > 0 {
        println!(
            "  - {} git commit summar{}",
            c.git_commits,
            if c.git_commits == 1 { "y" } else { "ies" }
        );
    }
    if c.readme > 0 {
        println!("  - README");
    }
    if c.doc_files > 0 {
        println!(
            "  - {} doc file{} (under docs/)",
            c.doc_files,
            if c.doc_files == 1 { "" } else { "s" }
        );
    }
    if c.module_headers > 0 {
        println!(
            "  - {} Rust module header{}",
            c.module_headers,
            if c.module_headers == 1 { "" } else { "s" }
        );
    }
    if c.project_rules > 0 {
        println!(
            "  - {} project-rules file{} (CLAUDE.md / AGENTS.md / ...)",
            c.project_rules,
            if c.project_rules == 1 { "" } else { "s" }
        );
    }
    println!(
        "  -> ~{} input tokens estimated{}",
        outcome.estimated_input_tokens,
        if outcome.sources_dropped > 0 {
            format!(
                " (dropped {} lower-priority source{} to stay under budget)",
                outcome.sources_dropped,
                if outcome.sources_dropped == 1 {
                    ""
                } else {
                    "s"
                }
            )
        } else {
            String::new()
        }
    );

    if outcome.dry_run {
        println!("\n(dry-run -- no LLM call, no pages written)");
    } else {
        println!(
            "\nGenerated {} wiki page{}:",
            outcome.pages_written.len(),
            if outcome.pages_written.len() == 1 {
                ""
            } else {
                "s"
            }
        );
        for p in &outcome.pages_written {
            println!("  - {p}");
        }
        if !outcome.rationale.is_empty() {
            println!("\nRationale: {}", outcome.rationale);
        }
    }

    println!(
        "\nWhat ai-memory knows now\n  \
         Only the sources listed above. NOT every file in your project,\n  \
         NOT every commit since project start, NOT runtime behaviour or\n  \
         test logs. As you use Claude Code (or another MCP agent) the\n  \
         lifecycle hooks will automatically capture your actual workflow,\n  \
         and consolidation will refine the wiki over time."
    );
}

/// `git rev-parse --show-toplevel` — finds the repo root from $PWD.
fn resolve_repo_root() -> Result<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("running `git rev-parse --show-toplevel`")?;
    if !output.status.success() {
        bail!(
            "git rev-parse failed (cwd is not inside a git repository?). \
             stderr: {}",
            String::from_utf8_lossy(&output.stderr).trim(),
        );
    }
    let line = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(PathBuf::from(line))
}
