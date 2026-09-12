use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use instant::Instant;
use warpui::{AppContext, Entity, ModelContext};

use crate::search::command_palette::codex_conversations::search_item::CodexSessionItem;
use crate::search::command_palette::mixer::CommandPaletteItemAction;
use crate::search::data_source::{Query, QueryResult};
use crate::search::mixer::{DataSourceRunErrorWrapper, SyncDataSource};
use crate::terminal::cli_agent_sessions::history::codex::{
    codex_sessions_dir, list_rollout_paths, read_session_summary, rollout_mtime, CodexSession,
};

/// How long a loaded session snapshot is reused before re-reading the Codex
/// store from disk. Short enough that a conversation you just started shows up
/// the next time you open the palette, long enough that a burst of keystrokes
/// in one search reuses a single read.
const SESSION_CACHE_TTL: Duration = Duration::from_secs(2);

/// Most rollouts summarized for the list.
///
/// The store grows without bound and this runs on the main thread, so the list
/// is capped rather than complete. Rollouts are visited newest-first, so the
/// cap only ever drops history far older than anything worth reopening — and it
/// puts a ceiling on the work a cold palette open can do, which is what the
/// equivalent Claude provider had to be retrofitted with after it froze the UI
/// parsing a full history.
const MAX_SESSIONS: usize = 300;

/// A rollout summarized at a known modification time. Reused as-is while the
/// file on disk is unchanged, so a refresh costs a directory walk plus one
/// `stat` per file instead of re-reading anything.
struct CachedSummary {
    mtime: Option<SystemTime>,
    session: CodexSession,
}

/// Datasource that lists past Codex CLI sessions as command-palette entries.
///
/// The session list is refreshed when the cached snapshot is missing or older
/// than [`SESSION_CACHE_TTL`], so sessions created after the app launched appear
/// without a restart. A refresh only re-reads rollouts whose mtime changed.
pub struct DataSource {
    cache: Mutex<Option<(Instant, Arc<Vec<CodexSession>>)>>,
    /// Per-rollout summaries, keyed by path.
    summaries: Mutex<HashMap<PathBuf, CachedSummary>>,
}

impl DataSource {
    pub fn new(_ctx: &mut ModelContext<Self>) -> Self {
        Self {
            cache: Mutex::new(None),
            summaries: Mutex::new(HashMap::new()),
        }
    }

    /// Returns the session list, re-reading from disk if the cached snapshot is
    /// missing or has expired.
    fn sessions(&self) -> Arc<Vec<CodexSession>> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let is_fresh = cache
            .as_ref()
            .is_some_and(|(loaded_at, _)| loaded_at.elapsed() < SESSION_CACHE_TTL);
        if !is_fresh {
            *cache = Some((Instant::now(), Arc::new(self.load_sessions())));
        }
        Arc::clone(&cache.as_ref().expect("cache populated above").1)
    }

    /// Rebuilds the session list, re-summarizing only rollouts whose
    /// modification time changed since the last pass.
    ///
    /// This runs on the main thread (the command palette's sync data sources do
    /// not get their own thread), so it must stay cheap no matter how large the
    /// user's Codex history is. Two things keep it so: only the newest
    /// [`MAX_SESSIONS`] rollouts are considered, and each is read with a bounded
    /// head scan that stops at the first real prompt.
    fn load_sessions(&self) -> Vec<CodexSession> {
        let Some(root) = codex_sessions_dir().filter(|root| root.is_dir()) else {
            return Vec::new();
        };

        // Newest-first from the paths alone, so the cap keeps recent history.
        let paths: Vec<PathBuf> = list_rollout_paths(&root)
            .into_iter()
            .take(MAX_SESSIONS)
            .collect();
        let live: HashSet<&PathBuf> = paths.iter().collect();
        let mut summaries = self.summaries.lock().unwrap_or_else(|e| e.into_inner());
        let mut sessions: Vec<CodexSession> = Vec::with_capacity(paths.len());

        for path in &paths {
            let mtime = rollout_mtime(path);
            let reusable = summaries
                .get(path)
                .is_some_and(|cached| cached.mtime == mtime && mtime.is_some());
            if !reusable {
                match read_session_summary(path) {
                    Some(session) => {
                        summaries.insert(path.clone(), CachedSummary { mtime, session });
                    }
                    None => {
                        summaries.remove(path);
                        continue;
                    }
                }
            }
            if let Some(cached) = summaries.get(path) {
                // A rollout with no prompt the user typed is scaffolding — a
                // session that was opened and abandoned. Listing those would
                // bury the real ones under "(untitled session)" rows.
                if cached.session.first_prompt.is_none() {
                    continue;
                }
                sessions.push(cached.session.clone());
            }
        }

        // Drop entries for rollouts that are gone or have aged past the cap.
        summaries.retain(|path, _| live.contains(path));

        sessions
    }
}

impl SyncDataSource for DataSource {
    type Action = CommandPaletteItemAction;

    fn run_query(
        &self,
        query: &Query,
        _app: &AppContext,
    ) -> Result<Vec<QueryResult<Self::Action>>, DataSourceRunErrorWrapper> {
        let needle = query.text.trim().to_lowercase();
        let sessions = self.sessions();

        // Sessions are already newest-first; score by position so the mixer
        // preserves recency order (newest = highest score).
        let results = sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| {
                needle.is_empty()
                    || session.display_title().to_lowercase().contains(&needle)
                    // Also match the whole first prompt, so content keywords
                    // (a client name, a file) find a session whose one-line
                    // title doesn't happen to mention them.
                    || session
                        .first_prompt
                        .as_deref()
                        .unwrap_or_default()
                        .to_lowercase()
                        .contains(&needle)
                    || session
                        .cwd
                        .as_deref()
                        .unwrap_or_default()
                        .to_lowercase()
                        .contains(&needle)
            })
            .map(|(index, session)| {
                QueryResult::from(CodexSessionItem::new(session.clone(), -(index as f64)))
            })
            .collect();
        Ok(results)
    }
}

impl Entity for DataSource {
    type Event = ();
}
