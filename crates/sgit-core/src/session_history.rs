//! Relocate per-provider agent session history when a worktree moves.
//!
//! Previously invoked from `stokd repo migrate` (moved to `sgit repo migrate` in
//! S4). Kept as a library for future config-repositories / relocate paths that
//! still need provider history rewrites after a worktree path change.
//!
//! `git worktree move` / `fs::rename` leave agent providers keying session
//! history by the working-directory path, so after a move native resume
//! (`claude --resume`, `codex resume`, Gemini, Grok) finds nothing at the new
//! location — the history is orphaned at the old path key. This module follows
//! the move: for each provider whose store exists, it relocates the history from
//! the old path key to the new one (MOVE semantics — the old key is removed).
#![allow(dead_code)]
//!
//! Provider keying (verified against the live stores and the telemetry ingest
//! adapters in `crate::telemetry::ingest`):
//!   - Claude: `~/.claude/projects/<encoded-cwd>/*.jsonl`; dir name = the path
//!     with every non-alphanumeric char replaced by `-`. Some records also embed
//!     an absolute `cwd`. → move the keyed dir + rewrite embedded cwd prefixes.
//!   - Codex: `~/.codex/sessions/<date>/rollout-*.jsonl`; cwd lives in
//!     `session_meta.payload.cwd` inside the file (date-keyed dir, NOT
//!     path-keyed). → rewrite the in-file cwd prefix; no directory move.
//!   - Gemini: `~/.gemini/tmp/<sha256(path)>/{logs.json,chats/}`. → move the
//!     hashed dir + rewrite embedded path prefixes. Best-effort: a no-op when
//!     the `sha256(from)` dir is absent.
//!   - Grok: `~/.grok/grok.db` (SQLite); `sessions.cwd_last`/`cwd_at_start` and
//!     `workspaces.canonical_path`. → UPDATE those columns for matching rows.
//!
//! Each provider is best-effort and only acts when its store for the old path
//! exists ("if they exist"); a provider error is captured as a `Failed` outcome
//! and never aborts the worktree move that already succeeded.

use std::fs;
use std::path::{Path, PathBuf};

/// Resolved on-disk roots for every provider session store. Threaded explicitly
/// (rather than read from a global `$HOME`) so callers — and especially tests —
/// can point each provider at a sandbox without touching the real home.
#[derive(Debug, Clone)]
pub struct StoreRoots {
    pub claude_projects: PathBuf,
    pub codex_sessions: PathBuf,
    pub gemini_tmp: PathBuf,
    pub grok_db: PathBuf,
}

impl StoreRoots {
    /// Roots under the real user home (`$HOME`, falling back to the OS home).
    pub fn system() -> Self {
        let home = std::env::var("HOME")
            .ok()
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty())
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        Self::under_home(&home)
    }

    /// Roots under an explicit home directory (tests, alternate homes).
    pub fn under_home(home: &Path) -> Self {
        Self {
            claude_projects: home.join(".claude").join("projects"),
            codex_sessions: home.join(".codex").join("sessions"),
            gemini_tmp: home.join(".gemini").join("tmp"),
            grok_db: home.join(".grok").join("grok.db"),
        }
    }
}

/// What happened for a single provider during a relocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderAction {
    /// A keyed directory was moved to the new path key (Claude, Gemini).
    Relocated { detail: String },
    /// In-place content (file bodies or DB rows) was rewritten (Codex, Grok).
    Rewrote { items: usize },
    /// The provider had no history for the old path — nothing to do.
    Skipped { reason: String },
    /// The provider store was present but the relocation errored. Surfaced as a
    /// warning; never aborts the migration.
    Failed { error: String },
}

/// One provider's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderOutcome {
    pub provider: &'static str,
    pub action: ProviderAction,
}

