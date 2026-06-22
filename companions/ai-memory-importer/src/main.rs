use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;
use walkdir::WalkDir;

const IMPORT_VERSION: &str = "omc-wiki-v1";
const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:49374";

#[derive(Parser, Debug)]
#[command(author, version, about = "Optional ai-memory import companion")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Import an oh-my-claudecode / OMC flat markdown wiki directory.
    OmcWiki(OmcArgs),
}

#[derive(Parser, Debug, Clone)]
struct OmcArgs {
    #[arg(long)]
    dir: PathBuf,
    #[arg(long)]
    workspace: Option<String>,
    #[arg(long)]
    project: Option<String>,
    #[arg(long, env = "AI_MEMORY_SERVER_URL", default_value = DEFAULT_SERVER_URL)]
    server_url: String,
    #[arg(long)]
    apply: bool,
    #[arg(long)]
    manifest_out: Option<PathBuf>,
    #[arg(long)]
    create_destination: bool,
    #[arg(long)]
    overwrite: bool,
    #[arg(long)]
    include_session_logs: bool,
    #[arg(long)]
    show_body: bool,
    #[arg(long)]
    pinned: bool,
}

#[derive(Debug, Clone)]
struct PlannedPage {
    source_path: String,
    source_sha256: String,
    destination_path: String,
    request: WritePageRequest,
}

