use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};
use clap::{Args, Subcommand};
use ctx_history_core::{database_path, SyncState, Visibility};
use ctx_history_store::Store;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::runtime::Runtime;

use crate::{
    output::{effective_format, print_json, OutputFormat},
    store_util::open_existing_store_read_only,
    SearchArgs, ShowArgs, ShowTarget,
};

const DATABASE_URL_ENV: &str = "CTX_TURSO_DATABASE_URL";
const AUTH_TOKEN_ENV: &str = "CTX_TURSO_AUTH_TOKEN";
const DEFAULT_PUSH_BATCH_SIZE: usize = 100;
const MAX_PUSH_BATCH_SIZE: usize = 1000;
const DEFAULT_SEARCH_LIMIT: usize = 20;
const MAX_SEARCH_LIMIT: usize = 200;
const REMOTE_WRITE_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const REMOTE_PROJECTION_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const DEFAULT_REMOTE_SYNC_INTERVAL_SECONDS: u64 = 300;
const MIN_REMOTE_SYNC_INTERVAL_SECONDS: u64 = 15;
const MAX_REMOTE_SYNC_INTERVAL_SECONDS: u64 = 3_600;
const REMOTE_SYNC_MAX_CONSECUTIVE_FAILURES: usize = 3;

#[derive(Debug, Args)]
pub(crate) struct TursoArgs {
    #[command(subcommand)]
    command: TursoCommand,
}

impl TursoArgs {
    pub(crate) fn json_output(&self) -> bool {
        match &self.command {
            TursoCommand::Init(args) => args.json,
            TursoCommand::Push(args) => args.json,
            TursoCommand::Import(args) => args.json,
            TursoCommand::Project(args) => args.json,
            TursoCommand::Search(args) => args.json,
            TursoCommand::Status(args) => args.json,
        }
    }
}

#[derive(Debug, Subcommand)]
enum TursoCommand {
    #[command(about = "Create the portable remote ctx projection")]
    Init(TursoInitArgs),
    #[command(about = "Export an existing local ctx index to Turso")]
    Push(TursoPushArgs),
    #[command(
        about = "Import all discovered provider histories directly into Turso without a local SQLite file"
    )]
    Import(TursoImportArgs),
    #[command(about = "Build the remote search projection from an imported ctx SQLite snapshot")]
    Project(TursoProjectArgs),
    #[command(about = "Search history stored in Turso")]
    Search(TursoSearchArgs),
    #[command(about = "Show Turso ctx projection status")]
    Status(TursoStatusArgs),
}

#[derive(Debug, Args)]
struct TursoInitArgs {
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct TursoPushArgs {
    #[arg(
        long,
        default_value_t = DEFAULT_PUSH_BATCH_SIZE,
        value_parser = parse_push_batch_size,
        help = "Number of events per remote transaction (1-1000)"
    )]
    batch_size: usize,
    #[arg(long, help = "Upload no more than this many events")]
    limit: Option<usize>,
    #[arg(
        long,
        help = "Resume one interrupted upload after this event UUID; use only with the same unchanged local index"
    )]
    after_event_id: Option<uuid::Uuid>,
    #[arg(
        long,
        help = "Also export local-only events; required unless events are marked sync_full"
    )]
    include_local_only: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct TursoImportArgs {
    #[arg(
        long,
        default_value_t = DEFAULT_PUSH_BATCH_SIZE,
        value_parser = parse_push_batch_size,
        help = "Number of events per remote transaction (1-1000)"
    )]
    batch_size: usize,
    #[arg(long, value_enum, hide_possible_values = true)]
    provider: Option<crate::NativeProviderArg>,
    #[arg(long, requires = "provider", help = "Import one provider history path")]
    path: Option<PathBuf>,
    #[arg(
        long,
        conflicts_with = "watch",
        help = "Record current provider source fingerprints as an already-imported Turso snapshot baseline"
    )]
    adopt_snapshot: bool,
    #[arg(
        long,
        help = "Keep importing changed provider histories into Turso at a fixed interval"
    )]
    watch: bool,
    #[arg(
        long,
        default_value_t = DEFAULT_REMOTE_SYNC_INTERVAL_SECONDS,
        value_parser = parse_remote_sync_interval_seconds,
        help = "Polling interval for --watch, in seconds (15-3600)"
    )]
    interval_seconds: u64,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct TursoProjectArgs {
    #[arg(long, default_value_t = DEFAULT_PUSH_BATCH_SIZE, value_parser = parse_push_batch_size)]
    batch_size: usize,
    #[arg(long)]
    limit: Option<usize>,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct TursoSearchArgs {
    #[arg(help = "ASCII case-insensitive substring query")]
    query: String,
    #[arg(
        long,
        help = "Filter by ctx provider name, for example codex or claude"
    )]
    provider: Option<String>,
    #[arg(long, default_value_t = DEFAULT_SEARCH_LIMIT, value_parser = parse_search_limit)]
    limit: usize,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct TursoStatusArgs {
    #[arg(long)]
    json: bool,
}

#[derive(Debug)]
struct TursoConfig {
    database_url: String,
    auth_token: String,
}

impl TursoConfig {
    fn from_env() -> Result<Self> {
        let database_url = required_env(DATABASE_URL_ENV)?;
        if !database_url.starts_with("libsql://") && !database_url.starts_with("https://") {
            return Err(anyhow!(
                "{DATABASE_URL_ENV} must start with libsql:// or https://"
            ));
        }
        Ok(Self {
            database_url,
            auth_token: required_env(AUTH_TOKEN_ENV)?,
        })
    }
}

#[derive(Debug)]
struct TursoPushReport {
    uploaded_events: u64,
    skipped_events: usize,
    scanned_events: usize,
    batches: usize,
}

#[derive(Clone, Debug)]
struct RemoteSourceCheckpoint {
    key: String,
    fingerprint: String,
}

#[derive(Clone, Debug)]
struct RemoteCheckpointState {
    fingerprint: String,
    updated_at_ms: i64,
}