/// Relocate the session history of every provider whose store exists from the
/// `from` worktree path to the `to` worktree path. Returns one outcome per
/// provider; a provider whose store is absent is `Skipped`, and a provider whose
/// relocation errors is `Failed` (a warning) — the aggregate call never fails.
pub fn migrate_session_history(from: &Path, to: &Path, roots: &StoreRoots) -> Vec<ProviderOutcome> {
    if from == to {
        let skipped = |provider| ProviderOutcome {
            provider,
            action: ProviderAction::Skipped {
                reason: "source and destination are identical".to_string(),
            },
        };
        return vec![
            skipped("claude"),
            skipped("codex"),
            skipped("gemini"),
            skipped("grok"),
        ];
    }

    vec![
        wrap("claude", migrate_claude(from, to, &roots.claude_projects)),
        wrap("codex", migrate_codex(from, to, &roots.codex_sessions)),
        wrap("gemini", migrate_gemini(from, to, &roots.gemini_tmp)),
        wrap("grok", migrate_grok(from, to, &roots.grok_db)),
    ]
}

fn wrap(provider: &'static str, result: Result<ProviderAction, String>) -> ProviderOutcome {
    ProviderOutcome {
        provider,
        action: result.unwrap_or_else(|error| ProviderAction::Failed { error }),
    }
}

/// Claude encodes the cwd into the projects dir name by replacing every
/// non-alphanumeric character with `-` (e.g. `/Users/stoked` → `-Users-stoked`,
/// `/a/.git` → `-a--git`). No collapsing of consecutive dashes.
pub fn claude_encode_path(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Gemini namespaces chat history under `tmp/<sha256(path)>/`. The key is the
/// lowercase hex SHA-256 of the absolute path string.
pub fn gemini_hash_path(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(path.to_string_lossy().as_bytes());
    hex::encode(hasher.finalize())
}

/// Replace `from` with `to` everywhere it appears as a path PREFIX — i.e. only
/// when the match is followed by a path boundary (`/`, a quote, whitespace, or
/// end-of-string), so `/opt/dev/x` is rewritten but `/opt/dev/xyz` is not.
pub fn rewrite_path_prefix(text: &str, from: &Path, to: &Path) -> String {
    let from = from.to_string_lossy();
    let to = to.to_string_lossy();
    if from.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(idx) = rest.find(from.as_ref()) {
        let after = &rest[idx + from.len()..];
        let at_boundary = match after.chars().next() {
            None => true,
            Some(c) => c == '/' || c == '"' || c == '\'' || c == '\\' || c.is_whitespace(),
        };
        out.push_str(&rest[..idx]);
        if at_boundary {
            out.push_str(&to);
        } else {
            out.push_str(&rest[idx..idx + from.len()]);
        }
        rest = &rest[idx + from.len()..];
    }
    out.push_str(rest);
    out
}

fn migrate_claude(from: &Path, to: &Path, projects_root: &Path) -> Result<ProviderAction, String> {
    let src = projects_root.join(claude_encode_path(from));
    if !src.is_dir() {
        return Ok(ProviderAction::Skipped {
            reason: format!("no claude history at {}", src.display()),
        });
    }
    let dst = projects_root.join(claude_encode_path(to));
    move_dir_merge(&src, &dst)?;
    let rewritten = rewrite_files_under(&dst, from, to, |_| true)?;
    Ok(ProviderAction::Relocated {
        detail: format!("{} → {} ({rewritten} file(s) rewritten)", src.display(), dst.display()),
    })
}

fn migrate_codex(from: &Path, to: &Path, sessions_root: &Path) -> Result<ProviderAction, String> {
    if !sessions_root.is_dir() {
        return Ok(ProviderAction::Skipped {
            reason: format!("no codex store at {}", sessions_root.display()),
        });
    }
    let is_rollout = |p: &Path| {
        p.file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
            .unwrap_or(false)
    };
    let rewritten = rewrite_files_under(sessions_root, from, to, is_rollout)?;
    if rewritten == 0 {
        return Ok(ProviderAction::Skipped {
            reason: "no codex rollouts reference the source path".to_string(),
        });
    }
    Ok(ProviderAction::Rewrote { items: rewritten })
}

fn migrate_gemini(from: &Path, to: &Path, tmp_root: &Path) -> Result<ProviderAction, String> {
    let src = tmp_root.join(gemini_hash_path(from));
    if !src.is_dir() {
        return Ok(ProviderAction::Skipped {
            reason: format!("no gemini history at {}", src.display()),
        });
    }
    let dst = tmp_root.join(gemini_hash_path(to));
    move_dir_merge(&src, &dst)?;
    let rewritten = rewrite_files_under(&dst, from, to, |_| true)?;
    Ok(ProviderAction::Relocated {
        detail: format!("{} → {} ({rewritten} file(s) rewritten)", src.display(), dst.display()),
    })
}

fn migrate_grok(from: &Path, to: &Path, db_path: &Path) -> Result<ProviderAction, String> {
    if !db_path.is_file() {
        return Ok(ProviderAction::Skipped {
            reason: format!("no grok store at {}", db_path.display()),
        });
    }
    let conn = rusqlite::Connection::open(db_path)
        .map_err(|e| format!("open grok store {}: {e}", db_path.display()))?;

    let mut changed = 0usize;
    // (table, key column, value columns) — only the cwd-bearing columns.
    let updates: &[(&str, &str, &[&str])] = &[
        ("sessions", "id", &["cwd_last", "cwd_at_start"]),
        ("workspaces", "id", &["canonical_path"]),
    ];
    for (table, key_col, value_cols) in updates {
        if !table_exists(&conn, table)? {
            continue;
        }
        for col in *value_cols {
            if !column_exists(&conn, table, col)? {
                continue;
            }
            changed += rewrite_sqlite_column(&conn, table, key_col, col, from, to)?;
        }
    }

    if changed == 0 {
        return Ok(ProviderAction::Skipped {
            reason: "no grok rows reference the source path".to_string(),
        });
    }
    Ok(ProviderAction::Rewrote { items: changed })
}

fn table_exists(conn: &rusqlite::Connection, table: &str) -> Result<bool, String> {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
        [table],
        |_| Ok(()),
    )
    .map(|_| true)
    .or_else(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => Ok(false),
        other => Err(format!("probe table {table}: {other}")),
    })
}

