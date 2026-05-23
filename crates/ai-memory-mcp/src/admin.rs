//! Admin HTTP routes — state-touching operations invoked by the CLI
//! over plain HTTP (not MCP). Currently exposes:
//!
//! - `POST /admin/bootstrap` — ingest a pre-collected source bundle
//!   into seed wiki pages via the configured LLM provider.
//! - `GET  /admin/status`    — lifetime counts + server data-dir info.
//! - `GET  /admin/search?q=` — FTS5 hits against the wiki index.
//!
//! The CLI is responsible for filesystem access (collecting sources from
//! the project repo, rendering output for humans); the server is
//! responsible for all state reads/writes against the wiki + SQLite.

use std::sync::Arc;

use std::path::PathBuf;

use ai_memory_consolidate::{
    Bootstrap, BootstrapConfig, BootstrapOutcome, BootstrapSource, SourceCounts,
};
use ai_memory_llm::LlmProvider;
use ai_memory_store::{ReaderPool, WriterHandle};
use ai_memory_wiki::Wiki;
use axum::Json;
use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};

/// Shared state for the admin router.
#[derive(Clone)]
pub struct AdminState {
    /// Writer actor handle — used to get-or-create workspace/project.
    pub writer: WriterHandle,
    /// Reader pool — used by the idempotency check inside Bootstrap.
    pub reader: ReaderPool,
    /// Wiki handle — pages are written here.
    pub wiki: Wiki,
    /// Optional LLM provider. When `None`, bootstrap returns 503.
    pub llm: Option<Arc<dyn LlmProvider>>,
    /// Server's resolved data directory (e.g. `/data` in the docker
    /// image). Surfaced via `/admin/status` so the CLI can report
    /// "where the wiki + db actually live".
    pub data_dir: PathBuf,
    /// Resolved SQLite path inside `data_dir`. Same purpose as above.
    pub db_path: PathBuf,
    /// Server's bind address — informational, surfaced in /admin/status.
    pub bind: String,
}

/// JSON request body for `POST /admin/bootstrap`.
#[derive(Deserialize)]
struct BootstrapRequest {
    /// Workspace name (auto-created if it doesn't exist).
    workspace: String,
    /// Project name (auto-created if it doesn't exist).
    project: String,
    /// Sources pre-collected on the client side.
    sources: Vec<BootstrapSource>,
    /// Maximum input tokens for LLM call.
    #[serde(default = "default_max_input_tokens")]
    max_input_tokens: usize,
    /// Skip the LLM call and page writes — returns a dry-run outcome.
    #[serde(default)]
    dry_run: bool,
    /// Allow re-bootstrap when `wiki/bootstrap.md` already exists.
    #[serde(default)]
    force: bool,
}

fn default_max_input_tokens() -> usize {
    50_000
}

/// Build the admin axum [`Router`]. Mounts:
/// - `POST /admin/bootstrap`
/// - `GET  /admin/status`
/// - `GET  /admin/search`
pub fn admin_router(state: AdminState) -> Router {
    Router::new()
        .route("/admin/bootstrap", post(handle_bootstrap))
        .route("/admin/status", get(handle_status))
        .route("/admin/search", get(handle_search))
        .with_state(Arc::new(state))
}

// ---------------------------------------------------------------------
// status
// ---------------------------------------------------------------------

/// JSON response body for `GET /admin/status`. The CLI's `status`
/// subcommand renders this either as JSON (`--json`) or as a small
/// human-friendly text block.
#[derive(Debug, Serialize)]
pub struct StatusReport {
    /// Server binary version (Cargo package version).
    pub version: String,
    /// Absolute data directory the server uses (server-side path).
    pub data_dir: String,
    /// Bind address the HTTP transport is listening on.
    pub bind: String,
    /// Absolute SQLite path inside `data_dir`.
    pub db_path: String,
    /// Lifetime counts: pages_latest, pages_all, sessions, observations.
    pub counts: ai_memory_store::StatusCounts,
}