#[derive(Clone, Debug)]
struct PendingSourceCheckpoint {
    checkpoint: RemoteSourceCheckpoint,
    previous_updated_at_ms: Option<i64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemoteLayout {
    Projection,
    Snapshot,
}

pub(crate) fn run_turso(args: TursoArgs, data_root: PathBuf) -> Result<()> {
    match args.command {
        TursoCommand::Init(args) => run_async(init(args.json)),
        TursoCommand::Push(args) => {
            let db_path = database_path(data_root);
            let store = open_existing_store_read_only(&db_path, "ctx turso push")?;
            let json_output = args.json;
            run_async(ensure_remote_schema())?;
            let report = run_async(push(store, args))?;
            print_push_report(&report, json_output)
        }
        TursoCommand::Import(args) => run_turso_import(args),
        TursoCommand::Project(args) => run_async(project(args)),
        TursoCommand::Search(args) => run_async(search(args)),
        TursoCommand::Status(args) => run_async(status(args.json)),
    }
}

pub(crate) fn remote_primary_configured() -> bool {
    env::var_os(DATABASE_URL_ENV).is_some()
}

pub(crate) fn run_remote_primary_import(args: crate::ImportArgs) -> Result<()> {
    if args.format.is_some()
        || args.history_source.is_some()
        || !args.history_source_manifest.is_empty()
        || args.reset_cursor
    {
        return Err(anyhow!(
            "remote-primary ctx import supports native providers only; use --provider, --path, or --all"
        ));
    }
    run_turso_import(TursoImportArgs {
        batch_size: DEFAULT_PUSH_BATCH_SIZE,
        provider: args.provider,
        path: args.path,
        adopt_snapshot: false,
        watch: false,
        interval_seconds: DEFAULT_REMOTE_SYNC_INTERVAL_SECONDS,
        json: args.json,
    })
}

pub(crate) fn run_remote_primary_daemon(args: crate::DaemonArgs) -> Result<()> {
    match args.command {
        crate::DaemonCommand::Run(args) => {
            let interval_seconds = args
                .loop_interval_seconds
                .unwrap_or(DEFAULT_REMOTE_SYNC_INTERVAL_SECONDS);
            parse_remote_sync_interval_seconds(&interval_seconds.to_string())
                .map_err(|message| anyhow!(message))?;
            run_turso_import(TursoImportArgs {
                batch_size: DEFAULT_PUSH_BATCH_SIZE,
                provider: None,
                path: None,
                adopt_snapshot: false,
                watch: !args.once,
                interval_seconds,
                json: args.json,
            })
        }
        crate::DaemonCommand::Status(args) => run_remote_primary_status(args.json),
        crate::DaemonCommand::Enable(_) | crate::DaemonCommand::Disable(_) => Err(anyhow!(
            "remote-primary does not persist a local daemon setting; run `ctx daemon run` to sync in the foreground"
        )),
    }
}

pub(crate) fn run_remote_primary_status(json_output: bool) -> Result<()> {
    run_async(status(json_output))
}

/// Search the imported ctx snapshot directly.  This intentionally bypasses `Store`: opening a
/// Store creates a local SQLite database and violates remote-primary mode.
pub(crate) fn run_remote_primary_search(args: SearchArgs) -> Result<()> {
    let query = args.query.as_deref().unwrap_or_default().trim().to_owned();
    if query.is_empty() && args.term.iter().all(|term| term.trim().is_empty()) {
        return Err(anyhow!("remote search needs a query or --term"));
    }
    reject_remote_search_filters(&args)?;
    let provider = args
        .provider
        .map(|provider| provider.capture_provider().as_str().to_owned());
    let value = run_async(remote_snapshot_search(RemoteSearchRequest {
        query,
        terms: args.term,
        limit: args.limit,
        provider,
        session: args.session,
        event_type: args.event_type,
    }))?;
    if args.json {
        return print_json(value);
    }
    print_remote_search_text(&value);
    Ok(())
}

pub(crate) fn run_remote_primary_show(args: ShowArgs) -> Result<()> {
    match args.target {
        ShowTarget::Session(args) => {
            if args.provider.is_some() || args.provider_session.is_some() {
                return Err(anyhow!(
                    "remote ctx show session currently accepts a ctx session id only"
                ));
            }
            let id = args
                .id
                .ok_or_else(|| anyhow!("ctx session id is required"))?;
            let value = run_async(remote_snapshot_session(&id))?;
            write_remote_show(&value, effective_format(args.format, args.json), args.out)?;
            Ok(())
        }
        ShowTarget::Event(args) => {
            if args.before != 0 || args.after != 0 || args.window.is_some() {
                return Err(anyhow!(
                    "remote ctx show event currently returns one event; surrounding windows are not available"
                ));
            }
            let value = run_async(remote_snapshot_event(&args.id))?;
            write_remote_show(&value, effective_format(args.format, args.json), None)?;
            Ok(())
        }
    }
}

pub(crate) fn remote_primary_status_value() -> Result<serde_json::Value> {
    run_async(remote_status_value())
}

pub(crate) fn remote_primary_search_value(
    query: String,
    limit: usize,
    provider: Option<String>,
    session: Option<String>,
    event_type: Option<String>,
) -> Result<serde_json::Value> {
    run_async(remote_snapshot_search(RemoteSearchRequest {
        query,
        terms: Vec::new(),
        limit,
        provider,
        session,
        event_type,
    }))
}

pub(crate) fn remote_primary_session_value(id: &str) -> Result<serde_json::Value> {
    run_async(remote_snapshot_session(id))
}

pub(crate) fn remote_primary_event_value(id: &str) -> Result<serde_json::Value> {
    run_async(remote_snapshot_event(id))
}

fn run_turso_import(args: TursoImportArgs) -> Result<()> {
    if args.adopt_snapshot {
        return run_turso_adopt_snapshot(&args);
    }
    if !args.watch {
        return run_turso_import_once(&args).map(|_| ());
    }

    let interval = Duration::from_secs(args.interval_seconds);
    let mut consecutive_failures = 0usize;
    loop {
        match run_turso_import_once(&args) {
            Ok(_) => consecutive_failures = 0,
            Err(error) => {
                consecutive_failures += 1;
                if consecutive_failures >= REMOTE_SYNC_MAX_CONSECUTIVE_FAILURES {
                    return Err(error.context(format!(
                        "remote-primary sync stopped after {REMOTE_SYNC_MAX_CONSECUTIVE_FAILURES} consecutive failures"
                    )));
                }
                eprintln!(
                    "remote-primary sync failed ({consecutive_failures}/{REMOTE_SYNC_MAX_CONSECUTIVE_FAILURES}); retrying in {} seconds: {error:#}",
                    interval.as_secs()
                );
            }
        }
        thread::sleep(interval);
    }
}

fn run_turso_adopt_snapshot(args: &TursoImportArgs) -> Result<()> {
    run_async(ensure_remote_schema())?;
    if !run_async(remote_has_snapshot())? {
        return Err(anyhow!(
            "ctx turso import --adopt-snapshot requires a previously imported ctx SQLite snapshot"
        ));
    }
    let sources =
        crate::commands::import::remote_native_import_requests(args.provider, args.path.clone())?;
    let mut adopted = 0usize;
    for source in sources {
        let checkpoint = remote_source_checkpoint(&source)?;
        run_async(remote_store_checkpoint(&checkpoint))?;
        adopted = adopted.saturating_add(1);
    }
    if args.json {
        print_json(json!({
            "adopted_snapshot": true,
            "providers_adopted": adopted,
            "remote_primary": true,
        }))?;
    } else {
        println!("adopted_snapshot: true");
        println!("providers_adopted: {adopted}");
    }
    Ok(())
}

fn run_turso_import_once(args: &TursoImportArgs) -> Result<TursoPushReport> {
    run_async(ensure_remote_schema())?;
    let mut report = TursoPushReport {
        uploaded_events: 0,
        skipped_events: 0,
        scanned_events: 0,
        batches: 0,
    };
    let checkpoints = RefCell::new(HashMap::<String, PendingSourceCheckpoint>::new());
    let unchanged_sources = Cell::new(0usize);
    let totals = crate::commands::import::import_all_providers_in_memory_by_source(
        args.provider,
        args.path.clone(),
        |source| {
            let checkpoint = remote_source_checkpoint(source)?;
            let previous = run_async(remote_checkpoint_state(&checkpoint))?;
            if previous
                .as_ref()
                .is_some_and(|state| state.fingerprint == checkpoint.fingerprint)
            {
                unchanged_sources.set(unchanged_sources.get().saturating_add(1));
            } else {
                checkpoints.borrow_mut().insert(
                    checkpoint.key.clone(),
                    PendingSourceCheckpoint {
                        checkpoint,
                        previous_updated_at_ms: previous.map(|state| state.updated_at_ms),
                    },
                );
            }
            Ok(checkpoints
                .borrow()
                .contains_key(&source_checkpoint_key(source)))
        },
        |_source, store| {
            let source_report = run_async(push(
                store,
                TursoPushArgs {
                    batch_size: args.batch_size,
                    limit: None,
                    after_event_id: None,
                    include_local_only: true,
                    json: args.json,
                },
            ))?;
            report.uploaded_events = report
                .uploaded_events
                .saturating_add(source_report.uploaded_events);
            report.skipped_events = report
                .skipped_events
                .saturating_add(source_report.skipped_events);
            report.scanned_events = report
                .scanned_events
                .saturating_add(source_report.scanned_events);
            report.batches = report.batches.saturating_add(source_report.batches);
            Ok(())
        },
        |source| {
            let current = remote_source_checkpoint(source)?;
            let Some(before) = checkpoints.borrow_mut().remove(&current.key) else {
                return Ok(());
            };
            if before.checkpoint.fingerprint == current.fingerprint {
                run_async(remote_store_checkpoint(&current))?;
            }
            Ok(())
        },
        |source, path| {
            let key = source_checkpoint_key(source);
            let checkpoints = checkpoints.borrow();
            let Some(pending) = checkpoints.get(&key) else {
                return Ok(false);
            };
            let Some(updated_at_ms) = pending.previous_updated_at_ms else {
                return Ok(true);
            };
            let modified_at_ms = fs::metadata(path)
                .with_context(|| format!("stat provider history file {}", path.display()))?
                .modified()
                .unwrap_or(UNIX_EPOCH)
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .try_into()
                .unwrap_or(i64::MAX);
            Ok(modified_at_ms > updated_at_ms)
        },
    )?;
    if args.json {
        print_json(json!({
            "ephemeral_store": true,
            "providers_imported": totals.imported_sources,
            "providers_skipped_unchanged": unchanged_sources.get(),
            "events_materialized": totals.imported_events,
            "source_failures": totals.failed_sources,
            "uploaded_events": report.uploaded_events,
            "skipped_events": report.skipped_events,
            "scanned_events": report.scanned_events,
            "batches": report.batches,
            "idempotent": true,
            "remote_projection": true,
        }))?
    } else {
        println!("ephemeral_store: true");
        println!("providers_imported: {}", totals.imported_sources);
        println!("providers_skipped_unchanged: {}", unchanged_sources.get());
        println!("events_materialized: {}", totals.imported_events);
        println!("source_failures: {}", totals.failed_sources);
        print_push_report(&report, false)?
    }
    Ok(report)
}

fn run_async<T>(operation: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    Runtime::new()
        .context("create async runtime for Turso")?
        .block_on(operation)
}

async fn connect() -> Result<libsql::Connection> {
    let config = TursoConfig::from_env()?;
    let database = libsql::Builder::new_remote(config.database_url, config.auth_token)
        .build()
        .await
        .context("connect to Turso")?;
    database.connect().context("open Turso connection")
}

async fn init(json_output: bool) -> Result<()> {
    ensure_remote_schema().await?;
    if json_output {
        print_json(json!({"initialized": true, "remote_projection": true}))?;
    } else {
        println!("Turso ctx projection is ready.");
    }
    Ok(())
}

async fn remote_has_snapshot() -> Result<bool> {
    let conn = connect().await?;
    has_imported_snapshot(&conn).await
}

async fn ensure_remote_schema() -> Result<()> {
    tokio::time::timeout(REMOTE_WRITE_TIMEOUT, async {
        let conn = connect().await?;
        if conn
            .query("SELECT 1 FROM ctx_turso_event_keys LIMIT 1", ())
            .await
            .is_ok()
        {
            return Ok(());
        }
        ensure_schema(&conn).await
    })
    .await
    .with_context(|| {
        format!(
            "prepare Turso remote-primary schema timed out after {} seconds",
            REMOTE_WRITE_TIMEOUT.as_secs()
        )
    })?
}

async fn ensure_schema(conn: &libsql::Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS ctx_turso_events (\
            event_id TEXT PRIMARY KEY,\
            session_id TEXT,\
            provider TEXT NOT NULL,\
            role TEXT,\
            event_type TEXT NOT NULL,\
            occurred_at_ms INTEGER NOT NULL,\
            dedupe_key TEXT,\
            payload_json TEXT NOT NULL,\
            search_text TEXT NOT NULL DEFAULT ''\
        )",
        (),
    )
    .await
    .context("create Turso event table")?;
    ensure_dedupe_key_column(conn).await?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS ctx_turso_events_occurred_at \
         ON ctx_turso_events(occurred_at_ms DESC)",
        (),
    )
    .await
    .context("create Turso event time index")?;
    conn.execute(
        "CREATE VIRTUAL TABLE IF NOT EXISTS ctx_turso_search USING fts5(event_id UNINDEXED, search_text)",
        (),
    )
    .await
    .context("create Turso full-text search index")?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS ctx_turso_events_provider \
         ON ctx_turso_events(provider)",
        (),
    )
    .await
    .context("create Turso provider index")?;
    conn.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS ctx_turso_events_dedupe_key \
         ON ctx_turso_events(dedupe_key) WHERE dedupe_key IS NOT NULL",
        (),
    )
    .await
    .context("create Turso dedupe index")?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS ctx_turso_event_keys (\
            dedupe_key TEXT PRIMARY KEY,\
            event_id TEXT NOT NULL\
        )",
        (),
    )
    .await
    .context("create Turso cross-device event key table")?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS ctx_turso_source_checkpoints (\
            source_key TEXT PRIMARY KEY,\
            fingerprint TEXT NOT NULL,\
            updated_at_ms INTEGER NOT NULL\
        )",
        (),
    )
    .await
    .context("create Turso remote source checkpoint table")?;
    Ok(())
}