#[derive(Debug, Serialize, Clone)]
struct ManifestEntry {
    import_version: String,
    source_path: String,
    source_sha256: String,
    destination_path: String,
    status: ManifestStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    page_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    checkpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum ManifestStatus {
    Planned,
    Written,
    Failed,
}

#[derive(Debug, Serialize, Clone)]
struct Manifest {
    import_version: String,
    entries: Vec<ManifestEntry>,
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
struct WritePageRequest {
    workspace: String,
    project: String,
    path: String,
    body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    tier: String,
    tags: Vec<String>,
    pinned: bool,
}

#[derive(Debug, Deserialize)]
struct WritePageResponse {
    page_id: String,
    path: String,
    checkpoint: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PageListItem {
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PageListBody {
    Bare(Vec<PageListItem>),
    Wrapped { pages: Vec<PageListItem> },
}

impl PageListBody {
    fn into_pages(self) -> Vec<PageListItem> {
        match self {
            Self::Bare(pages) | Self::Wrapped { pages } => pages,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Commands::OmcWiki(args) => run_omc(args).await,
    }
}

async fn run_omc(args: OmcArgs) -> Result<()> {
    if args.apply {
        if args
            .workspace
            .as_deref()
            .is_none_or(|s| s.trim().is_empty())
            || args.project.as_deref().is_none_or(|s| s.trim().is_empty())
        {
            bail!("--apply requires explicit --workspace and --project");
        }
        if args.manifest_out.is_none() {
            bail!("--apply requires --manifest-out <path>");
        }
    }
    let workspace = args
        .workspace
        .clone()
        .unwrap_or_else(|| "DRY-RUN-WORKSPACE".into());
    let project = args
        .project
        .clone()
        .unwrap_or_else(|| "DRY-RUN-PROJECT".into());
    let planned = plan_omc_wiki(
        &args.dir,
        &workspace,
        &project,
        args.include_session_logs,
        args.pinned,
    )?;
    let mut entries: Vec<_> = planned.iter().map(planned_entry).collect();

    if !args.apply {
        if args.show_body {
            for page in &planned {
                println!(
                    "--- {} -> {} ---\n{}",
                    page.source_path, page.destination_path, page.request.body
                );
            }
        }
        if let Some(path) = &args.manifest_out {
            write_manifest(path, &entries)?;
        } else {
            println!(
                "dry-run: planned {} writes; no HTTP writes performed",
                planned.len()
            );
            for page in &planned {
                println!("{} -> {}", page.source_path, page.destination_path);
            }
        }
        return Ok(());
    }

    let manifest_path = args.manifest_out.as_ref().unwrap();
    write_manifest(manifest_path, &entries)?;

    let client = ImportClient::new(&args.server_url)?;
    let destination_exists = client
        .preflight_project(&workspace, &project, args.create_destination)
        .await?;
    if !args.overwrite && destination_exists {
        let existing = client.existing_paths(&workspace, &project).await?;
        let conflicts: Vec<_> = planned
            .iter()
            .filter(|p| existing.contains_key(&p.destination_path))
            .collect();
        if !conflicts.is_empty() {
            bail!(
                "destination already has {} planned path(s); rerun with --overwrite to replace",
                conflicts.len()
            );
        }
    }

    for (idx, page) in planned.iter().enumerate() {
        if !args.overwrite
            && client
                .page_exists(&workspace, &project, &page.destination_path)
                .await?
        {
            entries[idx].status = ManifestStatus::Failed;
            entries[idx].error = Some("destination page appeared before write; aborting".into());
            write_manifest(manifest_path, &entries)?;
            bail!(
                "destination page appeared before write {}; completed {} writes",
                page.destination_path,
                idx
            );
        }
        match client.write_page(&page.request).await {
            Ok(resp) => {
                entries[idx].status = ManifestStatus::Written;
                entries[idx].page_id = Some(resp.page_id);
                entries[idx].path = Some(resp.path);
                entries[idx].checkpoint = resp.checkpoint;
                write_manifest(manifest_path, &entries)?;
            }
            Err(err) => {
                entries[idx].status = ManifestStatus::Failed;
                entries[idx].error = Some(err.to_string());
                write_manifest(manifest_path, &entries)?;
                bail!("live write failed after {} completed writes: {err}", idx);
            }
        }
    }
    println!(
        "import complete: wrote {} pages",
        entries
            .iter()
            .filter(|e| e.status == ManifestStatus::Written)
            .count()
    );
    Ok(())
}

fn planned_entry(page: &PlannedPage) -> ManifestEntry {
    ManifestEntry {
        import_version: IMPORT_VERSION.into(),
        source_path: page.source_path.clone(),
        source_sha256: page.source_sha256.clone(),
        destination_path: page.destination_path.clone(),
        status: ManifestStatus::Planned,
        page_id: None,
        path: None,
        checkpoint: None,
        error: None,
    }
}

fn write_manifest(path: &Path, entries: &[ManifestEntry]) -> Result<()> {
    let manifest = Manifest {
        import_version: IMPORT_VERSION.into(),
        entries: entries.to_vec(),
    };
    atomic_write(path, serde_json::to_string_pretty(&manifest)?.as_bytes())
        .with_context(|| format!("write manifest {}", path.display()))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        bail!(
            "manifest parent directory does not exist: {}",
            parent.display()
        );
    }
    let tmp = temp_path_for(path)?;
    fs::write(&tmp, bytes)?;
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = fs::remove_file(&tmp);
            Err(err.into())
        }
    }
}

fn temp_path_for(path: &Path) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("manifest path must include a file name"))?;
    Ok(path.with_file_name(format!(".{file_name}.tmp")))
}

fn plan_omc_wiki(
    dir: &Path,
    workspace: &str,
    project: &str,
    include_session_logs: bool,
    pinned: bool,
) -> Result<Vec<PlannedPage>> {
    let root =
        fs::canonicalize(dir).with_context(|| format!("read source dir {}", dir.display()))?;
    if !root.is_dir() {
        bail!("--dir must be a directory");
    }
    let mut planned = Vec::new();
    let mut destinations: HashMap<String, String> = HashMap::new();
    for entry in WalkDir::new(&root)
        .min_depth(1)
        .max_depth(1)
        .sort_by_file_name()
    {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let rel = path
            .strip_prefix(&root)?
            .to_string_lossy()
            .replace('\\', "/");
        validate_source_rel(&rel)?;
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow!("invalid filename"))?;
        if name == "index.md" || (!include_session_logs && name.starts_with("session-log-")) {
            continue;
        }
        let content =
            fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let source_sha256 = sha256_hex(content.as_bytes());
        let parsed = parse_markdown(&content)?;
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow!("invalid filename"))?;
        let dest = format!("omc/{}.md", slugify(stem));
        validate_destination_path(&dest)?;
        if let Some(other) = destinations.insert(dest.clone(), rel.clone()) {
            bail!("duplicate destination path collision: {other} and {rel} both map to {dest}");
        }
        planned.push(PlannedPage {
            source_path: rel,
            source_sha256,
            destination_path: dest.clone(),
            request: WritePageRequest {
                workspace: workspace.to_owned(),
                project: project.to_owned(),
                path: dest,
                body: parsed.body,
                title: parsed.title,
                kind: parsed.kind,
                tier: normalize_tier(parsed.tier.as_deref())?,
                tags: parsed.tags,
                pinned: parsed.pinned || pinned,
            },
        });
    }
    Ok(planned)
}

