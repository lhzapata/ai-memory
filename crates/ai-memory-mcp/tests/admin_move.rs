//! Integration tests for `POST /admin/move-project`.
//!
//! Follows the same pattern as `admin_rename.rs` / `admin_purge.rs`: build
//! a real [`AdminState`] over a tmpdir-backed store + wiki, drive the
//! router with `tower::ServiceExt::oneshot`.
//!
//! move-project copies every latest page of the source project into the
//! destination workspace through the normal write path, then purges the
//! source. Sessions/observations/handoffs are NOT migrated (only pages).

use ai_memory_core::{PagePath, Tier};
use ai_memory_mcp::{AdminState, admin_router};
use ai_memory_store::{DecayParams, Store};
use ai_memory_wiki::{Wiki, WritePageRequest};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tempfile::TempDir;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn make_state(tmp: &TempDir) -> (AdminState, Store) {
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let db_path = store.db_path().to_path_buf();
    let state = AdminState {
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        embedder: None,
        provider_health: ai_memory_llm::ProviderHealth::default(),
        decay_params: DecayParams::default(),
        data_dir: tmp.path().to_path_buf(),
        db_path,
        bind: "127.0.0.1:0".to_string(),
        bootstrap_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        token_pepper: None,
    };
    (state, store)
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

async fn post(state: AdminState, uri: &str, body: serde_json::Value) -> axum::response::Response {
    let router = admin_router(state);
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    router.oneshot(req).await.unwrap()
}

/// Seed `<ws>/<project>/<path>` with one page carrying `body`.
async fn seed_page(store: &Store, wiki: &Wiki, ws: &str, project: &str, path: &str, body: &str) {
    let ws_id = store.writer.get_or_create_workspace(ws).await.unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws_id, project, None)
        .await
        .unwrap();
    wiki.write_page(WritePageRequest {
        workspace_id: ws_id,
        project_id: proj,
        path: PagePath::new(path.to_string()).unwrap(),
        frontmatter: serde_json::json!({"title": path}),
        body: body.to_string(),
        tier: Tier::Semantic,
        pinned: false,
        title: Some(path.into()),
        author_id: None,
        actor: ai_memory_core::ActorContext::anonymous(),
        admission_ctx: None,
    })
    .await
    .unwrap();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Happy path with a FRESH destination (no same-named project there): this is
/// a lossless TRUE MOVE — the project_id is re-stamped into `dst`, nothing is
/// purged, and the on-disk dir is renamed. Assert the pages now live under
/// `dst/proj`, the source name no longer resolves, and the dir moved.
#[tokio::test]
async fn move_project_true_move_into_fresh_dest() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    seed_page(
        &store,
        &state.wiki,
        "src",
        "proj",
        "decisions/0001.md",
        "decision body",
    )
    .await;
    seed_page(
        &store,
        &state.wiki,
        "src",
        "proj",
        "gotchas/x.md",
        "see [[decisions/0001]] for the call",
    )
    .await;

    // Capture the source on-disk dir before `state` is consumed by `post`.
    let src_ws = store
        .reader
        .find_workspace("src".to_string())
        .await
        .unwrap()
        .expect("src workspace exists");
    let src_proj = store
        .reader
        .find_project(src_ws, "proj".to_string())
        .await
        .unwrap()
        .expect("src project exists");
    let src_dir = state.wiki.project_root(src_ws, src_proj);
    assert!(src_dir.exists(), "source dir must exist before move");

    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "move must succeed");

    let body = body_json(resp).await;
    assert_eq!(body["pages_copied"].as_u64().unwrap_or(0), 2, "{body}");
    assert_eq!(body["moved_via"], "true-move", "{body}");
    // Nothing is purged in a true move — the rows are re-stamped, not copied.
    assert_eq!(body["source_purged"], false, "{body}");
    assert_eq!(body["merged_into_existing"], false, "{body}");

    // project_id is preserved: the dst project IS the former src project.
    let dst_ws_check = store
        .reader
        .find_workspace("dst".to_string())
        .await
        .unwrap()
        .unwrap();
    let dst_proj_check = store
        .reader
        .find_project(dst_ws_check, "proj".to_string())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        dst_proj_check, src_proj,
        "true move keeps the same project_id"
    );

    // Both pages now belong to dst/proj (latest), content preserved.
    let dst_pages = store.reader.list_pages("dst", "proj").await.unwrap();
    let mut dst_paths: Vec<String> = dst_pages.into_iter().map(|p| p.path).collect();
    dst_paths.sort();
    assert_eq!(dst_paths, vec!["decisions/0001.md", "gotchas/x.md"]);

    let dst_ws = store
        .reader
        .find_workspace("dst".to_string())
        .await
        .unwrap()
        .expect("dst workspace exists");
    let dst_proj = store
        .reader
        .find_project(dst_ws, "proj".to_string())
        .await
        .unwrap()
        .expect("dst project exists");
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let read = wiki
        .read_page(
            dst_ws,
            dst_proj,
            &PagePath::new("decisions/0001.md".to_string()).unwrap(),
        )
        .unwrap();
    assert!(
        read.body.contains("decision body"),
        "moved body must round-trip; got {:?}",
        read.body
    );

    // Source project row and on-disk dir are gone.
    assert!(
        store
            .reader
            .find_project(src_ws, "proj".to_string())
            .await
            .unwrap()
            .is_none(),
        "source project row must be gone"
    );
    assert!(!src_dir.exists(), "source dir must be removed after move");
}