fn remote_source_checkpoint(
    source: &crate::provider_sources::SourceInfo,
) -> Result<RemoteSourceCheckpoint> {
    let key = source_checkpoint_key(source);
    let stats = crate::commands::import::source_stats(Path::new(&source.path))?;
    let change_token = stats.change_token.ok_or_else(|| {
        anyhow!(
            "could not calculate a change token for {} provider history at {}",
            source.provider.as_str(),
            source.path.display()
        )
    })?;
    let mut fingerprint = Sha256::new();
    fingerprint.update(source.provider.as_str().as_bytes());
    fingerprint.update([0]);
    fingerprint.update(source.source_format.as_bytes());
    fingerprint.update([0]);
    fingerprint.update((stats.files as u64).to_le_bytes());
    fingerprint.update(stats.bytes.to_le_bytes());
    fingerprint.update(change_token);
    Ok(RemoteSourceCheckpoint {
        key,
        fingerprint: format!("sha256:{:x}", fingerprint.finalize()),
    })
}

fn source_checkpoint_key(source: &crate::provider_sources::SourceInfo) -> String {
    let source_path = fs::canonicalize(&source.path).unwrap_or_else(|_| source.path.clone());
    let mut identity = Sha256::new();
    identity.update(source.provider.as_str().as_bytes());
    identity.update([0]);
    identity.update(source.source_format.as_bytes());
    identity.update([0]);
    identity.update(source_path.as_os_str().as_encoded_bytes());
    format!("sha256:{:x}", identity.finalize())
}

async fn remote_checkpoint_state(
    checkpoint: &RemoteSourceCheckpoint,
) -> Result<Option<RemoteCheckpointState>> {
    let conn = connect().await?;
    let mut rows = conn
        .query(
            "SELECT fingerprint, updated_at_ms FROM ctx_turso_source_checkpoints WHERE source_key = ?1",
            libsql::params![checkpoint.key.clone()],
        )
        .await
        .context("read Turso remote source checkpoint")?;
    let Some(row) = rows
        .next()
        .await
        .context("read Turso remote source checkpoint row")?
    else {
        return Ok(None);
    };
    Ok(Some(RemoteCheckpointState {
        fingerprint: row.get::<String>(0)?,
        updated_at_ms: row.get::<i64>(1)?,
    }))
}

async fn remote_store_checkpoint(checkpoint: &RemoteSourceCheckpoint) -> Result<()> {
    let conn = connect().await?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX);
    tokio::time::timeout(
        REMOTE_WRITE_TIMEOUT,
        conn.execute(
            "INSERT INTO ctx_turso_source_checkpoints(source_key, fingerprint, updated_at_ms) \
             VALUES (?1, ?2, ?3) \
             ON CONFLICT(source_key) DO UPDATE SET fingerprint = excluded.fingerprint, updated_at_ms = excluded.updated_at_ms",
            libsql::params![checkpoint.key.clone(), checkpoint.fingerprint.clone(), now],
        ),
    )
    .await
    .with_context(|| {
        format!(
            "store Turso remote source checkpoint timed out after {} seconds",
            REMOTE_WRITE_TIMEOUT.as_secs()
        )
    })?
    .context("store Turso remote source checkpoint")?;
    Ok(())
}