fn column_exists(conn: &rusqlite::Connection, table: &str, column: &str) -> Result<bool, String> {
    // PRAGMA table_info cannot be parameterized; the table name comes from a
    // fixed in-code allowlist (sessions/workspaces), never user input.
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|e| format!("prepare table_info({table}): {e}"))?;
    let cols = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| format!("query table_info({table}): {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read table_info({table}): {e}"))?;
    Ok(cols.iter().any(|c| c == column))
}

/// Rewrite a single text column's path-prefix for every row whose value matches,
/// returning the number of rows changed. Done row-by-row in Rust (not SQL
/// `replace()`) so the path-boundary rule in `rewrite_path_prefix` is honored.
fn rewrite_sqlite_column(
    conn: &rusqlite::Connection,
    table: &str,
    key_col: &str,
    value_col: &str,
    from: &Path,
    to: &Path,
) -> Result<usize, String> {
    // Column/table names are from a fixed in-code allowlist, never user input.
    let select = format!("SELECT {key_col}, {value_col} FROM {table} WHERE {value_col} IS NOT NULL");
    let mut stmt = conn
        .prepare(&select)
        .map_err(|e| format!("prepare select on {table}.{value_col}: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|e| format!("query {table}.{value_col}: {e}"))?
        .collect::<Result<Vec<(String, String)>, _>>()
        .map_err(|e| format!("read {table}.{value_col}: {e}"))?;

    let mut changed = 0usize;
    let update = format!("UPDATE {table} SET {value_col} = ?1 WHERE {key_col} = ?2");
    for (key, value) in rows {
        let rewritten = rewrite_path_prefix(&value, from, to);
        if rewritten != value {
            conn.execute(&update, rusqlite::params![rewritten, key])
                .map_err(|e| format!("update {table}.{value_col}: {e}"))?;
            changed += 1;
        }
    }
    Ok(changed)
}

/// Move `src` to `dst` with MOVE semantics. Fast path is `fs::rename`; on a
/// cross-device error (or when `dst` already exists) it merges entries
/// recursively and removes `src`.
fn move_dir_merge(src: &Path, dst: &Path) -> Result<(), String> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    if !dst.exists() && fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    // Merge path: dst exists, or rename failed (e.g. cross-device).
    fs::create_dir_all(dst).map_err(|e| format!("create {}: {e}", dst.display()))?;
    for entry in fs::read_dir(src).map_err(|e| format!("read {}: {e}", src.display()))? {
        let entry = entry.map_err(|e| format!("read entry in {}: {e}", src.display()))?;
        let from_path = entry.path();
        let to_path = dst.join(entry.file_name());
        let file_type = entry
            .file_type()
            .map_err(|e| format!("stat {}: {e}", from_path.display()))?;
        if file_type.is_dir() {
            move_dir_merge(&from_path, &to_path)?;
        } else {
            if let Some(parent) = to_path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("create {}: {e}", parent.display()))?;
            }
            if fs::rename(&from_path, &to_path).is_err() {
                fs::copy(&from_path, &to_path)
                    .map_err(|e| format!("copy {} → {}: {e}", from_path.display(), to_path.display()))?;
                fs::remove_file(&from_path)
                    .map_err(|e| format!("remove {}: {e}", from_path.display()))?;
            }
        }
    }
    fs::remove_dir_all(src).map_err(|e| format!("remove {}: {e}", src.display()))?;
    Ok(())
}

