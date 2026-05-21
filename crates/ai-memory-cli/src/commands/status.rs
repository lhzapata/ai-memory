//! `ai-memory status` — report runtime config and persisted counts.

use std::path::Path;

use ai_memory_store::{StatusCounts, Store};
use anyhow::{Context, Result};
use serde::Serialize;

use crate::cli::StatusArgs;
use crate::config::Config;

#[derive(Debug, Serialize)]
struct Report<'a> {
    version: &'a str,
    data_dir: &'a Path,
    bind: &'a str,
    db_path: &'a Path,
    counts: StatusCounts,
}

/// Run the `status` subcommand.
///
/// # Errors
/// Returns an error if the store cannot be opened or JSON serialisation fails.
pub async fn run(config: &Config, args: StatusArgs) -> Result<()> {
    let store = Store::open(&config.data_dir)
        .with_context(|| format!("opening store at {}", config.data_dir.display()))?;
    let counts = store.reader.status_counts().await?;

    let report = Report {
        version: env!("CARGO_PKG_VERSION"),
        data_dir: &config.data_dir,
        bind: &config.bind,
        db_path: store.db_path(),
        counts,
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("ai-memory {}", report.version);
        println!("  data-dir:     {}", report.data_dir.display());
        println!("  db:           {}", report.db_path.display());
        println!("  bind:         {}", report.bind);
        println!(
            "  pages:        {} (all versions: {})",
            report.counts.pages_latest, report.counts.pages_all
        );
        println!("  sessions:     {}", report.counts.sessions);
        println!("  observations: {}", report.counts.observations);
    }
    Ok(())
}