async fn project(args: TursoProjectArgs) -> Result<()> {
    let conn = connect().await?;
    if !has_imported_snapshot(&conn).await? {
        return Err(anyhow!(
            "ctx turso project requires an imported ctx SQLite snapshot"
        ));
    }
    ensure_schema(&conn).await?;
    let mut projected = 0usize;
    loop {
        if args.limit.is_some_and(|limit| projected >= limit) {
            break;
        }
        let after = remote_max_event_id(&conn).await?;
        let predicate = after
            .as_ref()
            .map(|id| format!("AND e.id > {}", sql_text_literal(id)))
            .unwrap_or_default();
        let search_predicate = after
            .as_ref()
            .map(|id| format!("WHERE event_id > {}", sql_text_literal(id)))
            .unwrap_or_default();
        let sql = format!("INSERT OR IGNORE INTO ctx_turso_events \
        (event_id, session_id, provider, role, event_type, occurred_at_ms, payload_json, search_text) \
        SELECT e.id, e.session_id, COALESCE(s.provider, 'unknown'), e.role, e.event_type, e.occurred_at_ms, e.payload_json, e.payload_json \
        FROM events e LEFT JOIN sessions s ON s.id = e.session_id \
        WHERE e.deleted_at_ms IS NULL {predicate} ORDER BY e.id LIMIT {}; \
        INSERT INTO ctx_turso_search(event_id, search_text) \
        SELECT event_id, search_text FROM ctx_turso_events {search_predicate} ORDER BY event_id LIMIT {};", args.batch_size, args.batch_size);
        tokio::time::timeout(
            REMOTE_PROJECTION_TIMEOUT,
            conn.execute_transactional_batch(&sql),
        )
        .await
        .with_context(|| {
            format!(
                "build Turso search projection timed out after {} seconds",
                REMOTE_PROJECTION_TIMEOUT.as_secs()
            )
        })?
        .context("build Turso search projection")?;
        let next = remote_max_event_id(&conn).await?;
        if next == after {
            break;
        }
        projected += args.batch_size;
    }
    if args.json {
        print_json(json!({"projected": true, "remote_primary": true}))?;
    } else {
        println!("Turso search projection is ready.");
    }
    Ok(())
}

async fn remote_max_event_id(conn: &libsql::Connection) -> Result<Option<String>> {
    let mut rows = conn
        .query("SELECT MAX(event_id) FROM ctx_turso_events", ())
        .await?;
    Ok(rows
        .next()
        .await?
        .map(|row| row.get::<Option<String>>(0))
        .transpose()?
        .flatten())
}

async fn ensure_dedupe_key_column(conn: &libsql::Connection) -> Result<()> {
    let mut columns = conn
        .query("PRAGMA table_info(ctx_turso_events)", ())
        .await
        .context("inspect Turso event table")?;
    while let Some(column) = columns.next().await.context("read Turso table column")? {
        if column.get::<String>(1)? == "dedupe_key" {
            return Ok(());
        }
    }
    conn.execute(
        "ALTER TABLE ctx_turso_events ADD COLUMN dedupe_key TEXT",
        (),
    )
    .await
    .context("add Turso event dedupe key")?;
    Ok(())
}

async fn push(store: Store, args: TursoPushArgs) -> Result<TursoPushReport> {
    let conn = connect().await?;
    let session_identities = store
        .list_sessions()?
        .into_iter()
        .map(|session| {
            (
                session.id,
                (
                    session.provider.as_str().to_owned(),
                    session.external_session_id,
                ),
            )
        })
        .collect::<std::collections::HashMap<_, _>>();
    let capture_source_providers = store
        .list_capture_sources()?
        .into_iter()
        .map(|source| (source.id, source.descriptor.provider.as_str().to_owned()))
        .collect::<std::collections::HashMap<_, _>>();

    let mut after_id = args.after_event_id;
    let mut uploaded = 0u64;
    let mut skipped = 0usize;
    let mut scanned = 0usize;
    let mut batches = 0usize;
    loop {
        let remaining = args.limit.map(|limit| limit.saturating_sub(scanned));
        if remaining == Some(0) {
            break;
        }
        let page_size = remaining
            .map(|limit| limit.min(args.batch_size))
            .unwrap_or(args.batch_size);
        let events = store.list_events_page_after(after_id, page_size)?;
        let Some(last) = events.last() else {
            break;
        };
        scanned += events.len();
        after_id = Some(last.id);

        let dedupe_candidates = events
            .iter()
            .filter_map(|event| remote_dedupe_candidate(event, &session_identities))
            .collect::<Vec<_>>();
        let known_dedupe_keys = tokio::time::timeout(
            REMOTE_WRITE_TIMEOUT,
            remote_existing_dedupe_keys(&conn, &dedupe_candidates),
        )
        .await
        .with_context(|| {
            format!(
                "read Turso cross-device event keys timed out after {} seconds",
                REMOTE_WRITE_TIMEOUT.as_secs()
            )
        })??;

        let mut sql = String::new();
        let mut exported_statements = 0usize;
        let mut projected_event_ids = Vec::new();
        for event in &events {
            if !remote_export_allowed(event, args.include_local_only) {
                skipped += 1;
                continue;
            }
            let payload_json =
                serde_json::to_string(&event.payload).context("serialize event payload")?;
            let provider = event
                .session_id
                .and_then(|id| session_identities.get(&id).map(|identity| &identity.0))
                .or_else(|| {
                    event
                        .capture_source_id
                        .and_then(|id| capture_source_providers.get(&id))
                })
                .map(String::as_str)
                .unwrap_or("unknown");
            let dedupe_key = remote_dedupe_key(event, &session_identities);
            if dedupe_key
                .as_ref()
                .is_some_and(|key| known_dedupe_keys.contains(key))
            {
                skipped += 1;
                continue;
            }
            let event_id = event.id.to_string();
            let event_id_literal = sql_text_literal(&event_id);
            if let Some(dedupe_key) = dedupe_key.as_deref() {
                sql.push_str(
                    "INSERT OR IGNORE INTO ctx_turso_event_keys(dedupe_key, event_id) VALUES (",
                );
                sql.push_str(sql_text_literal(dedupe_key).as_str());
                sql.push(',');
                sql.push_str(event_id_literal.as_str());
                sql.push_str(");");
            }
            sql.push_str("INSERT OR IGNORE INTO ctx_turso_events \
                (event_id, session_id, provider, role, event_type, occurred_at_ms, dedupe_key, payload_json, search_text) \
                SELECT ");
            sql.push_str(event_id_literal.as_str());
            sql.push(',');
            push_optional_sql_text_literal(&mut sql, event.session_id.map(|id| id.to_string()));
            sql.push(',');
            sql.push_str(sql_text_literal(provider).as_str());
            sql.push(',');
            push_optional_sql_text_literal(
                &mut sql,
                event.role.map(|role| role.as_str().to_owned()),
            );
            sql.push(',');
            sql.push_str(sql_text_literal(event.event_type.as_str()).as_str());
            sql.push(',');
            sql.push_str(event.occurred_at.timestamp_millis().to_string().as_str());
            sql.push(',');
            push_optional_sql_text_literal(&mut sql, dedupe_key.clone());
            sql.push(',');
            sql.push_str(sql_text_literal(payload_json.as_str()).as_str());
            sql.push(',');
            sql.push_str(sql_text_literal(payload_json.as_str()).as_str());
            if let Some(dedupe_key) = dedupe_key.as_deref() {
                sql.push_str(
                    " WHERE (SELECT event_id FROM ctx_turso_event_keys WHERE dedupe_key = ",
                );
                sql.push_str(sql_text_literal(dedupe_key).as_str());
                sql.push_str(") = ");
                sql.push_str(event_id_literal.as_str());
            }
            sql.push_str(";");
            projected_event_ids.push(event_id_literal);
            exported_statements += 1;
        }
        if exported_statements > 0 {
            let event_ids = projected_event_ids.join(",");
            // FTS5 has no unique event-id constraint. Replace the batch in the same remote
            // transaction so retries remain idempotent and newly imported events are searchable.
            sql.push_str("DELETE FROM ctx_turso_search WHERE event_id IN (");
            sql.push_str(&event_ids);
            sql.push_str(");");
            sql.push_str(
                "INSERT INTO ctx_turso_search(event_id, search_text) \
                 SELECT event_id, search_text FROM ctx_turso_events WHERE event_id IN (",
            );
            sql.push_str(&event_ids);
            sql.push_str(");");
            tokio::time::timeout(
                REMOTE_WRITE_TIMEOUT,
                conn.execute_transactional_batch(sql.as_str()),
            )
            .await
            .with_context(|| {
                format!(
                    "upload batch ending at event {} timed out after {} seconds",
                    last.id,
                    REMOTE_WRITE_TIMEOUT.as_secs()
                )
            })?
            .context("upload Turso event batch")?;
            uploaded += exported_statements as u64;
        }
        batches += 1;
    }

    Ok(TursoPushReport {
        uploaded_events: uploaded,
        skipped_events: skipped,
        scanned_events: scanned,
        batches,
    })
}

