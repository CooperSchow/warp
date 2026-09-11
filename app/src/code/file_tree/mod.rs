//! File picker component for rendering expandable folder structures.

#[cfg_attr(not(feature = "local_fs"), allow(dead_code, unused_imports))]
mod delete_confirmation_dialog;
pub mod snapshot;

#[cfg_attr(not(feature = "local_fs"), allow(dead_code, unused_imports))]
mod view;

pub use view::*;