async fn handle_status(State(state): State<Arc<AdminState>>) -> impl IntoResponse {
    match state.reader.status_counts().await {
        Ok(counts) => {
            let report = StatusReport {
                version: env!("CARGO_PKG_VERSION").to_string(),
                data_dir: state.data_dir.display().to_string(),
                bind: state.bind.clone(),
                db_path: state.db_path.display().to_string(),
                counts,
            };
            (
                StatusCode::OK,
                Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

// ---------------------------------------------------------------------
// search
// ---------------------------------------------------------------------

/// Query string for `GET /admin/search?q=…&limit=…`.
#[derive(Debug, Deserialize)]
struct SearchQuery {
    /// FTS5 query expression.
    q: String,
    /// Max number of hits to return. Capped at 100 server-side.
    #[serde(default = "default_search_limit")]
    limit: usize,
}

fn default_search_limit() -> usize {
    10
}

async fn handle_search(
    State(state): State<Arc<AdminState>>,
    Query(query): Query<SearchQuery>,
) -> impl IntoResponse {
    let limit = query.limit.clamp(1, 100);
    match state.reader.search_pages(query.q, limit).await {
        Ok(hits) => (
            StatusCode::OK,
            Json(serde_json::to_value(&hits).unwrap_or_else(|_| serde_json::json!([]))),
        ),
        Err(e) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

// ---------------------------------------------------------------------
// bootstrap
// ---------------------------------------------------------------------

async fn handle_bootstrap(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<BootstrapRequest>,
) -> impl IntoResponse {
    // LLM is required only for live runs. Dry-runs never call the LLM
    // so we handle them directly here without constructing Bootstrap.
    if !req.dry_run && state.llm.is_none() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "LLM provider not configured on server"
            })),
        );
    }

    // Resolve workspace + project — create if absent.
    let ws = match state.writer.get_or_create_workspace(req.workspace).await {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("workspace: {e}") })),
            );
        }
    };
    let proj = match state
        .writer
        .get_or_create_project(ws, req.project, None)
        .await
    {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("project: {e}") })),
            );
        }
    };

    // Dry-run with no LLM: compute the budget-pruned source counts and
    // return early without constructing Bootstrap (which requires an LLM).
    if req.dry_run && state.llm.is_none() {
        return dry_run_outcome(req.sources, req.max_input_tokens);
    }

    let llm = Arc::clone(
        state
            .llm
            .as_ref()
            .expect("llm is Some: checked above (non-dry-run without LLM returns 503)"),
    );

    let cfg = BootstrapConfig {
        // repo_path is unused by process_sources — the path field is
        // only consumed by collect_sources on the client side.
        repo_path: std::path::PathBuf::new(),
        workspace_id: ws,
        project_id: proj,
        max_input_tokens: req.max_input_tokens,
        // The individual include_* flags don't matter here: sources
        // are already collected; process_sources ignores them.
        include_git: true,
        include_readme: true,
        include_docs: true,
        include_code: true,
        since: None,
        dry_run: req.dry_run,
        force: req.force,
    };

    let bootstrap = Bootstrap {
        reader: state.reader.clone(),
        wiki: state.wiki.clone(),
        llm,
    };

    match bootstrap.process_sources(&cfg, req.sources).await {
        Ok(outcome) => (
            StatusCode::OK,
            Json(serde_json::to_value(&outcome).unwrap_or_else(|_| serde_json::json!({}))),
        ),
        Err(e) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

/// Build a dry-run [`BootstrapOutcome`] without an LLM by applying the
/// same budget-pruning logic that `Bootstrap::process_sources` would use.
fn dry_run_outcome(
    sources: Vec<BootstrapSource>,
    max_input_tokens: usize,
) -> (StatusCode, Json<serde_json::Value>) {
    use ai_memory_consolidate::BootstrapError;
    if sources.is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": BootstrapError::NoSources.to_string()
            })),
        );
    }
    // Mirror the prune logic: sort by drop_priority desc, drop until
    // under budget. We replicate the constants here rather than
    // exposing prune_to_budget publicly (it's an implementation detail
    // of the consolidation pipeline).
    const CHARS_PER_TOKEN: usize = 4;
    let collected = sources.len();
    let usable = max_input_tokens.saturating_sub(1_000);
    let mut sorted = sources;
    sorted.sort_by_key(|s| std::cmp::Reverse(s.kind.drop_priority()));
    let mut total: usize = sorted
        .iter()
        .map(|s| (s.label.len() + s.text.len() + 16).div_ceil(CHARS_PER_TOKEN))
        .sum();
    while total > usable && !sorted.is_empty() {
        let victim_tokens =
            (sorted[0].label.len() + sorted[0].text.len() + 16).div_ceil(CHARS_PER_TOKEN);
        total = total.saturating_sub(victim_tokens);
        sorted.remove(0);
    }
    let kept = &sorted;
    let dropped = collected - kept.len();
    let counts = SourceCounts::from_sources(kept);
    let outcome = BootstrapOutcome {
        sources_collected: collected,
        sources_sent: kept.len(),
        sources_dropped: dropped,
        sources_by_kind: counts,
        estimated_input_tokens: total,
        pages_written: Vec::new(),
        rationale: "(dry-run; LLM not invoked)".to_string(),
        dry_run: true,
    };
    (
        StatusCode::OK,
        Json(serde_json::to_value(&outcome).unwrap_or_else(|_| serde_json::json!({}))),
    )
}
