//! Command-palette provider that lists past Codex CLI sessions and reopens one
//! with `codex resume <id>` in a new tab. Gated by
//! `FeatureFlag::CodexConversations`.

mod data_source;
mod search_item;

pub use data_source::DataSource;