async fn remote_existing_dedupe_keys(
    conn: &libsql::Connection,
    candidates: &[RemoteDedupeCandidate],
) -> Result<std::collections::HashSet<String>> {
    if candidates.is_empty() {
        return Ok(std::collections::HashSet::new());
    }
    let mut unique = candidates
        .iter()
        .map(|candidate| candidate.canonical.clone())
        .collect::<Vec<_>>();
    unique.sort();
    unique.dedup();
    let sql = format!(
        "SELECT dedupe_key FROM ctx_turso_event_keys WHERE dedupe_key IN ({})",
        unique
            .iter()
            .map(|key| sql_text_literal(key))
            .collect::<Vec<_>>()
            .join(",")
    );
    let mut rows = conn
        .query(&sql, ())
        .await
        .context("read Turso cross-device event keys")?;
    let mut existing = std::collections::HashSet::new();
    while let Some(row) = rows.next().await.context("read Turso event key")? {
        existing.insert(row.get::<String>(0)?);
    }
    // `ctx_turso_event_keys` is the authoritative cross-device merge point. Searching arbitrary
    // suffixes in a multi-GB imported snapshot is an unindexed full-table scan and can stall a
    // normal provider import indefinitely. New keys and projection events are committed together,
    // so concurrent Macs still converge through this unique key table.
    Ok(existing)
}

#[derive(Clone)]
struct RemoteDedupeCandidate {
    canonical: String,
}

fn remote_dedupe_candidate(
    event: &ctx_history_core::Event,
    sessions: &std::collections::HashMap<uuid::Uuid, (String, Option<String>)>,
) -> Option<RemoteDedupeCandidate> {
    let source_dedupe_key = event.dedupe_key.as_deref()?.to_owned();
    let session_id = event.session_id?;
    let (provider, external_session_id) = sessions.get(&session_id)?;
    let external_session_id = external_session_id.as_deref()?.trim();
    if external_session_id.is_empty() {
        return None;
    }
    let canonical = canonical_provider_source_dedupe_key(
        provider,
        Some(external_session_id),
        &source_dedupe_key,
    )?;
    Some(RemoteDedupeCandidate { canonical })
}

fn sql_text_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn push_optional_sql_text_literal(target: &mut String, value: Option<String>) {
    match value {
        Some(value) => target.push_str(sql_text_literal(value.as_str()).as_str()),
        None => target.push_str("NULL"),
    }
}

fn remote_dedupe_key(
    event: &ctx_history_core::Event,
    sessions: &std::collections::HashMap<uuid::Uuid, (String, Option<String>)>,
) -> Option<String> {
    let source_dedupe_key = event.dedupe_key.as_deref()?;
    let session_id = event.session_id?;
    let (provider, external_session_id) = sessions.get(&session_id)?;
    canonical_provider_source_dedupe_key(
        provider,
        external_session_id.as_deref(),
        source_dedupe_key,
    )
    .or_else(|| Some(source_dedupe_key.to_owned()))
}

fn canonical_provider_source_dedupe_key(
    provider: &str,
    external_session_id: Option<&str>,
    source_dedupe_key: &str,
) -> Option<String> {
    let external_session_id = external_session_id?.trim();
    if external_session_id.is_empty() {
        return None;
    }
    let source_dedupe_key = source_dedupe_key.strip_prefix("provider-source:")?;
    let (_, event_identity) = source_dedupe_key.split_once(':')?;
    let (provider_event_index, payload_hash) = event_identity.split_once(':')?;
    if provider_event_index.is_empty() || payload_hash.is_empty() {
        return None;
    }
    Some(format!(
        "provider:{provider}:{external_session_id}:{provider_event_index}:{payload_hash}"
    ))
}

fn print_push_report(report: &TursoPushReport, json_output: bool) -> Result<()> {
    if json_output {
        print_json(json!({
            "uploaded_events": report.uploaded_events,
            "skipped_events": report.skipped_events,
            "scanned_events": report.scanned_events,
            "batches": report.batches,
            "idempotent": true,
            "remote_projection": true,
        }))?;
    } else {
        println!("uploaded_events: {}", report.uploaded_events);
        println!("skipped_events: {}", report.skipped_events);
        println!("scanned_events: {}", report.scanned_events);
        println!("batches: {}", report.batches);
        println!("idempotent: true");
    }
    Ok(())
}

async fn search(args: TursoSearchArgs) -> Result<()> {
    let conn = connect().await?;
    let layout = remote_layout(&conn).await?;
    let (sql, params) = match (layout, args.provider.as_deref()) {
        (RemoteLayout::Projection, Some(provider)) => (
            "SELECT event_id, session_id, provider, role, event_type, occurred_at_ms, payload_json \
             FROM ctx_turso_events JOIN ctx_turso_search USING(event_id) \
             WHERE provider = ?1 AND ctx_turso_search MATCH ?2 \
             ORDER BY occurred_at_ms DESC LIMIT ?3",
            libsql::params_from_iter(vec![
                libsql::Value::Text(provider.to_owned()),
                libsql::Value::Text(fts_match_query(&args.query)),
                libsql::Value::Integer(args.limit as i64),
            ]),
        ),
        (RemoteLayout::Projection, None) => (
            "SELECT event_id, session_id, provider, role, event_type, occurred_at_ms, payload_json \
             FROM ctx_turso_events JOIN ctx_turso_search USING(event_id) \
             WHERE ctx_turso_search MATCH ?1 \
             ORDER BY occurred_at_ms DESC LIMIT ?2",
            libsql::params_from_iter(vec![
                libsql::Value::Text(fts_match_query(&args.query)),
                libsql::Value::Integer(args.limit as i64),
            ]),
        ),
        (RemoteLayout::Snapshot, Some(provider)) => (
            "SELECT ctx_event_id, ctx_session_id, COALESCE(provider, 'unknown'), role, event_type, occurred_at_ms, payload_json \
             FROM ctx_events WHERE provider = ?1 AND payload_json LIKE ?2 ESCAPE '\\' COLLATE NOCASE \
             ORDER BY occurred_at_ms DESC LIMIT ?3",
            libsql::params_from_iter(vec![
                libsql::Value::Text(provider.to_owned()),
                libsql::Value::Text(substring_pattern(&args.query)),
                libsql::Value::Integer(args.limit as i64),
            ]),
        ),
        (RemoteLayout::Snapshot, None) => (
            "SELECT ctx_event_id, ctx_session_id, COALESCE(provider, 'unknown'), role, event_type, occurred_at_ms, payload_json \
             FROM ctx_events WHERE payload_json LIKE ?1 ESCAPE '\\' COLLATE NOCASE \
             ORDER BY occurred_at_ms DESC LIMIT ?2",
            libsql::params_from_iter(vec![
                libsql::Value::Text(substring_pattern(&args.query)),
                libsql::Value::Integer(args.limit as i64),
            ]),
        ),
    };
    let mut rows = conn
        .query(sql, params)
        .await
        .context("search Turso history")?;
    let mut results = Vec::new();
    while let Some(row) = rows.next().await.context("read Turso search result")? {
        results.push(json!({
            "event_id": row.get::<String>(0)?,
            "session_id": row.get::<Option<String>>(1)?,
            "provider": row.get::<String>(2)?,
            "role": row.get::<Option<String>>(3)?,
            "event_type": row.get::<String>(4)?,
            "occurred_at_ms": row.get::<i64>(5)?,
            "payload_json": row.get::<String>(6)?,
        }));
    }
    if args.json {
        print_json(json!({"query": args.query, "results": results}))?;
    } else {
        for result in results {
            println!("{}", serde_json::to_string(&result)?);
        }
    }
    Ok(())
}