/// Without `confirm: true` the server returns 400 and leaves the source intact.
#[tokio::test]
async fn move_project_requires_confirm() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_page(&store, &state.wiki, "src", "proj", "notes/a.md", "body a").await;

    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": false }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Source untouched.
    let src_ws = store
        .reader
        .find_workspace("src".to_string())
        .await
        .unwrap()
        .unwrap();
    assert!(
        store
            .reader
            .find_project(src_ws, "proj".to_string())
            .await
            .unwrap()
            .is_some(),
        "source project must still exist after a rejected move"
    );
}

/// A move from a nonexistent source project returns 404.
#[tokio::test]
async fn move_project_404_on_unknown_source() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_state(&tmp).await;

    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "nope", "project": "ghost", "to_workspace": "dst", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Moving into a workspace that already has a same-named project MERGES:
/// the destination ends up with both the pre-existing and the moved pages.
#[tokio::test]
async fn move_project_merges_into_existing_dest() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_page(&store, &state.wiki, "src", "proj", "notes/a.md", "body a").await;
    seed_page(&store, &state.wiki, "dst", "proj", "notes/b.md", "body b").await;

    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    assert_eq!(body["merged_into_existing"], true, "{body}");
    // A same-named project in the dest forces copy+purge (can't re-stamp two
    // project_ids into one).
    assert_eq!(body["moved_via"], "copy-purge", "{body}");
    assert_eq!(body["source_purged"], true, "{body}");

    // Destination holds BOTH the pre-existing and the moved page.
    let mut dst_paths: Vec<String> = store
        .reader
        .list_pages("dst", "proj")
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.path)
        .collect();
    dst_paths.sort();
    assert_eq!(dst_paths, vec!["notes/a.md", "notes/b.md"]);

    // Source gone.
    let src_ws = store
        .reader
        .find_workspace("src".to_string())
        .await
        .unwrap()
        .unwrap();
    assert!(
        store
            .reader
            .find_project(src_ws, "proj".to_string())
            .await
            .unwrap()
            .is_none(),
        "source project must be purged after merge-move"
    );
}

/// A same-workspace move is rejected with 422 (use rename-project instead).
#[tokio::test]
async fn move_project_same_workspace_rejected() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_state(&tmp).await;

    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "w", "project": "proj", "to_workspace": "w", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// The move carries the source page's existing embedding over instead of
