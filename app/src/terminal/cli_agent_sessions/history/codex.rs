//! Reads the Codex CLI's on-disk session store into a browsable, resume-able
//! conversation list, and resolves which conversation a restored tab reopens.
//!
//! Codex persists every session as
//! `~/.codex/sessions/<yyyy>/<mm>/<dd>/rollout-<timestamp>-<uuid>.jsonl`, one
//! JSON object per line, and reopens one natively with `codex resume <uuid>`.
//! Two properties of that layout do most of the work here: the conversation id
//! is in the file name, so existence checks and ordering cost a directory walk
//! with no file reads at all; and the directories are date-partitioned with
//! zero-padded names, so walking them in reverse lexical order walks them
//! newest-first.
//!
//! This is the Codex counterpart of the Claude reader in the parent module. It
//! is deliberately a sibling rather than a generalization: the two stores agree
//! on almost nothing but the `.jsonl` extension.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;
use walkdir::WalkDir;

/// Sentinel stored in a pane's `codex_session_id` when we know the pane was
/// running Codex (its terminal session was command-detected as Codex) but we
/// don't have an exact conversation id to resume — e.g. the Codex Warp plugin
/// wasn't emitting session events. On restore this triggers a lookup of the
/// newest unused conversation in the pane's working directory. It is
/// deliberately not a valid UUID, so it can never collide with a real id.
pub const CODEX_RESUME_SENTINEL: &str = "__warp_codex_resume__";

/// Bytes read from the head of a rollout when summarizing it for the
/// conversation list.
///
/// A rollout opens with a ~22 KB `session_meta` line (it embeds the agent's
/// whole base instructions) followed by a run of synthetic `developer` and
/// `user` messages — app context, the plugin catalogue, world state, turn
/// context — before the human's first prompt. Measured across a real store the
/// first genuine prompt sits at a median of 24 KB and a worst case of 115 KB,
/// so this budget clears every case with headroom. The scan also stops at the
/// first real prompt it finds, so the common file costs about 24 KB, not this.
const HEAD_SCAN_BYTES: u64 = 192 * 1024;

/// Most conversations returned when resolving candidates for one working
/// directory. Restore only ever needs one per pane, and no window has anywhere
/// near this many panes.
const MAX_CWD_MATCHES: usize = 64;

/// Rollout files opened while resolving conversations for a working directory.
///
/// Bounded because this runs during tab restore, where a slow pass delays the
/// window appearing. Files are visited newest-first, so the cap only ever
/// discards history far older than anything a restored tab would want.
const MAX_CWD_SCAN: usize = 512;

/// Bytes read when all that's wanted is the `session_meta` line — the working
/// directory and conversation id. That line is first and runs about 22 KB, so
/// this reads it and stops.
const META_SCAN_BYTES: u64 = 64 * 1024;

/// One browsable Codex conversation, keyed by its native session id (the value
/// passed to `codex resume <id>`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CodexSession {
    /// Native Codex session id; the resume target and the file name's suffix.
    pub session_id: String,
    /// Working directory the session ran in — used to reopen the tab in place.
    pub cwd: Option<String>,
    /// First prompt the human actually typed — the conversation list's title.
    pub first_prompt: Option<String>,
    /// When the session began, from its `session_meta` line (ISO-8601, so it
    /// sorts lexically).
    pub started_at: Option<String>,
    /// Which client created the session (`Codex Desktop`, a CLI originator, …).
    pub originator: Option<String>,
    /// Most recent write to the rollout, used to order the list.
    pub last_activity: Option<SystemTime>,
    /// Absolute path of the rollout this was summarized from.
    pub path: PathBuf,
}

impl CodexSession {
    /// The label shown in the conversation list.
    pub fn display_title(&self) -> String {
        match self.first_prompt.as_deref().map(str::trim) {
            Some(prompt) if !prompt.is_empty() => first_line_summary(prompt),
            _ => "(untitled session)".to_string(),
        }
    }

    /// The shell command that reopens this conversation in a new tab.
    pub fn resume_command(&self) -> String {
        resume_command_for(&self.session_id)
    }

    /// Short project label for display (last path component of the cwd).
    pub fn project_label(&self) -> String {
        let raw = self.cwd.clone().unwrap_or_default();
        let trimmed = raw.trim_end_matches('/');
        match trimmed.rsplit('/').next() {
            Some(label) if !label.is_empty() => label.to_string(),
            _ => "(unknown directory)".to_string(),
        }
    }

    /// Compact `09-11 21:21` stamp for the list's subtitle, from the session's
    /// own start timestamp rather than a file stat.
    pub fn started_at_label(&self) -> Option<String> {
        let stamp = self.started_at.as_deref()?;
        // `2026-09-11T21:21:37.683Z` -> `09-11 21:21`
        let date = stamp.get(5..10)?;
        let time = stamp.get(11..16)?;
        Some(format!("{date} {time}"))
    }
}