struct RemoteSearchRequest {
    query: String,
    terms: Vec<String>,
    limit: usize,
    provider: Option<String>,
    session: Option<String>,
    event_type: Option<String>,
}

fn reject_remote_search_filters(args: &SearchArgs) -> Result<()> {
    let unsupported = [
        (args.history_source.is_some(), "--history-source"),
        (args.provider_key.is_some(), "--provider-key"),
        (args.source_id.is_some(), "--source-id"),
        (args.source_format.is_some(), "--source-format"),
        (args.workspace.is_some(), "--workspace"),
        (args.since.is_some(), "--since"),
        (args.primary_only, "--primary-only"),
        (args.include_subagents, "--include-subagents"),
        (args.file.is_some(), "--file"),
        (args.include_current_session, "--include-current-session"),
    ];
    if let Some((_, flag)) = unsupported.into_iter().find(|(enabled, _)| *enabled) {
        return Err(anyhow!(
            "{flag} is not available in remote-primary mode yet; use query, --term, --provider, --session, or --event-type"
        ));
    }
    if !matches!(args.backend, None | Some(crate::SearchBackendArg::Lexical)) {
        return Err(anyhow!(
            "semantic search is local-only; use --backend lexical in remote-primary mode"
        ));
    }
    Ok(())
}

async fn remote_snapshot_search(request: RemoteSearchRequest) -> Result<serde_json::Value> {
    let conn = connect().await?;
    if !has_imported_snapshot(&conn).await? {
        return remote_projection_search(&conn, request).await;
    }
    let mut filters = vec!["e.deleted_at_ms IS NULL".to_owned()];
    if let Some(provider) = request.provider.as_deref() {
        filters.push(format!("s.provider = {}", sql_text_literal(provider)));
    }
    if let Some(session) = request.session.as_deref() {
        filters.push(format!(
            "e.session_id LIKE {}",
            sql_text_literal(&format!("{session}%"))
        ));
    }
    if let Some(event_type) = request.event_type.as_deref() {
        filters.push(format!("e.event_type = {}", sql_text_literal(event_type)));
    }
    let terms = std::iter::once(request.query.as_str())
        .chain(request.terms.iter().map(String::as_str))
        .map(str::trim)
        .filter(|term| !term.is_empty())
        .map(fts_match_query)
        .collect::<Vec<_>>();
    let where_clause = filters.join(" AND ");
    let sql = if terms.is_empty() {
        format!(
            "SELECT e.id, e.session_id, COALESCE(s.provider, 'unknown'), e.role, e.event_type, e.occurred_at_ms, e.payload_json \
             FROM events e LEFT JOIN sessions s ON s.id = e.session_id \
             WHERE {where_clause} ORDER BY e.occurred_at_ms DESC LIMIT {}",
            request.limit
        )
    } else {
        format!(
            "SELECT e.id, e.session_id, COALESCE(s.provider, 'unknown'), e.role, e.event_type, e.occurred_at_ms, e.payload_json \
             FROM event_search es JOIN events e ON e.id = es.event_id \
             LEFT JOIN sessions s ON s.id = e.session_id \
             WHERE event_search MATCH {} AND {where_clause} \
             ORDER BY e.occurred_at_ms DESC LIMIT {}",
            sql_text_literal(&terms.join(" OR ")),
            request.limit
        )
    };
    let mut results = remote_event_rows(&conn, &sql).await?;
    // `ctx turso import` writes portable events into the projection table. Keep those events
    // searchable while an imported snapshot remains the authoritative historical corpus.
    if !terms.is_empty() && has_projection(&conn).await? {
        let mut projection_filters = vec![
            "raw_event.id IS NULL".to_owned(),
            "(p.dedupe_key IS NULL OR remote_key.event_id = p.event_id)".to_owned(),
        ];
        if let Some(provider) = request.provider.as_deref() {
            projection_filters.push(format!("p.provider = {}", sql_text_literal(provider)));
        }
        if let Some(session) = request.session.as_deref() {
            projection_filters.push(format!(
                "p.session_id LIKE {}",
                sql_text_literal(&format!("{session}%"))
            ));
        }
        if let Some(event_type) = request.event_type.as_deref() {
            projection_filters.push(format!("p.event_type = {}", sql_text_literal(event_type)));
        }
        let projection_sql = format!(
            "SELECT p.event_id, p.session_id, p.provider, p.role, p.event_type, p.occurred_at_ms, p.payload_json \
             FROM ctx_turso_search JOIN ctx_turso_events p USING(event_id) \
             LEFT JOIN events raw_event ON raw_event.id = p.event_id \
             LEFT JOIN ctx_turso_event_keys remote_key ON remote_key.dedupe_key = p.dedupe_key \
             WHERE ctx_turso_search MATCH {} AND {} ORDER BY p.occurred_at_ms DESC LIMIT {}",
            sql_text_literal(&terms.join(" OR ")),
            projection_filters.join(" AND "),
            request.limit
        );
        results.extend(remote_event_rows(&conn, &projection_sql).await?);
        results.sort_by_key(|event| {
            std::cmp::Reverse(
                event
                    .get("occurred_at_ms")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or_default(),
            )
        });
        results.truncate(request.limit);
    }
    Ok(json!({
        "remote_primary": true,
        "query": request.query,
        "results": results,
    }))
}

async fn remote_projection_search(
    conn: &libsql::Connection,
    request: RemoteSearchRequest,
) -> Result<serde_json::Value> {
    if !has_projection(conn).await? {
        return Err(anyhow!("remote-primary has no imported history yet"));
    }
    let mut filters = Vec::new();
    if let Some(provider) = request.provider.as_deref() {
        filters.push(format!("p.provider = {}", sql_text_literal(provider)));
    }
    if let Some(session) = request.session.as_deref() {
        filters.push(format!(
            "p.session_id LIKE {}",
            sql_text_literal(&format!("{session}%"))
        ));
    }
    if let Some(event_type) = request.event_type.as_deref() {
        filters.push(format!("p.event_type = {}", sql_text_literal(event_type)));
    }
    let terms = std::iter::once(request.query.as_str())
        .chain(request.terms.iter().map(String::as_str))
        .map(str::trim)
        .filter(|term| !term.is_empty())
        .map(fts_match_query)
        .collect::<Vec<_>>();
    let where_clause = (!filters.is_empty())
        .then(|| format!("WHERE {}", filters.join(" AND ")))
        .unwrap_or_default();
    let sql = if terms.is_empty() {
        format!(
            "SELECT p.event_id, p.session_id, p.provider, p.role, p.event_type, p.occurred_at_ms, p.payload_json \
             FROM ctx_turso_events p {where_clause} ORDER BY p.occurred_at_ms DESC LIMIT {}",
            request.limit
        )
    } else {
        let conjunction = if filters.is_empty() { "WHERE" } else { "AND" };
        format!(
            "SELECT p.event_id, p.session_id, p.provider, p.role, p.event_type, p.occurred_at_ms, p.payload_json \
             FROM ctx_turso_search JOIN ctx_turso_events p USING(event_id) {where_clause} \
             {conjunction} ctx_turso_search MATCH {} ORDER BY p.occurred_at_ms DESC LIMIT {}",
            sql_text_literal(&terms.join(" OR ")),
            request.limit
        )
    };
    Ok(json!({
        "remote_primary": true,
        "query": request.query,
        "results": remote_event_rows(conn, &sql).await?,
    }))
}

