use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use instant::Instant;
use warp_util::standardized_path::StandardizedPath;

use super::{
    canonical_location, changed_since_request_message, click_confirms, delete_failed_message,
    delete_refusal_for_location, folder_partly_deleted_message, no_longer_exists_message,
    real_home_dir, test_build_delete_refusal, test_build_delete_roots, ItemIdentity, ItemKind,
    PendingDelete, ARM_DELAY,
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
        folder_partly_deleted_message("victim-folder"),
        "Couldn't finish deleting \"victim-folder\". Some items may already be gone."
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

#[test]
fn delete_is_armed_half_a_second_after_the_dialog_appears() {
    assert_eq!(ARM_DELAY, Duration::from_millis(500));
}

/// The arming rule, checked over the whole range of timings rather than a few samples. For every
/// press from a second before the dialog appears to two seconds after, and a spread of hold
/// times, a click confirms exactly when its press began once Delete was armed.
#[test]
fn a_click_on_delete_confirms_only_if_its_press_began_once_delete_was_armed() {
    let ms = Duration::from_millis;
    let start = Instant::now();
    // The dialog appears a second in, so presses can begin before it did.
    let opened_at = start + ms(1_000);
    for arm_delay in [Duration::ZERO, ms(1), ARM_DELAY, ms(1_000)] {
        let armed_at = opened_at + arm_delay;
        for press in (0..=3_000u64).step_by(5) {
            let pressed_at = start + ms(press);
            for hold in [0u64, 1, 40, 120, 499, 500, 501, 2_000] {
                let clicked_at = pressed_at + ms(hold);
                let confirms = click_confirms(opened_at, Some(pressed_at), clicked_at, arm_delay);
                let context = format!("press at {press} ms, held {hold} ms, delay {arm_delay:?}");
                assert_eq!(confirms, pressed_at >= armed_at, "{context}");
                // The two rules as stated: a click that completes within the delay never
                // confirms, and neither does one whose press began before the dialog appeared.
                if clicked_at < armed_at || pressed_at < opened_at {
                    assert!(!confirms, "{context}");
                }
            }
        }

        // A click whose press the dialog never saw doesn't confirm, however late it completes.
        for release in (0..=3_000u64).step_by(5) {
            assert!(
                !click_confirms(opened_at, None, start + ms(release), arm_delay),
                "release at {release} ms with no press, delay {arm_delay:?}"
            );
        }
        // Nor does a release that comes before the press it's paired with.
        assert!(!click_confirms(
            opened_at,
            Some(armed_at + ms(10)),
            armed_at + ms(5),
            arm_delay
        ));
    }
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

/// `rm file.txt; echo new > file.txt`. The old file is gone before the new one exists, so on
/// Linux the new file is often given the old inode, and only the birth time tells them apart.
#[cfg(unix)]
#[test]
fn identity_changes_when_the_file_is_deleted_and_recreated() {
    let temp = tempfile::tempdir().expect("create a temp folder");
    let file = temp.path().join("file.txt");
    let identity =
        |path: &Path| ItemIdentity::from_metadata(&fs::symlink_metadata(path).expect("lstat"));
    fs::write(&file, "old\n").expect("write the file");
    if let Err(error) = fs::symlink_metadata(&file).expect("lstat").created() {
        // Without a birth time the check has only the inode to go on.
        eprintln!("Skipping: this file system doesn't record birth times ({error})");
        return;
    }
    let original = identity(&file);

    assert!(
        file.starts_with(temp.path()),
        "only delete files inside this test's own temp folder"
    );
    fs::remove_file(&file).expect("delete the file");
    // Linux stamps times at the kernel's clock tick, a few milliseconds long. A file recreated
    // on the old inode within the same tick would get the same birth time, so step past it.
    std::thread::sleep(Duration::from_millis(50));
    fs::write(&file, "new\n").expect("recreate the file");
    assert_ne!(
        identity(&file),
        original,
        "a recreated file has a new identity"
    );
}

// ── The guard compiled into test builds ─────────────────────────────

#[test]
fn the_test_build_guard_allows_the_temp_folder_and_nothing_outside_it() {
    let temp = tempfile::tempdir().expect("create a temp folder");
    let file = temp.path().join("victim.txt");
    fs::write(&file, "victim\n").expect("write the file");

    assert_eq!(test_build_delete_refusal(&file), None);
    assert!(
        test_build_delete_refusal(&std::env::temp_dir()).is_some(),
        "not the temp folder itself"
    );
    assert!(
        test_build_delete_refusal(Path::new("/")).is_some(),
        "not a path with no parent folder"
    );
    // "..", enough times, leaves the temp folder. Nothing at the path exists.
    let climbing = temp
        .path()
        .join("..")
        .join("..")
        .join("..")
        .join("warp-test-guard-probe");
    assert!(test_build_delete_refusal(&climbing).is_some());
    #[cfg(unix)]
    assert!(test_build_delete_refusal(Path::new("/usr/warp-test-guard-probe")).is_some());
}

#[cfg(unix)]
#[test]
fn the_test_build_guard_resolves_the_folders_on_the_way_but_not_the_item_itself() {
    let temp = tempfile::tempdir().expect("create a temp folder");
    let root = dunce::canonicalize(temp.path()).expect("resolve the temp folder");
    let allowed = root.join("allowed");
    let outside = root.join("outside");
    let forbidden = allowed.join("Desktop");
    for folder in [&allowed, &outside, &forbidden] {
        fs::create_dir_all(folder).expect("create a folder");
    }
    let link_out = allowed.join("link-out");
    std::os::unix::fs::symlink(&outside, &link_out).expect("link to the outside folder");
    let refusal = |path: &Path| {
        let location = canonical_location(path).expect("the path resolves");
        delete_refusal_for_location(
            path,
            &location,
            std::slice::from_ref(&allowed),
            std::slice::from_ref(&forbidden),
        )
    };

    assert_eq!(refusal(&allowed.join("victim.txt")), None);
    assert!(refusal(&outside.join("victim.txt")).is_some());
    assert!(
        refusal(&allowed.join("..").join("outside").join("victim.txt")).is_some(),
        "\"..\" can't climb out"
    );
    assert!(
        refusal(&link_out.join("victim.txt")).is_some(),
        "a link to a folder can't carry a delete out"
    );
    assert_eq!(
        refusal(&link_out),
        None,
        "deleting the link itself removes only the link"
    );
    assert!(
        refusal(&forbidden.join("victim.txt")).is_some(),
        "a refused folder wins even inside an allowed one"
    );
    assert!(refusal(&allowed).is_some(), "not the allowed folder itself");
}

#[test]
fn the_test_build_guard_always_refuses_the_real_desktop_and_documents() {
    let home = real_home_dir().expect("the user has a home folder");
    let (allowed, forbidden) = test_build_delete_roots();
    let temp_dir = dunce::canonicalize(std::env::temp_dir()).expect("resolve the temp folder");
    assert!(allowed.contains(&temp_dir), "the temp folder is allowed");

    // Decided from the paths alone, so nothing under the real Desktop or Documents is read.
    // Even allowing the whole home folder doesn't let them through.
    let allowed_with_home: Vec<PathBuf> = allowed.iter().cloned().chain([home.clone()]).collect();
    for folder in ["Desktop", "Documents"] {
        assert!(
            forbidden.contains(&home.join(folder)),
            "{folder} is on the refused list"
        );
        let path = home.join(folder).join("warp-test-guard-probe.txt");
        assert!(
            delete_refusal_for_location(&path, &path, &allowed_with_home, &forbidden).is_some(),
            "{folder} is refused"
        );
    }
}
