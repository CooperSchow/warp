use std::fs;
use std::sync::Arc;

use repo_metadata::file_tree_store::{FileTreeDirectoryEntryState, FileTreeEntryState};
use repo_metadata::{FileMetadata, FileTreeEntry};
use warp_util::standardized_path::StandardizedPath;

use super::{rename_refusal, sort_entries_for_file_tree};

fn std_path(s: &str) -> StandardizedPath {
    StandardizedPath::try_new(s).expect("test path should be valid")
}

fn dir_state(path: &str) -> FileTreeEntryState {
    FileTreeEntryState::Directory(FileTreeDirectoryEntryState {
        path: Arc::new(std_path(path)),
        ignored: false,
        loaded: true,
    })
}

fn file_state(path: &str) -> FileTreeEntryState {
    FileTreeEntryState::File(FileMetadata::from_standardized(std_path(path), false).into())
}

#[test]
fn sort_entries_for_file_tree_is_antisymmetric_for_missing_entries() {
    let root = std_path("/repo");
    let mut entry = FileTreeEntry::new_for_directory(Arc::new(root.clone()));
    entry.insert_child_state(&root, dir_state("/repo/src"));
    entry.insert_child_state(&root, file_state("/repo/README.md"));

    let paths = [
        std_path("/repo/src"),       // present (directory)
        std_path("/repo/README.md"), // present (file)
        std_path("/repo/ghost_a"),   // missing
        std_path("/repo/ghost_b"),   // missing
    ];

    for a in &paths {
        for b in &paths {
            let ab = sort_entries_for_file_tree(a, b, &entry);
            let ba = sort_entries_for_file_tree(b, a, &entry);
            assert_eq!(
                ab.reverse(),
                ba,
                "comparator not antisymmetric for ({}, {}): cmp(a,b) = {:?}, cmp(b,a) = {:?}",
                a.as_str(),
                b.as_str(),
                ab,
                ba,
            );
        }
    }
}

#[test]
fn sort_entries_for_file_tree_sorts_without_panicking_on_missing_children() {
    let root = std_path("/repo");
    let mut entry = FileTreeEntry::new_for_directory(Arc::new(root.clone()));
    entry.insert_child_state(&root, dir_state("/repo/src"));

    // Multiple missing entries are required to reliably trigger the sort's
    // total-order violation check.
    let mut paths = [
        std_path("/repo/src"),
        std_path("/repo/ghost_a"),
        std_path("/repo/ghost_b"),
        std_path("/repo/ghost_c"),
        std_path("/repo/ghost_d"),
        std_path("/repo/ghost_e"),
    ];

    paths.sort_by(|a, b| sort_entries_for_file_tree(a, b, &entry));
}

#[test]
fn sort_entries_for_file_tree_uses_natural_order_for_numbered_files() {
    let root = std_path("/repo");
    let mut entry = FileTreeEntry::new_for_directory(Arc::new(root.clone()));
    for name in ["L1", "L2", "L3", "L10", "L11", "L12"] {
        entry.insert_child_state(&root, file_state(&format!("/repo/{name}.tsx")));
    }

    let mut paths = [
        std_path("/repo/L10.tsx"),
        std_path("/repo/L2.tsx"),
        std_path("/repo/L1.tsx"),
        std_path("/repo/L12.tsx"),
        std_path("/repo/L3.tsx"),
        std_path("/repo/L11.tsx"),
    ];
    paths.sort_by(|a, b| sort_entries_for_file_tree(a, b, &entry));

    let sorted: Vec<&str> = paths.iter().map(|p| p.as_str()).collect();
    assert_eq!(
        sorted,
        [
            "/repo/L1.tsx",
            "/repo/L2.tsx",
            "/repo/L3.tsx",
            "/repo/L10.tsx",
            "/repo/L11.tsx",
            "/repo/L12.tsx",
        ]
    );
}

#[test]
fn rename_is_refused_when_another_item_has_the_new_name() {
    let temp = tempfile::tempdir().expect("create a temp folder");
    let a = temp.path().join("a.txt");
    let b = temp.path().join("b.txt");
    fs::write(&a, "a\n").expect("write a.txt");
    fs::write(&b, "b\n").expect("write b.txt");

    assert_eq!(
        rename_refusal(&a, &b).as_deref(),
        Some("Couldn't rename \"a.txt\": \"b.txt\" already exists.")
    );
}

#[cfg(unix)]
#[test]
fn a_symbolic_link_takes_its_name_even_when_it_points_nowhere() {
    let temp = tempfile::tempdir().expect("create a temp folder");
    let a = temp.path().join("a.txt");
    let link = temp.path().join("dangling");
    fs::write(&a, "a\n").expect("write a.txt");
    std::os::unix::fs::symlink(temp.path().join("missing"), &link).expect("create the link");

    assert!(rename_refusal(&a, &link).is_some());
}

#[test]
fn rename_goes_ahead_to_a_free_name_or_the_same_name() {
    let temp = tempfile::tempdir().expect("create a temp folder");
    let a = temp.path().join("a.txt");
    fs::write(&a, "a\n").expect("write a.txt");

    assert_eq!(rename_refusal(&a, &temp.path().join("c.txt")), None);
    assert_eq!(rename_refusal(&a, &a), None);
}

#[test]
fn rename_can_change_only_the_case_of_a_name() {
    let temp = tempfile::tempdir().expect("create a temp folder");
    let lower = temp.path().join("notes.txt");
    let upper = temp.path().join("NOTES.txt");
    fs::write(&lower, "notes\n").expect("write notes.txt");

    // On a disk that ignores case, "NOTES.txt" already names the file being renamed; on one
    // that doesn't, the name is free. Either way the rename goes ahead.
    assert_eq!(rename_refusal(&lower, &upper), None);

    // Where case matters, a different file can have the other spelling, and then it's refused.
    if fs::symlink_metadata(&upper).is_err() {
        fs::write(&upper, "other\n").expect("write NOTES.txt");
        assert!(rename_refusal(&lower, &upper).is_some());
    }
}
