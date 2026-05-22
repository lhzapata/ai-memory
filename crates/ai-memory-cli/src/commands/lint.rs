//! `ai-memory lint` — run the M8 lint pass manually.

use ai_memory_consolidate::run_lint;
use ai_memory_llm::{build_provider, provider_from_env};
use ai_memory_store::Store;
use ai_memory_wiki::Wiki;
use anyhow::{Context, Result};

use crate::cli::LintArgs;
use crate::config::Config;

/// Run the `lint` subcommand.
///
/// # Errors
/// Returns an error if the store cannot be opened or the lint pass
/// fails.
pub async fn run(config: &Config, args: LintArgs) -> Result<()> {
    let store = Store::open(&config.data_dir)
        .with_context(|| format!("opening store at {}", config.data_dir.display()))?;
    let ws = store
        .writer
        .get_or_create_workspace(args.workspace.clone())
        .await?;
    let proj = store
        .writer
        .get_or_create_project(ws, args.project.clone(), None)
        .await?;
    let wiki = Wiki::new(&config.data_dir, store.writer.clone())?;

    // Build provider if env configured; otherwise rule-based only.
    let llm = if let Some(cfg) = provider_from_env()? {
        Some(build_provider(cfg)?)
    } else {
        None
    };

    let report = run_lint(&store.reader, &wiki, llm.as_ref(), ws, proj, args.dry_run).await?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
