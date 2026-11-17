//! Eqho fork: mirrors the focused pane's Claude session id into
//! `~/.warp-oss/focused-session`, so a Stream Deck plugin (which reads it
//! mtime-gated) knows which session Cooper is looking at. The file holds the
//! id and a newline, or nothing when the focused pane has no Claude session.
//! Gated behind `FeatureFlag::FocusedSessionFile`.
//!
//! ponytail: written on focus changes only; a Claude session that starts, or
//! changes id, in the already-focused pane shows up at the next focus change.

use std::path::PathBuf;

use parking_lot::Mutex;
use warp_core::features::FeatureFlag;
use warpui::{AppContext, EntityId, SingletonEntity};

use crate::terminal::cli_agent_sessions::CLIAgentSessionsModel;
use crate::terminal::CLIAgent;

/// The contents last handed to the writer; `None` before the first report.
static LATEST: Mutex<Option<String>> = Mutex::new(None);
/// Held across a write, so overlapping background writes can't interleave.
static WRITE: Mutex<()> = Mutex::new(());

/// Records that `terminal_view_id` (`None` for a non-terminal pane) is now
/// the focused pane of the active window, writing the file off the main
/// thread if what it should hold changed.
pub(crate) fn report(terminal_view_id: Option<EntityId>, ctx: &AppContext) {
    if !FeatureFlag::FocusedSessionFile.is_enabled() {
        return;
    }
    let session_id = terminal_view_id
        .and_then(|id| CLIAgentSessionsModel::as_ref(ctx).session(id))
        .filter(|session| matches!(session.agent, CLIAgent::Claude))
        .and_then(|session| session.resume_id());
    if !replace_if_changed(&mut LATEST.lock(), file_contents(session_id.as_deref())) {
        return;
    }
    ctx.background_executor()
        .spawn(async { write_latest() })
        .detach();
}

/// What the file should hold for `session_id`.
fn file_contents(session_id: Option<&str>) -> String {
    session_id.map(|id| format!("{id}\n")).unwrap_or_default()
}

/// Stores `contents` in `latest`, returning whether it differed.
fn replace_if_changed(latest: &mut Option<String>, contents: String) -> bool {
    if latest.as_deref() == Some(contents.as_str()) {
        return false;
    }
    *latest = Some(contents);
    true
}

/// Writes whatever is latest (not what this task was spawned for), so
/// writes that finish out of order still leave the newest value on disk.
fn write_latest() {
    let _write = WRITE.lock();
    let Some(contents) = LATEST.lock().clone() else {
        return;
    };
    let Some(dir) = dirs::home_dir().map(|home| home.join(".warp-oss")) else {
        return;
    };
    if let Err(e) = write_atomically(dir, &contents) {
        log::debug!("[focused_session] could not write focused-session: {e}");
    }
}

fn write_atomically(dir: PathBuf, contents: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(".focused-session.tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(tmp, dir.join("focused-session"))
}

#[cfg(test)]
#[path = "focused_session_tests.rs"]
mod tests;
