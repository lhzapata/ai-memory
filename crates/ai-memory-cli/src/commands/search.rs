//! `ai-memory search` — run an FTS5 query against the wiki index.

use ai_memory_store::Store;
use anyhow::{Context, Result};

use crate::cli::SearchArgs;
use crate::config::Config;

/// Run the `search` subcommand.
///
/// # Errors
/// Returns an error if the store cannot be opened, the query is malformed,
/// or JSON serialisation fails.
pub async fn run(config: &Config, args: SearchArgs) -> Result<()> {
    let store = Store::open(&config.data_dir)
        .with_context(|| format!("opening store at {}", config.data_dir.display()))?;
    let hits = store
        .reader
        .search_pages(args.query.clone(), args.limit)
        .await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
    } else if hits.is_empty() {
        println!("no results for {:?}", args.query);
    } else {
        println!("{} result(s) for {:?}:", hits.len(), args.query);
        for hit in &hits {
            println!("  {}  rank={:.4}", hit.path, hit.rank);
            println!("    {}", hit.title);
            println!("    {}", hit.snippet);
        }
    }
    Ok(())
}