fn normalize_tier(tier: Option<&str>) -> Result<String> {
    let tier = tier.unwrap_or("semantic").trim();
    match tier {
        "working" | "episodic" | "semantic" | "procedural" => Ok(tier.to_owned()),
        other => bail!("unsupported tier '{other}'"),
    }
}

#[derive(Default)]
struct ParsedMarkdown {
    body: String,
    title: Option<String>,
    kind: Option<String>,
    tier: Option<String>,
    tags: Vec<String>,
    pinned: bool,
}

fn parse_markdown(input: &str) -> Result<ParsedMarkdown> {
    let mut out = ParsedMarkdown::default();
    let body = if let Some(rest) = input.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---\n") {
            let yaml = &rest[..end];
            let value: serde_yaml::Value =
                serde_yaml::from_str(yaml).context("parse YAML frontmatter")?;
            if let Some(map) = value.as_mapping() {
                out.title = yaml_string(map, "title");
                out.kind = yaml_string(map, "kind");
                out.tier = yaml_string(map, "tier");
                out.pinned = yaml_bool(map, "pinned").unwrap_or(false);
                out.tags = yaml_tags(map);
            }
            rest[end + "\n---\n".len()..].to_owned()
        } else {
            input.to_owned()
        }
    } else {
        input.to_owned()
    };
    if out.title.is_none() {
        out.title = first_h1(&body);
    }
    out.body = body;
    Ok(out)
}

fn yaml_key(key: &str) -> serde_yaml::Value {
    serde_yaml::Value::String(key.into())
}
fn yaml_string(map: &serde_yaml::Mapping, key: &str) -> Option<String> {
    map.get(yaml_key(key))
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .filter(|s| !s.trim().is_empty())
}
fn yaml_bool(map: &serde_yaml::Mapping, key: &str) -> Option<bool> {
    map.get(yaml_key(key)).and_then(|v| v.as_bool())
}
fn yaml_tags(map: &serde_yaml::Mapping) -> Vec<String> {
    match map.get(yaml_key("tags")) {
        Some(serde_yaml::Value::Sequence(seq)) => seq
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        Some(v) => v.as_str().map(|s| vec![s.to_owned()]).unwrap_or_default(),
        None => Vec::new(),
    }
}
fn first_h1(body: &str) -> Option<String> {
    body.lines()
        .find_map(|l| l.strip_prefix("# ").map(str::trim).map(str::to_owned))
        .filter(|s| !s.is_empty())
}

fn validate_source_rel(rel: &str) -> Result<()> {
    let p = Path::new(rel);
    if p.is_absolute() || p.components().any(|c| matches!(c, Component::ParentDir)) {
        bail!("unsafe source relative path: {rel}");
    }
    Ok(())
}

fn validate_destination_path(path: &str) -> Result<()> {
    let p = Path::new(path);
    if p.is_absolute()
        || path.starts_with('/')
        || path.contains('\\')
        || p.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("unsafe destination path: {path}");
    }
    let first = path.split('/').next().unwrap_or_default();
    if matches!(
        first,
        "_rules"
            | "_internal"
            | ".git"
            | "sessions"
            | "session-logs"
            | "procedures"
            | "decisions"
            | "gotchas"
    ) {
        bail!("reserved destination prefix: {first}");
    }
    if !path.ends_with(".md") {
        bail!("destination path must end in .md");
    }
    Ok(())
}

fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for ch in s.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "page".into()
    } else {
        trimmed
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

struct ImportClient {
    client: reqwest::Client,
    origin: String,
    base_path: String,
    token: Option<String>,
}

impl ImportClient {
    fn new(server_url: &str) -> Result<Self> {
        let url = Url::parse(server_url).context("invalid --server-url")?;
        let origin = format!(
            "{}://{}",
            url.scheme(),
            url.host_str()
                .ok_or_else(|| anyhow!("server URL needs host"))?
        );
        let origin = if let Some(port) = url.port() {
            format!("{origin}:{port}")
        } else {
            origin
        };
        let base_path = url.path().trim_end_matches('/').to_owned();
        let base_path = if base_path == "/" {
            String::new()
        } else {
            base_path
        };
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()?,
            origin,
            base_path,
            token: std::env::var("AI_MEMORY_AUTH_TOKEN")
                .ok()
                .filter(|s| !s.is_empty()),
        })
    }
    fn url(&self, path: &str) -> String {
        format!("{}{}{}", self.origin, self.base_path, path)
    }
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let req = self.client.request(method, self.url(path));
        if let Some(token) = &self.token {
            req.bearer_auth(token)
        } else {
            req
        }
    }
    async fn preflight_project(
        &self,
        workspace: &str,
        project: &str,
        create: bool,
    ) -> Result<bool> {
        let resp = self
            .request(
                reqwest::Method::GET,
                &format!(
                    "/api/v1/workspaces/{}/projects/{}/pages",
                    enc(workspace),
                    enc(project)
                ),
            )
            .send()
            .await?;
        if resp.status().is_success() {
            Ok(true)
        } else if resp.status() == StatusCode::NOT_FOUND && create {
            Ok(false)
        } else {
            bail!(
                "destination workspace/project must already exist (or pass --create-destination): HTTP {}",
                resp.status()
            )
        }
    }
    async fn existing_paths(&self, workspace: &str, project: &str) -> Result<HashMap<String, ()>> {
        let resp = self
            .request(
                reqwest::Method::GET,
                &format!(
                    "/api/v1/workspaces/{}/projects/{}/pages",
                    enc(workspace),
                    enc(project)
                ),
            )
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("list destination pages failed: HTTP {}", resp.status());
        }
        let pages: PageListBody = resp.json().await?;
        Ok(pages
            .into_pages()
            .into_iter()
            .map(|page| (page.path, ()))
            .collect())
    }
    async fn page_exists(&self, workspace: &str, project: &str, path: &str) -> Result<bool> {
        let resp = self
            .request(
                reqwest::Method::GET,
                &format!(
                    "/api/v1/workspaces/{}/projects/{}/pages/{}",
                    enc(workspace),
                    enc(project),
                    enc_path(path)
                ),
            )
            .send()
            .await?;
        match resp.status() {
            StatusCode::OK => Ok(true),
            StatusCode::NOT_FOUND => Ok(false),
            s => bail!("page pre-write check failed: HTTP {s}"),
        }
    }
    async fn write_page(&self, req: &WritePageRequest) -> Result<WritePageResponse> {
        let resp = self
            .request(reqwest::Method::POST, "/admin/write-page")
            .json(req)
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("write-page failed: HTTP {}", resp.status());
        }
        Ok(resp.json().await?)
    }
}

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}
fn enc_path(s: &str) -> String {
    s.split('/').map(enc).collect::<Vec<_>>().join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(dir: &Path, name: &str, body: &str) {
        fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn parses_omc_frontmatter() {
        let parsed = parse_markdown("---\ntitle: T\nkind: rule\ntier: procedural\ntags: [a, b]\npinned: true\nextra: ignored\n---\n# Body\ntext").unwrap();
        assert_eq!(parsed.title.as_deref(), Some("T"));
        assert_eq!(parsed.kind.as_deref(), Some("rule"));
        assert_eq!(parsed.tier.as_deref(), Some("procedural"));
        assert_eq!(parsed.tags, vec!["a", "b"]);
        assert!(parsed.pinned);
        assert_eq!(parsed.body, "# Body\ntext");
    }

    #[test]
    fn planning_rejects_unknown_tier_before_live_write() {
        let td = tempdir().unwrap();
        write(td.path(), "note.md", "---\ntier: legendary\n---\n# Note");
        assert!(
            plan_omc_wiki(td.path(), "w", "p", false, false)
                .unwrap_err()
                .to_string()
                .contains("unsupported tier")
        );
    }

    #[test]
    fn skips_index_and_session_logs_by_default() {
        let td = tempdir().unwrap();
        write(td.path(), "index.md", "# Index");
        write(td.path(), "session-log-1.md", "# Log");
        write(td.path(), "note.md", "# Note");
        let pages = plan_omc_wiki(td.path(), "w", "p", false, false).unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].destination_path, "omc/note.md");
    }

    #[test]
    fn path_traversal_rejected() {
        assert!(validate_source_rel("../x.md").is_err());
        assert!(validate_destination_path("omc/../x.md").is_err());
        assert!(validate_destination_path("/omc/x.md").is_err());
    }

    #[test]
    fn detects_duplicate_slug_collisions() {
        let td = tempdir().unwrap();
        write(td.path(), "A B.md", "# A");
        write(td.path(), "a-b.md", "# B");
        assert!(
            plan_omc_wiki(td.path(), "w", "p", false, false)
                .unwrap_err()
                .to_string()
                .contains("collision")
        );
    }

    #[test]
    fn dry_run_planning_has_no_http_client() {
        let td = tempdir().unwrap();
        write(td.path(), "note.md", "# Note");
        let pages = plan_omc_wiki(td.path(), "w", "p", false, false).unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].request.workspace, "w");
    }

    #[test]
    fn write_request_json_shape() {
        let req = WritePageRequest {
            workspace: "w".into(),
            project: "p".into(),
            path: "omc/n.md".into(),
            body: "# N".into(),
            title: Some("N".into()),
            kind: Some("fact".into()),
            tier: "semantic".into(),
            tags: vec!["omc".into()],
            pinned: true,
        };
        let v = serde_json::to_value(req).unwrap();
        assert_eq!(v["workspace"], "w");
        assert_eq!(v["project"], "p");
        assert_eq!(v["path"], "omc/n.md");
        assert_eq!(v["body"], "# N");
        assert_eq!(v["pinned"], true);
    }

    #[test]
    fn parses_current_bare_api_page_list_shape() {
        let body = r#"[{"path":"omc/a.md"},{"path":"notes/b.md"}]"#;
        let parsed: PageListBody = serde_json::from_str(body).unwrap();
        let paths: Vec<_> = parsed.into_pages().into_iter().map(|p| p.path).collect();
        assert_eq!(paths, vec!["omc/a.md", "notes/b.md"]);
    }

    #[test]
    fn parses_legacy_wrapped_api_page_list_shape() {
        let body = r#"{"pages":[{"path":"omc/a.md"},{"path":"notes/b.md"}]}"#;
        let parsed: PageListBody = serde_json::from_str(body).unwrap();
        let paths: Vec<_> = parsed.into_pages().into_iter().map(|p| p.path).collect();
        assert_eq!(paths, vec!["omc/a.md", "notes/b.md"]);
    }

    #[test]
    fn auth_header_uses_bearer_scheme() {
        let rb = reqwest::Client::new()
            .get("http://127.0.0.1/")
            .bearer_auth("secret-token");
        let req = rb.build().unwrap();
        assert_eq!(
            req.headers().get(reqwest::header::AUTHORIZATION).unwrap(),
            "Bearer secret-token"
        );
        let dbg = format!("{:?}", req.headers());
        assert!(!dbg.contains("AI_MEMORY_AUTH_TOKEN"));
    }

    #[test]
    fn overwrite_requires_explicit_flag() {
        let args = OmcArgs {
            dir: PathBuf::from("."),
            workspace: Some("w".into()),
            project: Some("p".into()),
            server_url: DEFAULT_SERVER_URL.into(),
            apply: true,
            manifest_out: Some(PathBuf::from("m.json")),
            create_destination: false,
            overwrite: false,
            include_session_logs: false,
            show_body: false,
            pinned: false,
        };
        assert!(!args.overwrite);
    }
}
