use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use super::{
    is_synthetic_user_text, list_rollout_paths, read_session_cwd, read_session_summary,
    resume_command_for, session_id_from_file_stem, session_id_from_path, session_ids_for_cwd_in,
    session_path_for_id_in,
};

const ID_A: &str = "01a093a2-8893-7c42-8165-5acc9b86b857";
const ID_B: &str = "01a093ec-22ba-7363-8163-fd7007b27347";
const ID_C: &str = "01a08fa2-e61d-7c23-870c-de54908077a2";

/// A `session_meta` line, the shape Codex actually writes: the real one also
/// embeds ~22 KB of base instructions, which nothing here reads.
fn meta_line(id: &str, cwd: &str, timestamp: &str) -> String {
    format!(
        r#"{{"timestamp":"{timestamp}","ordinal":0,"type":"session_meta","payload":{{"session_id":"{id}","id":"{id}","timestamp":"{timestamp}","cwd":"{cwd}","originator":"Codex Desktop","cli_version":"0.154.0","source":"vscode","history_mode":"legacy"}}}}"#
    )
}

fn user_line(text: &str) -> String {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    format!(
        r#"{{"type":"response_item","payload":{{"type":"message","id":"msg_1","role":"user","content":[{{"type":"input_text","text":"{escaped}"}}]}}}}"#
    )
}

fn developer_line(text: &str) -> String {
    format!(
        r#"{{"type":"response_item","payload":{{"type":"message","id":"msg_0","role":"developer","content":[{{"type":"input_text","text":"{text}"}}]}}}}"#
    )
}

/// Writes a rollout at the date-partitioned path Codex uses, so tests exercise
/// the real directory layout rather than a flat folder.
fn write_rollout(root: &Path, day: &str, stamp: &str, id: &str, lines: &[String]) -> PathBuf {
    let (year, rest) = day.split_once('-').expect("day is yyyy-mm-dd");
    let (month, dom) = rest.split_once('-').expect("day is yyyy-mm-dd");
    let dir = root.join(year).join(month).join(dom);
    fs::create_dir_all(&dir).expect("create day dir");
    let path = dir.join(format!("rollout-{stamp}-{id}.jsonl"));
    fs::write(&path, format!("{}\n", lines.join("\n"))).expect("write rollout");
    path
}

#[test]
fn pulls_the_conversation_id_out_of_a_rollout_file_name() {
    let stem = format!("rollout-2026-09-11T21-21-37-{ID_A}");
    assert_eq!(session_id_from_file_stem(&stem).as_deref(), Some(ID_A));
    assert_eq!(
        session_id_from_path(Path::new(&format!("/x/y/{stem}.jsonl"))).as_deref(),
        Some(ID_A)
    );
}

#[test]
fn rejects_file_names_that_are_not_rollouts() {
    // The timestamp segment is hyphen-separated too, so a field split would
    // mis-parse these; the trailing-36-characters rule must not be fooled.
    assert_eq!(
        session_id_from_file_stem("rollout-2026-09-11T21-21-37"),
        None
    );
    assert_eq!(session_id_from_file_stem(&format!("index-{ID_A}")), None);
    assert_eq!(
        session_id_from_file_stem("rollout-2026-09-11T21-21-37-not-a-uuid-at-all-x"),
        None
    );
    assert_eq!(session_id_from_file_stem(""), None);
}

#[test]
fn summarizes_metadata_and_the_first_real_prompt() {
    let dir = TempDir::new().expect("tempdir");
    let path = write_rollout(
        dir.path(),
        "2026-09-11",
        "2026-09-11T21-21-37",
        ID_A,
        &[
            meta_line(ID_A, "/Users/me/proj", "2026-09-11T21:21:37.683Z"),
            developer_line("<app-context>desktop</app-context>"),
            user_line("<recommended_plugins>\nAirtable\n</recommended_plugins>"),
            user_line("Overhaul the cadence manager UI"),
            user_line("a later prompt that must not win"),
        ],
    );

    let session = read_session_summary(&path).expect("summary");
    assert_eq!(session.session_id, ID_A);
    assert_eq!(session.cwd.as_deref(), Some("/Users/me/proj"));
    assert_eq!(
        session.first_prompt.as_deref(),
        Some("Overhaul the cadence manager UI")
    );
    assert_eq!(session.display_title(), "Overhaul the cadence manager UI");
    assert_eq!(session.originator.as_deref(), Some("Codex Desktop"));
    assert_eq!(session.started_at_label(), Some("09-11 21:21".to_string()));
    assert_eq!(session.project_label(), "proj");
    assert_eq!(session.resume_command(), format!("codex resume {ID_A}"));
}

#[test]
fn falls_back_to_an_untitled_label_when_every_prompt_is_scaffolding() {
    let dir = TempDir::new().expect("tempdir");
    let path = write_rollout(
        dir.path(),
        "2026-09-11",
        "2026-09-11T21-21-37",
        ID_A,
        &[
            meta_line(ID_A, "/Users/me/proj", "2026-09-11T21:21:37.683Z"),
            user_line("<task-notification>done</task-notification>"),
        ],
    );

    let session = read_session_summary(&path).expect("summary");
    assert_eq!(session.first_prompt, None);
    assert_eq!(session.display_title(), "(untitled session)");
}

