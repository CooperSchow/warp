//! Rebuilds this crate whenever a file is added under the asset folders it
//! embeds, not only when a file it already embeds changes.
//!
//! `RustEmbed` embeds each file it finds with `include_bytes!`, so Cargo
//! rebuilds when one of those files changes, but it can't learn about a file
//! added since the last build. An incremental build would then ship without the
//! new file, and the app would draw nothing wherever it's used. Cargo watches a
//! directory named here for any change inside it, which closes that gap.

fn main() {
    // Kept in sync with the `folder` and `include` attributes in `lib.rs`.
    println!("cargo:rerun-if-changed=../../app/assets/bundled");
    println!("cargo:rerun-if-changed=../../app/assets/async");
}