/// The command that reopens conversation `id`.
///
/// `codex resume <id>` takes the bare UUID; unlike Claude's equivalent there is
/// no "most recent conversation here" form worth using, because `--last` is not
/// scoped to a working directory. So callers always resolve a concrete id
/// first, and fall back to launching plain `codex` when they can't.
pub fn resume_command_for(id: &str) -> String {
    format!("codex resume {id}")
}

/// Resolve the Codex sessions directory (`$CODEX_HOME/sessions` or
/// `~/.codex/sessions`). Mirrors the CLI's own config-dir resolution.
pub fn codex_sessions_dir() -> Option<PathBuf> {
    let base = match std::env::var("CODEX_HOME") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        Ok(_) | Err(_) => dirs::home_dir()?.join(".codex"),
    };
    Some(base.join("sessions"))
}

/// True when `s` is a canonical hyphenated UUID. Used to pull a conversation id
/// out of a rollout file name, whose timestamp segment also contains hyphens.
fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.as_bytes().iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// The conversation id encoded in a rollout file stem
/// (`rollout-2026-09-11T21-21-37-<uuid>`).
///
/// Taken as the trailing 36 characters rather than by splitting on `-`: the
/// timestamp segment is itself hyphen-separated, so a field split would have to
/// count backwards through it.
pub fn session_id_from_file_stem(stem: &str) -> Option<String> {
    if !stem.starts_with("rollout-") {
        return None;
    }
    let id = stem.get(stem.len().checked_sub(36)?..)?;
    is_uuid(id).then(|| id.to_owned())
}

/// The conversation id for a rollout path, from its file name.
pub fn session_id_from_path(path: &Path) -> Option<String> {
    session_id_from_file_stem(path.file_stem()?.to_str()?)
}

/// Every rollout path under the store, newest-first.
///
/// Ordering comes from the paths alone — the date directories and the timestamp
/// in each file name are zero-padded, so reverse lexical order is reverse
/// chronological order. That means no `stat` calls, which is what makes this
/// cheap enough to call from the command palette's main-thread data source.
pub fn list_rollout_paths(sessions_root: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = WalkDir::new(sessions_root)
        .into_iter()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                return None;
            }
            session_id_from_path(path).map(|_| path.to_path_buf())
        })
        .collect();
    // Compare on file name, not the full path: every name starts with the same
    // `rollout-<iso timestamp>` prefix, so names sort chronologically even
    // though their parent directories are nested by date.
    paths.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    paths
}

/// True if a rollout for conversation `id` exists in the store. Used to guard
/// `codex resume <id>` on tab restore, so a stale or deleted id doesn't error
/// the restored shell.
///
/// Matches on the file name, so it opens nothing.
pub fn session_file_exists(id: &str) -> bool {
    session_path_for_id(id).is_some()
}

/// Absolute path of the rollout for conversation `id`, if the store holds one.
pub fn session_path_for_id(id: &str) -> Option<PathBuf> {
    session_path_for_id_in(&codex_sessions_dir()?, id)
}

/// [`session_path_for_id`] against an explicit store root, so the lookup can be
/// tested against a fixture directory rather than the machine's own store.
fn session_path_for_id_in(sessions_root: &Path, id: &str) -> Option<PathBuf> {
    // Only accept a bare conversation id: the id reaches us from persisted
    // snapshot state, and anything path-shaped has no business being joined.
    if !is_uuid(id) {
        return None;
    }
    let suffix = format!("-{id}.jsonl");
    WalkDir::new(sessions_root)
        .into_iter()
        .filter_map(Result::ok)
        .map(|entry| entry.path().to_path_buf())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(&suffix))
        })
}

/// Resumable Codex conversation ids for `cwd`, newest-first.
///
/// Used to give a restored tab a concrete, distinct conversation when its exact
/// id was lost, so several tabs in one directory don't all reopen the same one.
///
/// Unlike the Claude store, Codex doesn't partition by working directory — the
/// cwd lives inside each rollout's first line — so this has to open files. It
/// reads a bounded head of each (the `session_meta` line and no more), visits
/// them newest-first, and stops once it has enough or has looked at
/// [`MAX_CWD_SCAN`] of them.
pub fn session_ids_for_cwd(cwd: &str) -> Vec<String> {
    match codex_sessions_dir() {
        Some(root) => session_ids_for_cwd_in(&root, cwd),
        None => Vec::new(),
    }
}

/// [`session_ids_for_cwd`] against an explicit store root, so the resolution
/// order can be tested against a fixture directory.
fn session_ids_for_cwd_in(sessions_root: &Path, cwd: &str) -> Vec<String> {
    let root = sessions_root;
    let mut matches = Vec::new();
    for path in list_rollout_paths(root).into_iter().take(MAX_CWD_SCAN) {
        if matches.len() >= MAX_CWD_MATCHES {
            break;
        }
        let Some(id) = session_id_from_path(&path) else {
            continue;
        };
        if read_session_cwd(&path).as_deref() == Some(cwd) {
            matches.push(id);
        }
    }
    matches
}