/// The scaffolding rule is stated as a shape — an opening tag on the first line
/// — so it is checked across the whole family of wrappers Codex injects rather
/// than the one or two a single example would cover.
#[test]
fn treats_every_injected_wrapper_shape_as_scaffolding() {
    for wrapper in [
        "<task-notification>",
        "<command-name>/loop</command-name>",
        "<local-command-stdout>ok</local-command-stdout>",
        "<bash-input>ls</bash-input>",
        "<bash-stdout>a b c</bash-stdout>",
        "<recommended_plugins>\nAirtable\n</recommended_plugins>",
        "<environment_context>\ncwd=/x\n</environment_context>",
        "  <user_instructions>be brief</user_instructions>",
    ] {
        assert!(
            is_synthetic_user_text(wrapper),
            "expected scaffolding: {wrapper:?}"
        );
    }

    for typed in [
        "Overhaul the cadence manager UI",
        "why is 3 > 2 in this comparison?",
        "fix the a < b check",
        "",
    ] {
        assert!(
            !is_synthetic_user_text(typed),
            "expected a real prompt: {typed:?}"
        );
    }
}

#[test]
fn reads_just_the_working_directory_without_parsing_the_body() {
    let dir = TempDir::new().expect("tempdir");
    let path = write_rollout(
        dir.path(),
        "2026-09-11",
        "2026-09-11T21-21-37",
        ID_A,
        &[
            meta_line(ID_A, "/Users/me/proj", "2026-09-11T21:21:37.683Z"),
            user_line("hello"),
        ],
    );
    assert_eq!(read_session_cwd(&path).as_deref(), Some("/Users/me/proj"));
}

#[test]
fn lists_rollouts_newest_first_across_date_directories() {
    let dir = TempDir::new().expect("tempdir");
    write_rollout(
        dir.path(),
        "2026-09-08",
        "2026-09-08T10-00-00",
        ID_C,
        &[meta_line(ID_C, "/Users/me/proj", "2026-09-08T10:00:00Z")],
    );
    write_rollout(
        dir.path(),
        "2026-09-11",
        "2026-09-11T23-00-00",
        ID_B,
        &[meta_line(ID_B, "/Users/me/proj", "2026-09-11T23:00:00Z")],
    );
    write_rollout(
        dir.path(),
        "2026-09-11",
        "2026-09-11T21-21-37",
        ID_A,
        &[meta_line(ID_A, "/Users/me/proj", "2026-09-11T21:21:37Z")],
    );

    let ids: Vec<String> = list_rollout_paths(dir.path())
        .iter()
        .filter_map(|path| session_id_from_path(path))
        .collect();
    assert_eq!(ids, vec![ID_B, ID_A, ID_C]);
}

#[test]
fn resolves_only_sessions_from_the_requested_directory_newest_first() {
    let dir = TempDir::new().expect("tempdir");
    write_rollout(
        dir.path(),
        "2026-09-11",
        "2026-09-11T21-00-00",
        ID_A,
        &[meta_line(ID_A, "/Users/me/wanted", "2026-09-11T21:00:00Z")],
    );
    write_rollout(
        dir.path(),
        "2026-09-11",
        "2026-09-11T22-00-00",
        ID_B,
        &[meta_line(ID_B, "/Users/me/other", "2026-09-11T22:00:00Z")],
    );
    write_rollout(
        dir.path(),
        "2026-09-11",
        "2026-09-11T23-00-00",
        ID_C,
        &[meta_line(ID_C, "/Users/me/wanted", "2026-09-11T23:00:00Z")],
    );

    assert_eq!(
        session_ids_for_cwd_in(dir.path(), "/Users/me/wanted"),
        vec![ID_C, ID_A]
    );
    assert_eq!(
        session_ids_for_cwd_in(dir.path(), "/Users/me/nothing-here"),
        Vec::<String>::new()
    );
}

#[test]
fn finds_a_rollout_by_id_and_refuses_path_shaped_ids() {
    let dir = TempDir::new().expect("tempdir");
    let path = write_rollout(
        dir.path(),
        "2026-09-11",
        "2026-09-11T21-21-37",
        ID_A,
        &[meta_line(ID_A, "/Users/me/proj", "2026-09-11T21:21:37Z")],
    );

    assert_eq!(session_path_for_id_in(dir.path(), ID_A), Some(path));
    assert_eq!(session_path_for_id_in(dir.path(), ID_B), None);
    // Anything that isn't a bare UUID is refused before a path is built, so a
    // persisted id can never be used to walk out of the store.
    for hostile in [
        "../../etc/passwd",
        "__warp_codex_resume__",
        "",
        "01a093a2-8893-7c42-8165-5acc9b86b857/../x",
    ] {
        assert_eq!(
            session_path_for_id_in(dir.path(), hostile),
            None,
            "expected refusal: {hostile:?}"
        );
    }
}

#[test]
fn clips_a_long_prompt_to_one_line_for_display() {
    let dir = TempDir::new().expect("tempdir");
    let long = "x".repeat(400);
    let path = write_rollout(
        dir.path(),
        "2026-09-11",
        "2026-09-11T21-21-37",
        ID_A,
        &[
            meta_line(ID_A, "/Users/me/proj", "2026-09-11T21:21:37Z"),
            user_line(&format!("{long}\nsecond line")),
        ],
    );

    let title = read_session_summary(&path)
        .expect("summary")
        .display_title();
    assert!(title.ends_with('…'), "expected an ellipsis: {title:?}");
    assert!(!title.contains('\n'));
    assert_eq!(title.chars().count(), 121);
}

#[test]
fn builds_the_resume_command_from_a_bare_id() {
    assert_eq!(resume_command_for(ID_A), format!("codex resume {ID_A}"));
}