async fn remote_snapshot_session(id: &str) -> Result<serde_json::Value> {
    let conn = connect().await?;
    if !has_imported_snapshot(&conn).await? {
        return remote_projection_session(&conn, id).await;
    }
    let session_match = remote_id_match("id", id);
    let session_sql = format!(
        "SELECT id, provider, external_session_id, started_at_ms, ended_at_ms FROM sessions \
         WHERE {session_match} AND deleted_at_ms IS NULL LIMIT 2",
    );
    let mut sessions = conn.query(&session_sql, ()).await?;
    let Some(session) = sessions.next().await? else {
        return remote_projection_session(&conn, id).await;
    };
    if sessions.next().await?.is_some() {
        return Err(anyhow!("remote ctx session id prefix {id} is ambiguous"));
    }
    let session_id = session.get::<String>(0)?;
    let events = remote_event_rows(
        &conn,
        &format!(
            "SELECT e.id, e.session_id, COALESCE(s.provider, 'unknown'), e.role, e.event_type, e.occurred_at_ms, e.payload_json \
             FROM events e LEFT JOIN sessions s ON s.id = e.session_id \
             WHERE e.session_id = {} AND e.deleted_at_ms IS NULL ORDER BY e.occurred_at_ms, e.seq",
            sql_text_literal(&session_id)
        ),
    )
    .await?;
    Ok(json!({
        "remote_primary": true,
        "session": {
            "id": session_id,
            "provider": session.get::<String>(1)?,
            "external_session_id": session.get::<Option<String>>(2)?,
            "started_at_ms": session.get::<i64>(3)?,
            "ended_at_ms": session.get::<Option<i64>>(4)?,
        },
        "events": events,
    }))
}

async fn remote_snapshot_event(id: &str) -> Result<serde_json::Value> {
    let conn = connect().await?;
    if !has_imported_snapshot(&conn).await? {
        return remote_projection_event(&conn, id).await;
    }
    let events = remote_event_rows(
        &conn,
        &format!(
            "SELECT e.id, e.session_id, COALESCE(s.provider, 'unknown'), e.role, e.event_type, e.occurred_at_ms, e.payload_json \
             FROM events e LEFT JOIN sessions s ON s.id = e.session_id \
             WHERE {} AND e.deleted_at_ms IS NULL LIMIT 2",
            remote_id_match("e.id", id)
        ),
    )
    .await?;
    match events.as_slice() {
        [] => remote_projection_event(&conn, id).await,
        [event] => Ok(json!({"remote_primary": true, "event": event})),
        _ => Err(anyhow!("remote ctx event id prefix {id} is ambiguous")),
    }
}

async fn remote_projection_session(
    conn: &libsql::Connection,
    id: &str,
) -> Result<serde_json::Value> {
    if !has_projection(conn).await? {
        return Err(anyhow!("remote ctx session {id} was not found"));
    }
    let session_match = remote_id_match("session_id", id);
    let metadata_sql = format!(
        "SELECT session_id, MIN(provider), MIN(occurred_at_ms), MAX(occurred_at_ms) \
         FROM ctx_turso_events WHERE {session_match} GROUP BY session_id LIMIT 2"
    );
    let mut sessions = conn.query(&metadata_sql, ()).await?;
    let Some(session) = sessions.next().await? else {
        return Err(anyhow!("remote ctx session {id} was not found"));
    };
    if sessions.next().await?.is_some() {
        return Err(anyhow!("remote ctx session id prefix {id} is ambiguous"));
    }
    let session_id = session.get::<String>(0)?;
    let events = remote_event_rows(
        conn,
        &format!(
            "SELECT event_id, session_id, provider, role, event_type, occurred_at_ms, payload_json \
             FROM ctx_turso_events WHERE session_id = {} ORDER BY occurred_at_ms, event_id",
            sql_text_literal(&session_id)
        ),
    )
    .await?;
    Ok(json!({
        "remote_primary": true,
        "session": {
            "id": session_id,
            "provider": session.get::<String>(1)?,
            "started_at_ms": session.get::<i64>(2)?,
            "ended_at_ms": session.get::<i64>(3)?,
        },
        "events": events,
    }))
}

async fn remote_projection_event(conn: &libsql::Connection, id: &str) -> Result<serde_json::Value> {
    if !has_projection(conn).await? {
        return Err(anyhow!("remote ctx event {id} was not found"));
    }
    let events = remote_event_rows(
        conn,
        &format!(
            "SELECT event_id, session_id, provider, role, event_type, occurred_at_ms, payload_json \
             FROM ctx_turso_events WHERE {} LIMIT 2",
            remote_id_match("event_id", id)
        ),
    )
    .await?;
    match events.as_slice() {
        [] => Err(anyhow!("remote ctx event {id} was not found")),
        [event] => Ok(json!({"remote_primary": true, "event": event})),
        _ => Err(anyhow!("remote ctx event id prefix {id} is ambiguous")),
    }
}

fn remote_id_match(column: &str, id: &str) -> String {
    if uuid::Uuid::parse_str(id).is_ok() {
        format!("{column} = {}", sql_text_literal(id))
    } else {
        format!("{column} LIKE {}", sql_text_literal(&format!("{id}%")))
    }
}

async fn remote_event_rows(conn: &libsql::Connection, sql: &str) -> Result<Vec<serde_json::Value>> {
    let mut rows = conn
        .query(sql, ())
        .await
        .context("read remote ctx events")?;
    let mut events = Vec::new();
    while let Some(row) = rows.next().await.context("read remote ctx event row")? {
        let payload_json = row.get::<String>(6)?;
        events.push(json!({
            "event_id": row.get::<String>(0)?,
            "session_id": row.get::<Option<String>>(1)?,
            "provider": row.get::<String>(2)?,
            "role": row.get::<Option<String>>(3)?,
            "event_type": row.get::<String>(4)?,
            "occurred_at_ms": row.get::<i64>(5)?,
            "payload": serde_json::from_str::<serde_json::Value>(&payload_json).unwrap_or_else(|_| json!({"raw": payload_json})),
        }));
    }
    Ok(events)
}

async fn has_projection(conn: &libsql::Connection) -> Result<bool> {
    Ok(conn
        .query("SELECT 1 FROM ctx_turso_events LIMIT 1", ())
        .await
        .is_ok())
}