/// The working directory recorded in a rollout's `session_meta` line.
///
/// Reads only [`META_SCAN_BYTES`], which covers that line with room to spare.
pub fn read_session_cwd(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    let mut first_line = String::new();
    BufReader::new(file.take(META_SCAN_BYTES))
        .read_line(&mut first_line)
        .ok()?;
    let value: Value = serde_json::from_str(first_line.trim_end()).ok()?;
    session_meta_payload(&value)?
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// The `session_meta` payload of a rollout's first line, if that's what it is.
fn session_meta_payload(value: &Value) -> Option<&Value> {
    (value.get("type").and_then(Value::as_str) == Some("session_meta"))
        .then(|| value.get("payload"))
        .flatten()
}

/// Summarize one rollout for the conversation list.
///
/// Reads a bounded head of the file rather than the whole thing — rollouts here
/// reach several megabytes and this runs on the main thread via the command
/// palette's sync data source — and returns as soon as it has both the metadata
/// and the first real prompt.
pub fn read_session_summary(path: &Path) -> Option<CodexSession> {
    let session_id = session_id_from_path(path)?;
    let file = File::open(path).ok()?;
    let mut session = CodexSession {
        session_id,
        path: path.to_path_buf(),
        last_activity: rollout_mtime(path),
        ..CodexSession::default()
    };

    let reader = BufReader::new(file.take(HEAD_SCAN_BYTES));
    for line in reader.lines().map_while(Result::ok) {
        if ingest_line(&mut session, &line) {
            break;
        }
    }
    Some(session)
}

/// Folds one rollout line into `session`. Returns `true` once nothing further
/// in the file can improve the summary, so the caller can stop reading.
fn ingest_line(session: &mut CodexSession, line: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return false;
    };

    if let Some(meta) = session_meta_payload(&value) {
        session.cwd = meta.get("cwd").and_then(Value::as_str).map(str::to_owned);
        session.started_at = meta
            .get("timestamp")
            .and_then(Value::as_str)
            .map(str::to_owned);
        session.originator = meta
            .get("originator")
            .and_then(Value::as_str)
            .map(str::to_owned);
        // The file name is the authority on the id, but prefer the recorded one
        // when it agrees in shape — a forked session records the id it answers
        // to, which is what `codex resume` wants.
        if let Some(id) = meta
            .get("session_id")
            .or_else(|| meta.get("id"))
            .and_then(Value::as_str)
            .filter(|id| is_uuid(id))
        {
            session.session_id = id.to_owned();
        }
        return false;
    }

    if session.first_prompt.is_some() {
        return true;
    }
    if let Some(text) = user_message_text(&value) {
        if !is_synthetic_user_text(&text) {
            session.first_prompt = Some(text);
            return true;
        }
    }
    false
}

/// The text of a `user` message line, joined across its content parts.
fn user_message_text(value: &Value) -> Option<String> {
    if value.get("type").and_then(Value::as_str) != Some("response_item") {
        return None;
    }
    let payload = value.get("payload")?;
    if payload.get("type").and_then(Value::as_str) != Some("message")
        || payload.get("role").and_then(Value::as_str) != Some("user")
    {
        return None;
    }
    let parts = payload.get("content")?.as_array()?;
    let text = parts
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then_some(text)
}

/// True when a `user` message is Codex's own scaffolding rather than something
/// the human typed.
///
/// Codex injects several of these — app context, the plugin catalogue, task
/// notifications, echoed shell input and output — and each opens with an
/// XML-ish tag on its first line. Matching that *shape* rather than a list of
/// known tag names is what keeps a newly-added wrapper from surfacing in the
/// conversation list as a title; the list of names changes with every Codex
/// release, the shape has not.
///
/// A human prompt that opens with a tag-like fragment is misread as scaffolding
/// and the session falls back to its next prompt for a title. That costs a
/// label, which is the right way round: a wrong title is a smaller harm than a
/// wall of injected context posing as one.
fn is_synthetic_user_text(text: &str) -> bool {
    let first_line = text.trim_start().lines().next().unwrap_or_default();
    first_line.starts_with('<') && first_line.contains('>')
}

/// Collapse a prompt to a single line for display, trimming it to a sane width.
fn first_line_summary(prompt: &str) -> String {
    const MAX_CHARS: usize = 120;
    let line = prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    if line.chars().count() <= MAX_CHARS {
        return line.to_string();
    }
    let clipped: String = line.chars().take(MAX_CHARS).collect();
    format!("{}…", clipped.trim_end())
}

/// Modification time of `path`, used both to order the conversation list and to
/// skip re-reading rollouts that haven't changed since they were summarized.
pub fn rollout_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

#[cfg(test)]
#[path = "codex_test.rs"]
mod tests;
