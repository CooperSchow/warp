//! Command-palette provider that lists past Claude Code sessions and reopens
//! one with `claude --resume <id>` in a new tab. Gated by
//! `FeatureFlag::ClaudeConversations`.

mod data_source;
mod search_item;

pub use data_source::DataSource;