fn print_remote_search_text(value: &serde_json::Value) {
    let Some(results) = value.get("results").and_then(serde_json::Value::as_array) else {
        return;
    };
    if results.is_empty() {
        println!("no remote results");
        return;
    }
    for (index, result) in results.iter().enumerate() {
        let provider = result
            .get("provider")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let event_type = result
            .get("event_type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("event");
        let event_id = result
            .get("event_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        println!("{}. {provider} {event_type} {event_id}", index + 1);
    }
}

fn write_remote_show(
    value: &serde_json::Value,
    format: OutputFormat,
    out: Option<PathBuf>,
) -> Result<()> {
    let rendered = match format {
        OutputFormat::Json | OutputFormat::Jsonl => serde_json::to_string_pretty(value)?,
        OutputFormat::Text | OutputFormat::Markdown => serde_json::to_string_pretty(value)?,
    };
    if let Some(path) = out {
        std::fs::write(path, format!("{rendered}\n"))?;
    } else {
        println!("{rendered}");
    }
    Ok(())
}

async fn remote_status_value() -> Result<serde_json::Value> {
    let conn = connect().await?;
    let layout = remote_layout(&conn).await?;
    if matches!(layout, RemoteLayout::Snapshot) {
        let schema_version =
            remote_scalar_i64(&conn, "PRAGMA user_version", "schema version").await?;
        return Ok(json!({
            "remote_primary": true,
            "storage_layout": "imported_sqlite_snapshot",
            "schema_version": schema_version,
        }));
    }
    remote_projection_status_value(&conn).await
}

async fn remote_projection_status_value(conn: &libsql::Connection) -> Result<serde_json::Value> {
    let mut rows = conn
        .query(
            "SELECT COUNT(*), COUNT(DISTINCT provider), MIN(occurred_at_ms), MAX(occurred_at_ms) FROM ctx_turso_events",
            (),
        )
        .await
        .context("read Turso projection status")?;
    let row = rows
        .next()
        .await
        .context("read Turso projection status row")?
        .ok_or_else(|| anyhow!("Turso projection status query returned no row"))?;
    Ok(json!({
        "remote_primary": true,
        "storage_layout": "ctx_turso_projection",
        "events": row.get::<i64>(0)?,
        "providers": row.get::<i64>(1)?,
        "oldest_event_ms": row.get::<Option<i64>>(2)?,
        "newest_event_ms": row.get::<Option<i64>>(3)?,
    }))
}

async fn status(json_output: bool) -> Result<()> {
    let conn = connect().await?;
    let layout = remote_layout(&conn).await?;
    if matches!(layout, RemoteLayout::Snapshot) {
        return print_snapshot_status(&conn, json_output).await;
    }
    let mut rows = conn
        .query(
            "SELECT COUNT(*), COUNT(DISTINCT provider), MIN(occurred_at_ms), MAX(occurred_at_ms) \
             FROM ctx_turso_events",
            (),
        )
        .await
        .context("read Turso status")?;
    let row = rows
        .next()
        .await
        .context("read Turso status row")?
        .ok_or_else(|| anyhow!("Turso status query returned no row"))?;
    let value = json!({
        "remote_projection": true,
        "events": row.get::<i64>(0)?,
        "providers": row.get::<i64>(1)?,
        "oldest_event_ms": row.get::<Option<i64>>(2)?,
        "newest_event_ms": row.get::<Option<i64>>(3)?,
    });
    if json_output {
        print_json(value)?;
    } else {
        for (key, value) in value.as_object().expect("status is an object") {
            println!("{key}: {value}");
        }
    }
    Ok(())
}

fn required_env(name: &str) -> Result<String> {
    env::var(name).with_context(|| {
        format!("{name} is required; set it in your shell, never in a file or CLI argument")
    })
}

fn parse_push_batch_size(value: &str) -> std::result::Result<usize, String> {
    let size = value
        .parse::<usize>()
        .map_err(|error| format!("invalid batch size: {error}"))?;
    if !(1..=MAX_PUSH_BATCH_SIZE).contains(&size) {
        return Err(format!(
            "batch size must be between 1 and {MAX_PUSH_BATCH_SIZE}"
        ));
    }
    Ok(size)
}

fn parse_remote_sync_interval_seconds(value: &str) -> std::result::Result<u64, String> {
    let seconds = value
        .parse::<u64>()
        .map_err(|error| format!("invalid remote sync interval: {error}"))?;
    if !(MIN_REMOTE_SYNC_INTERVAL_SECONDS..=MAX_REMOTE_SYNC_INTERVAL_SECONDS).contains(&seconds) {
        return Err(format!(
            "remote sync interval must be between {MIN_REMOTE_SYNC_INTERVAL_SECONDS} and {MAX_REMOTE_SYNC_INTERVAL_SECONDS} seconds"
        ));
    }
    Ok(seconds)
}

fn parse_search_limit(value: &str) -> std::result::Result<usize, String> {
    let limit = value
        .parse::<usize>()
        .map_err(|error| format!("invalid search limit: {error}"))?;
    if !(1..=MAX_SEARCH_LIMIT).contains(&limit) {
        return Err(format!(
            "search limit must be between 1 and {MAX_SEARCH_LIMIT}"
        ));
    }
    Ok(limit)
}

fn substring_pattern(query: &str) -> String {
    format!(
        "%{}%",
        query
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    )
}

fn fts_match_query(query: &str) -> String {
    query
        .split_whitespace()
        .filter(|term| !term.is_empty())
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" AND ")
}

async fn remote_layout(conn: &libsql::Connection) -> Result<RemoteLayout> {
    // A snapshot is authoritative. A leftover projection is only a derived cache and may be
    // partial after an interrupted build.
    if has_imported_snapshot(conn).await? {
        return Ok(RemoteLayout::Snapshot);
    }
    if conn
        .query("SELECT 1 FROM ctx_turso_events LIMIT 1", ())
        .await
        .is_ok()
    {
        return Ok(RemoteLayout::Projection);
    }
    Err(anyhow!(
        "remote database is neither a ctx Turso projection nor an imported ctx SQLite snapshot"
    ))
}

async fn has_imported_snapshot(conn: &libsql::Connection) -> Result<bool> {
    Ok(conn
        .query("SELECT 1 FROM ctx_events LIMIT 1", ())
        .await
        .is_ok())
}

async fn print_snapshot_status(conn: &libsql::Connection, json_output: bool) -> Result<()> {
    let schema_version = remote_scalar_i64(conn, "PRAGMA user_version", "schema version").await?;
    let page_count = remote_scalar_i64(conn, "PRAGMA page_count", "page count").await?;
    let value = json!({
        "remote_primary": true,
        "storage_layout": "imported_sqlite_snapshot",
        "schema_version": schema_version,
        "page_count": page_count,
    });
    if json_output {
        print_json(value)?;
    } else {
        for (key, value) in value.as_object().expect("status is an object") {
            println!("{key}: {value}");
        }
    }
    Ok(())
}

async fn remote_scalar_i64(conn: &libsql::Connection, sql: &str, label: &str) -> Result<i64> {
    let mut rows = conn
        .query(sql, ())
        .await
        .with_context(|| format!("read imported snapshot {label}"))?;
    rows.next()
        .await
        .with_context(|| format!("read imported snapshot {label} row"))?
        .ok_or_else(|| anyhow!("imported snapshot returned no {label}"))?
        .get::<i64>(0)
        .with_context(|| format!("read imported snapshot {label} value"))
}

fn remote_export_allowed(event: &ctx_history_core::Event, include_local_only: bool) -> bool {
    if matches!(event.sync.visibility, Visibility::Withheld)
        || matches!(event.sync.sync_state, SyncState::Withheld)
    {
        return false;
    }
    matches!(event.sync.visibility, Visibility::SyncFull) || include_local_only
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_bounded_push_batch_size() {
        assert_eq!(parse_push_batch_size("1"), Ok(1));
        assert_eq!(parse_push_batch_size("250"), Ok(250));
        assert!(parse_push_batch_size("0").is_err());
        assert!(parse_push_batch_size("1001").is_err());
    }

    #[test]
    fn validates_bounded_remote_sync_interval() {
        assert_eq!(parse_remote_sync_interval_seconds("15"), Ok(15));
        assert_eq!(parse_remote_sync_interval_seconds("300"), Ok(300));
        assert!(parse_remote_sync_interval_seconds("14").is_err());
        assert!(parse_remote_sync_interval_seconds("3601").is_err());
    }

    #[test]
    fn canonicalizes_provider_source_dedupe_keys_for_cross_mac_merging() {
        assert_eq!(
            canonical_provider_source_dedupe_key(
                "codex",
                Some("session-123"),
                "provider-source:a5dcd0d3-41d1-7bad-90ee-e1ba0b64be32:2:fnv1a64:abc",
            ),
            Some("provider:codex:session-123:2:fnv1a64:abc".to_owned())
        );
    }

    #[test]
    fn leaves_source_dedupe_key_unchanged_without_a_stable_session_id() {
        assert_eq!(
            canonical_provider_source_dedupe_key(
                "codex",
                None,
                "provider-source:a5dcd0d3-41d1-7bad-90ee-e1ba0b64be32:2:fnv1a64:abc",
            ),
            None
        );
    }

    #[test]
    fn validates_bounded_search_limit() {
        assert_eq!(parse_search_limit("200"), Ok(200));
        assert!(parse_search_limit("0").is_err());
        assert!(parse_search_limit("201").is_err());
    }

    #[test]
    fn escapes_like_wildcards_in_queries() {
        assert_eq!(substring_pattern("50%_off\\now"), "%50\\%\\_off\\\\now%");
    }

    #[test]
    fn quotes_sql_text_literals_for_transactional_batches() {
        assert_eq!(sql_text_literal("a'quoted value"), "'a''quoted value'");
    }
}