/// Rewrite the `from`→`to` path prefix in every regular file under `root` for
/// which `keep` returns true and whose content actually contains `from`.
/// Returns the number of files changed. Writes via a temp sibling + rename so a
/// crash never leaves a half-written history file.
fn rewrite_files_under(
    root: &Path,
    from: &Path,
    to: &Path,
    keep: impl Fn(&Path) -> bool,
) -> Result<usize, String> {
    let from_str = from.to_string_lossy().to_string();
    let mut changed = 0usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries {
            let entry = entry.map_err(|e| format!("read entry in {}: {e}", dir.display()))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|e| format!("stat {}: {e}", path.display()))?;
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() || !keep(&path) {
                continue;
            }
            let content = match fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue, // binary / unreadable — skip
            };
            if !content.contains(&from_str) {
                continue;
            }
            let rewritten = rewrite_path_prefix(&content, from, to);
            if rewritten == content {
                continue;
            }
            let tmp = path.with_extension("stokd-migrate-tmp");
            fs::write(&tmp, rewritten.as_bytes())
                .map_err(|e| format!("write {}: {e}", tmp.display()))?;
            fs::rename(&tmp, &path)
                .map_err(|e| format!("replace {}: {e}", path.display()))?;
            changed += 1;
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn claude_encode_path_replaces_non_alphanumeric_with_dash() {
        assert_eq!(claude_encode_path(Path::new("/Users/stoked")), "-Users-stoked");
        assert_eq!(claude_encode_path(Path::new("/a/.git")), "-a--git");
        assert_eq!(
            claude_encode_path(Path::new("/opt/dev/stokd-cloud/x")),
            "-opt-dev-stokd-cloud-x"
        );
    }

    #[test]
    fn gemini_hash_path_is_lowercase_hex_sha256() {
        let h = gemini_hash_path(Path::new("/Users/stoked"));
        assert_eq!(h.len(), 64, "sha256 hex is 64 chars");
        assert!(
            h.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "lowercase hex only: {h}"
        );
        // Known vector: sha256("/Users/stoked").
        assert_eq!(
            h,
            "4cbc6279011fe901a71fdd6b96bdacbb6aab26707005f844c6db8364f79f0e76"
        );
    }

    #[test]
    fn rewrite_path_prefix_respects_component_boundaries() {
        let from = Path::new("/opt/dev/x");
        let to = Path::new("/new/y");
        assert_eq!(
            rewrite_path_prefix("\"cwd\":\"/opt/dev/x\"", from, to),
            "\"cwd\":\"/new/y\""
        );
        assert_eq!(
            rewrite_path_prefix("/opt/dev/x/sub/file", from, to),
            "/new/y/sub/file"
        );
        // A longer sibling name must NOT be rewritten.
        assert_eq!(rewrite_path_prefix("/opt/dev/xyz", from, to), "/opt/dev/xyz");
        // An unrelated path is unchanged.
        assert_eq!(rewrite_path_prefix("/other/path", from, to), "/other/path");
    }

    #[test]
    fn migrate_claude_moves_dir_and_rewrites_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let from = PathBuf::from("/opt/dev/acme/oldleaf");
        let to = PathBuf::from("/opt/wt/acme/widgets/oldleaf");
        let enc_from = claude_encode_path(&from);
        let enc_to = claude_encode_path(&to);
        write(
            &projects.join(&enc_from).join("sess.jsonl"),
            "{\"type\":\"x\",\"cwd\":\"/opt/dev/acme/oldleaf\"}\n{\"cwd\":\"/opt/dev/acme/oldleaf/src\"}\n",
        );

        let action = migrate_claude(&from, &to, &projects).unwrap();
        assert!(matches!(action, ProviderAction::Relocated { .. }), "got {action:?}");
        assert!(!projects.join(&enc_from).exists(), "old encoded dir removed");
        let moved = projects.join(&enc_to).join("sess.jsonl");
        assert!(moved.exists(), "history moved to new encoded dir");
        let body = fs::read_to_string(&moved).unwrap();
        assert!(
            body.contains("\"cwd\":\"/opt/wt/acme/widgets/oldleaf\""),
            "cwd rewritten: {body}"
        );
        assert!(
            body.contains("/opt/wt/acme/widgets/oldleaf/src"),
            "nested cwd rewritten: {body}"
        );
        assert!(!body.contains("/opt/dev/acme/oldleaf"), "no stale path remains: {body}");
    }

    #[test]
    fn migrate_claude_skips_when_no_history() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        fs::create_dir_all(&projects).unwrap();
        let action =
            migrate_claude(Path::new("/opt/dev/none"), Path::new("/opt/wt/none"), &projects).unwrap();
        assert!(matches!(action, ProviderAction::Skipped { .. }), "got {action:?}");
    }

    #[test]
    fn migrate_codex_rewrites_cwd_in_matching_rollouts_only() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        let from = PathBuf::from("/opt/dev/acme/oldleaf");
        let to = PathBuf::from("/opt/wt/acme/widgets/oldleaf");
        let matching = sessions.join("2026/06/19").join("rollout-match.jsonl");
        write(
            &matching,
            "{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/opt/dev/acme/oldleaf\"}}\n",
        );
        let unrelated_body = "{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/somewhere/else\"}}\n";
        let unrelated = sessions.join("2026/06/19").join("rollout-other.jsonl");
        write(&unrelated, unrelated_body);

        let action = migrate_codex(&from, &to, &sessions).unwrap();
        assert!(matches!(action, ProviderAction::Rewrote { .. }), "got {action:?}");
        assert!(matching.exists(), "no directory move for codex");
        let m = fs::read_to_string(&matching).unwrap();
        assert!(m.contains("/opt/wt/acme/widgets/oldleaf"), "matching cwd rewritten: {m}");
        assert!(!m.contains("/opt/dev/acme/oldleaf"), "stale cwd gone: {m}");
        assert_eq!(
            fs::read_to_string(&unrelated).unwrap(),
            unrelated_body,
            "unrelated rollout byte-for-byte unchanged"
        );
    }

    #[test]
    fn migrate_gemini_moves_hash_dir_and_rewrites() {
        let tmp = tempfile::tempdir().unwrap();
        let gtmp = tmp.path().join("tmp");
        let from = PathBuf::from("/opt/dev/acme/oldleaf");
        let to = PathBuf::from("/opt/wt/acme/widgets/oldleaf");
        let hfrom = gemini_hash_path(&from);
        let hto = gemini_hash_path(&to);
        write(
            &gtmp.join(&hfrom).join("logs.json"),
            "[{\"cwd\":\"/opt/dev/acme/oldleaf\"}]",
        );

        let action = migrate_gemini(&from, &to, &gtmp).unwrap();
        assert!(matches!(action, ProviderAction::Relocated { .. }), "got {action:?}");
        assert!(!gtmp.join(&hfrom).exists(), "old hash dir removed");
        let moved = gtmp.join(&hto).join("logs.json");
        assert!(moved.exists(), "history moved to new hash dir");
        let body = fs::read_to_string(&moved).unwrap();
        assert!(body.contains("/opt/wt/acme/widgets/oldleaf"), "embedded path rewritten: {body}");
        assert!(!body.contains("/opt/dev/acme/oldleaf"), "no stale path remains: {body}");
    }

    #[test]
    fn migrate_gemini_noop_when_hash_dir_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let gtmp = tmp.path().join("tmp");
        fs::create_dir_all(&gtmp).unwrap();
        let action =
            migrate_gemini(Path::new("/opt/dev/x"), Path::new("/opt/wt/x"), &gtmp).unwrap();
        assert!(matches!(action, ProviderAction::Skipped { .. }), "got {action:?}");
    }

    #[test]
    fn migrate_grok_updates_cwd_columns_for_matching_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("grok.db");
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE workspaces(id TEXT PRIMARY KEY, canonical_path TEXT);
                 CREATE TABLE sessions(id TEXT PRIMARY KEY, cwd_last TEXT, cwd_at_start TEXT, repo_slug TEXT);
                 INSERT INTO workspaces VALUES('w1','/opt/dev/acme/oldleaf');
                 INSERT INTO workspaces VALUES('w2','/unrelated/path');
                 INSERT INTO sessions VALUES('s1','/opt/dev/acme/oldleaf/src','/opt/dev/acme/oldleaf','w1');
                 INSERT INTO sessions VALUES('s2','/unrelated/path','/unrelated/path','w2');",
            )
            .unwrap();
        }
        let from = PathBuf::from("/opt/dev/acme/oldleaf");
        let to = PathBuf::from("/opt/wt/acme/widgets/oldleaf");

        let action = migrate_grok(&from, &to, &db).unwrap();
        assert!(matches!(action, ProviderAction::Rewrote { .. }), "got {action:?}");

        let conn = rusqlite::Connection::open(&db).unwrap();
        let q = |sql: &str| -> String { conn.query_row(sql, [], |r| r.get::<_, String>(0)).unwrap() };
        assert_eq!(
            q("SELECT cwd_last FROM sessions WHERE id='s1'"),
            "/opt/wt/acme/widgets/oldleaf/src"
        );
        assert_eq!(
            q("SELECT cwd_at_start FROM sessions WHERE id='s1'"),
            "/opt/wt/acme/widgets/oldleaf"
        );
        assert_eq!(
            q("SELECT canonical_path FROM workspaces WHERE id='w1'"),
            "/opt/wt/acme/widgets/oldleaf"
        );
        // Unrelated rows untouched.
        assert_eq!(q("SELECT cwd_last FROM sessions WHERE id='s2'"), "/unrelated/path");
        assert_eq!(q("SELECT canonical_path FROM workspaces WHERE id='w2'"), "/unrelated/path");
    }

    #[test]
    fn migrate_session_history_skips_absent_providers_without_error() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = StoreRoots::under_home(&tmp.path().join("home"));
        let outcomes =
            migrate_session_history(Path::new("/opt/dev/x"), Path::new("/opt/wt/x"), &roots);
        assert_eq!(outcomes.len(), 4, "one outcome per provider");
        assert!(
            outcomes
                .iter()
                .all(|o| matches!(o.action, ProviderAction::Skipped { .. })),
            "all skip when stores absent: {outcomes:?}"
        );
    }

    #[test]
    fn migrate_session_history_noop_when_from_equals_to() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = StoreRoots::under_home(&tmp.path().join("home"));
        let p = Path::new("/opt/dev/x");
        let outcomes = migrate_session_history(p, p, &roots);
        assert!(
            outcomes
                .iter()
                .all(|o| matches!(o.action, ProviderAction::Skipped { .. })),
            "identical from/to is a no-op: {outcomes:?}"
        );
    }
}