/// recomputing it. Proven by overwriting the source embedding with a
/// recognisable marker vector: if the move re-embedded, the destination would
/// hold the synthetic bag-of-words vector, not the marker.
#[tokio::test]
async fn move_project_carries_source_embedding() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let embedder: std::sync::Arc<dyn ai_memory_llm::Embedder> =
        std::sync::Arc::new(ai_memory_llm::SyntheticEmbedder::new(8));
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_embedder(embedder.clone());
    let db_path = store.db_path().to_path_buf();
    let state = AdminState {
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        embedder: Some(embedder),
        provider_health: ai_memory_llm::ProviderHealth::default(),
        decay_params: DecayParams::default(),
        data_dir: tmp.path().to_path_buf(),
        db_path,
        bind: "127.0.0.1:0".to_string(),
        bootstrap_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        token_pepper: None,
    };

    // Seed source page (gets a synthetic embedding on write).
    seed_page(
        &store,
        &state.wiki,
        "src",
        "proj",
        "notes/a.md",
        "hello world embed",
    )
    .await;

    let src_ws = store
        .reader
        .find_workspace("src".to_string())
        .await
        .unwrap()
        .unwrap();
    let src_proj = store
        .reader
        .find_project(src_ws, "proj".to_string())
        .await
        .unwrap()
        .unwrap();
    let src = store
        .reader
        .load_embeddings(
            src_ws,
            src_proj,
            "synthetic".to_string(),
            "bag-of-words-v1".to_string(),
            8,
        )
        .await
        .unwrap();
    assert_eq!(src.len(), 1, "source page must have an embedding to carry");

    // Overwrite with a recognisable marker vector.
    let marker: Vec<f32> = vec![9.0, 8.0, 7.0, 6.0, 5.0, 4.0, 3.0, 2.0];
    store
        .writer
        .store_embedding(
            src[0].id,
            ai_memory_store::f32_vec_to_bytes(&marker),
            "synthetic".to_string(),
            "bag-of-words-v1".to_string(),
            8,
        )
        .await
        .unwrap();

    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // The destination page must carry the MARKER (not a recomputed vector).
    let dst_ws = store
        .reader
        .find_workspace("dst".to_string())
        .await
        .unwrap()
        .unwrap();
    let dst_proj = store
        .reader
        .find_project(dst_ws, "proj".to_string())
        .await
        .unwrap()
        .unwrap();
    let dst = store
        .reader
        .load_embeddings(
            dst_ws,
            dst_proj,
            "synthetic".to_string(),
            "bag-of-words-v1".to_string(),
            8,
        )
        .await
        .unwrap();
    assert_eq!(dst.len(), 1, "dest page must carry an embedding");
    for (got, want) in dst[0].vector.iter().zip(marker.iter()) {
        assert!(
            (got - want).abs() < 1e-4,
            "carried embedding must equal the source marker (not a recompute); got {:?}",
            dst[0].vector
        );
    }
}

/// The whole point of the true move over copy+purge: a session and its
/// observation — episodic rows that copy+purge would DROP — survive a move
/// into a fresh destination, re-stamped to the new workspace.
#[tokio::test]
async fn move_project_true_move_preserves_sessions_and_observations() {
    use ai_memory_core::{AgentKind, NewObservation, NewSession, ObservationKind, SessionId};

    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_page(&store, &state.wiki, "src", "proj", "notes/a.md", "body a").await;

    let src_ws = store
        .reader
        .find_workspace("src".to_string())
        .await
        .unwrap()
        .unwrap();
    let src_proj = store
        .reader
        .find_project(src_ws, "proj".to_string())
        .await
        .unwrap()
        .unwrap();

    // Seed an episodic session + observation under src/proj.
    let sid = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: sid,
            workspace_id: src_ws,
            project_id: src_proj,
            agent_kind: AgentKind::ClaudeCode,
            cwd: None,
        })
        .await
        .unwrap();
    store
        .writer
        .insert_observation(NewObservation {
            session_id: sid,
            workspace_id: src_ws,
            project_id: src_proj,
            kind: ObservationKind::UserPrompt,
            extension: None,
            source_event: None,
            title: "prompt".into(),
            body: "do the thing".into(),
            importance: 5,
        })
        .await
        .unwrap();

    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["moved_via"], "true-move", "{body}");
    assert_eq!(body["source_purged"], false, "{body}");

    // The session followed the project to the new workspace (same session_id,
    // same project_id, new workspace_id).
    let dst_ws = store
        .reader
        .find_workspace("dst".to_string())
        .await
        .unwrap()
        .unwrap();
    let (sess_ws, sess_proj) = store
        .reader
        .session_project_ids(sid)
        .await
        .unwrap()
        .expect("session must still exist after the move");
    assert_eq!(sess_ws, dst_ws, "session re-stamped to dst workspace");
    assert_eq!(sess_proj, src_proj, "session keeps its project_id");

    // The observation survived and re-stamped too.
    let obs = store.reader.observations_for_session(sid).await.unwrap();
    assert_eq!(obs.len(), 1, "observation must survive the move");
    assert_eq!(obs[0].workspace_id, dst_ws, "observation re-stamped to dst");
}
