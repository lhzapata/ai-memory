//! `ai-memory move-project` — thin HTTP client for cross-workspace project move.

use anyhow::{Result, bail};
use serde::Serialize;

use crate::cli::MoveProjectArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

/// Request sent to `POST /admin/move-project`.
#[derive(Serialize)]
struct MoveProjectRequest {
    from_workspace: String,
    project: String,
    to_workspace: String,
    confirm: bool,
}

/// Run the `move-project` subcommand.
///
/// Resolves the source project name (auto-derived from the git repo root
/// when `--project` is omitted), requires `--confirm` before sending the
/// request (it purges the source after copying), then prints the report.
///
/// # Errors
/// Returns an error when `--confirm` is absent, the server is unreachable,
/// or the server returns a non-2xx response.
pub async fn run(config: &Config, args: MoveProjectArgs) -> Result<()> {
    let project = super::resolve_project_name(config, args.project.as_deref())?;

    if !args.confirm {
        bail!(
            "move-project copies the project's pages into the destination \
             workspace, then PURGES the source.\n\
             Re-run with --confirm to proceed:\n\n  \
             ai-memory move-project --from-workspace {} --project {} \
             --to-workspace {} --confirm",
            args.from_workspace,
            project,
            args.to_workspace,
        );
    }

    let endpoint = ServerEndpoint::from_config(config);
    let report: serde_json::Value = post_json(
        &endpoint,
        "/admin/move-project",
        &MoveProjectRequest {
            from_workspace: args.from_workspace.clone(),
            project: project.clone(),
            to_workspace: args.to_workspace.clone(),
            confirm: true,
        },
    )
    .await?;

    let copied = report["pages_copied"].as_u64().unwrap_or(0);
    let purged = report["source_purged"].as_bool().unwrap_or(false);
    let merged = report["merged_into_existing"].as_bool().unwrap_or(false);
    println!(
        "Moved {}/{} → {}/{}: {copied} pages copied{}{}.",
        args.from_workspace,
        project,
        args.to_workspace,
        project,
        if merged {
            " (merged into existing project)"
        } else {
            ""
        },
        if purged {
            ", source purged"
        } else {
            ", SOURCE LEFT INTACT (partial copy)"
        },
    );

    if let Some(skipped) = report["pages_skipped"].as_array()
        && !skipped.is_empty()
    {
        println!(
            "Warning: {} page(s) could not be read from the source and were \
             skipped; the source was NOT purged. Fix and re-run.",
            skipped.len()
        );
    }
    Ok(())
}
