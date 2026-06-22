# ai-memory-importer

Standalone optional companion for importing external memory corpora into a
running ai-memory server. This crate is deliberately isolated from the root
workspace: its `Cargo.toml` has its own `[workspace]`, uses only crates.io
dependencies, and is not included in root `cargo test --workspace`.

## Supported source: OMC wiki directory

The first importer supports oh-my-claudecode / OMC flat Markdown wiki
directories. It reads only top-level `*.md` files, skips `index.md` and
`session-log-*` by default, and writes deterministic destination paths under
`omc/<slug>.md`.

Claude memory graph and Qdrant imports are roadmap items only; there are no code
stubs for them in v1.

## Safety contract

- Default mode is dry-run; live mode requires `--apply`.
- Live mode requires explicit `--workspace`, `--project`, and
  `--manifest-out <path>`.
- Live writes use only `POST /admin/write-page`; the importer never opens
  ai-memory SQLite or wiki files directly and never deletes pages.
- The destination workspace/project must already exist unless
  `--create-destination` is passed.
- Existing destination pages abort the import unless `--overwrite` is passed.
  The importer also re-checks each page immediately before writing.
  This is best-effort protection: a concurrent writer could still race between
  the check and `/admin/write-page`, so avoid running competing import/write jobs
  into the same destination.
- It stops on the first live-write error and updates the manifest with completed
  writes and the failed checkpoint.
- Path handling fails closed: absolute paths, `..`, unsafe destination paths,
  and reserved/internal destination prefixes are rejected. Duplicate generated
  destination paths abort planning.
- Dry-run output does not print full page bodies unless `--show-body` is passed.
- Only endpoint-supported metadata is mapped: `title`, `kind`, `tier`, `tags`,
  `pinned`, and `body`. Unknown frontmatter is ignored.
- Auth comes only from `AI_MEMORY_AUTH_TOKEN`; there is intentionally no CLI
  token argument.

## Usage

Dry-run with a summary:

```bash
cargo run --manifest-path companions/ai-memory-importer/Cargo.toml -- \
  omc-wiki --dir /path/to/omc/wiki --workspace default --project my-project
```

Dry-run with a manifest:

```bash
cargo run --manifest-path companions/ai-memory-importer/Cargo.toml -- \
  omc-wiki --dir /path/to/omc/wiki --workspace default --project my-project \
  --manifest-out /tmp/omc-import-manifest.json
```

Live import:

```bash
AI_MEMORY_AUTH_TOKEN=... \
cargo run --manifest-path companions/ai-memory-importer/Cargo.toml -- \
  omc-wiki --dir /path/to/omc/wiki --workspace default --project my-project \
  --apply --manifest-out /tmp/omc-import-manifest.json
```

Options:

- `--server-url URL`: ai-memory server URL; defaults to
  `http://127.0.0.1:49374`, or `AI_MEMORY_SERVER_URL` when set. A URL path is
  treated as the base path.
- `--create-destination`: allow `/admin/write-page` to auto-create the
  workspace/project after the read preflight fails.
- `--overwrite`: replace existing destination pages.
- `--include-session-logs`: include `session-log-*` pages.
- `--show-body`: print full page bodies during dry-run.
- `--pinned`: pin all imported pages.

## Validation

Run these from the repository root:

```bash
cargo fmt --check --manifest-path companions/ai-memory-importer/Cargo.toml
cargo test --manifest-path companions/ai-memory-importer/Cargo.toml
cargo clippy --manifest-path companions/ai-memory-importer/Cargo.toml --all-targets -- -D warnings
```

Root hygiene checks remain separate:

```bash
cargo fmt --check
git diff --check
```

## Roadmap

- Claude Code memory graph export import.
- Qdrant collection import with user-supplied schema mapping.
- Optional deterministic normalization passes after OMC import is stable.
