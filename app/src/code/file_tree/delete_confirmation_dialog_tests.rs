use std::fs;
use std::io;
#[cfg(unix)]
use std::path::Path;

use warp_util::standardized_path::StandardizedPath;

use super::{
    changed_since_request_message, delete_failed_message, no_longer_exists_message, ItemIdentity,
    ItemKind, PendingDelete,
};

/// A pending delete for `name`. Its path is never touched; the identity is borrowed from the
/// system temp folder, which only has to exist.
fn pending_delete(name: &str, kind: ItemKind, child_count: Option<usize>) -> PendingDelete {
    let temp_dir = std::env::temp_dir();
    let metadata = fs::symlink_metadata(&temp_dir).expect("the temp folder exists");
    let local_path = temp_dir.join(name);
    PendingDelete {
        std_path: StandardizedPath::try_from_local(&local_path).expect("the path is absolute"),
        local_path,
        kind,
        display_name: name.to_owned(),
        child_count,
        identity: ItemIdentity::from_metadata(&metadata),
    }
}

#[test]
fn file_copy_warns_that_the_delete_is_permanent() {
    let target = pending_delete("notes.txt", ItemKind::File, None);
    assert_eq!(target.title(), "Delete \"notes.txt\"?");
    assert_eq!(
        target.body(),
        "This file will be permanently deleted. You can't undo this."
    );
}

#[test]
fn folder_copy_gives_the_count_only_when_it_is_known() {
    let warning =
        "This folder and everything in it will be permanently deleted. You can't undo this.";
    let body =
        |child_count| pending_delete("victim-folder", ItemKind::Directory, child_count).body();

    assert_eq!(body(None), warning);
    assert_eq!(body(Some(0)), format!("{warning} It contains 0 items."));
    assert_eq!(body(Some(1)), format!("{warning} It contains 1 item."));
    assert_eq!(body(Some(12)), format!("{warning} It contains 12 items."));
}

#[test]
fn symlink_copy_says_the_target_is_not_affected() {
    let target = pending_delete("link-to-keep", ItemKind::Symlink, None);
    assert_eq!(target.title(), "Delete \"link-to-keep\"?");
    assert_eq!(
        target.body(),
        "This symbolic link will be permanently deleted. The item it points to won't be affected."
    );
}

#[test]
fn toast_messages_name_the_item() {
    let error = io::Error::from(io::ErrorKind::PermissionDenied);
    assert_eq!(
        delete_failed_message("a.txt", &error),
        format!("Couldn't delete \"a.txt\": {error}")
    );
    assert_eq!(
        changed_since_request_message("a.txt"),
        "\"a.txt\" changed since you asked. Nothing was deleted."
    );
    assert_eq!(
        no_longer_exists_message("a.txt"),
        "\"a.txt\" no longer exists. Nothing was deleted."
    );
}

#[cfg(unix)]
#[test]
fn item_kind_comes_from_lstat_so_links_stay_links() {
    let temp = tempfile::tempdir().expect("create a temp folder");
    let file = temp.path().join("file.txt");
    let folder = temp.path().join("folder");
    let link_to_file = temp.path().join("link-to-file");
    let link_to_folder = temp.path().join("link-to-folder");
    fs::write(&file, "file\n").expect("write the file");
    fs::create_dir(&folder).expect("create the folder");
    std::os::unix::fs::symlink(&file, &link_to_file).expect("link to the file");
    std::os::unix::fs::symlink(&folder, &link_to_folder).expect("link to the folder");

    let kind = |path: &Path| {
        ItemKind::from_file_type(fs::symlink_metadata(path).expect("lstat").file_type())
    };
    assert_eq!(kind(&file), ItemKind::File);
    assert_eq!(kind(&folder), ItemKind::Directory);
    assert_eq!(kind(&link_to_file), ItemKind::Symlink);
    assert_eq!(kind(&link_to_folder), ItemKind::Symlink);
    // `Path::is_dir` follows the link, which is how the old code could treat it as a folder.
    assert!(link_to_folder.is_dir());
}

#[cfg(unix)]
#[test]
fn identity_survives_edits_and_changes_when_the_file_is_replaced() {
    use std::io::Write as _;

    let temp = tempfile::tempdir().expect("create a temp folder");
    let file = temp.path().join("file.txt");
    let identity =
        |path: &Path| ItemIdentity::from_metadata(&fs::symlink_metadata(path).expect("lstat"));
    fs::write(&file, "first\n").expect("write the file");
    let original = identity(&file);

    fs::OpenOptions::new()
        .append(true)
        .open(&file)
        .and_then(|mut opened| opened.write_all(b"more\n"))
        .expect("edit the file in place");
    assert_eq!(
        identity(&file),
        original,
        "an edit in place keeps the identity"
    );

    // Replace the file the way an editor's atomic save does. Both files exist at once, so the
    // new one can't reuse the old inode.
    let replacement = temp.path().join("file.txt.new");
    fs::write(&replacement, "replacement\n").expect("write the replacement");
    assert!(
        file.starts_with(temp.path()),
        "only replace files inside this test's own temp folder"
    );
    fs::rename(&replacement, &file).expect("replace the file");
    assert_ne!(
        identity(&file),
        original,
        "a replaced file has a new identity"
    );
}
