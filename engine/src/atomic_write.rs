// Copyright 2026 Vladimir Pankratov
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Replacing a generated file, rather than emptying it and writing it again.
//!
//! `File::create` truncates the destination and then writes into it, so a
//! build which stops part-way - Ctrl-C, a full disk, a killed job - leaves
//! half a file where a whole one was. Nothing downstream can tell: the macro
//! `include!`s whatever is there, and a truncated header is a header which
//! merely declares less than it did. A rename is atomic where a truncate and
//! a write are not, so the previous output stays in place until the new one
//! is complete, and a build which stopped leaves the output of the build
//! before it.
//!
//! What a rename costs, where writing through an existing handle did not:
//! the output directory has to be writable, and on Windows the destination
//! must not be held open by another process against deletion. What the new
//! file does not inherit from the one it replaces is anything outside the
//! permission bits - an ACL, an extended attribute - and, where this process
//! may not reassign an owner, the owner and group.

use std::io::Write;
use std::path::Path;
use tempfile::NamedTempFile;

/// Writes `content` to `destination`, replacing what is there in one step.
pub(crate) fn write_atomically(destination: &Path, content: &[u8]) -> std::io::Result<()> {
    // Through a symlink rather than onto it: a truncating write went through
    // one, and a rename onto the link would replace the link with a file of
    // its own instead. It is also the path the registry keeps its record
    // under, so the file being guarded and the file being written are the
    // same file.
    let destination = &crate::output_registry::destination_to_write(destination)?;
    // Staged where the rename will land, since a rename across filesystems is
    // not atomic - it fails - and the system temporary directory is routinely
    // a different filesystem from a build directory.
    let staging_dir = destination.parent().unwrap_or_else(|| Path::new("."));
    let mut staged = new_output_file(staging_dir, destination)?;
    staged.write_all(content)?;
    staged.persist(destination).map_err(|e| e.error)?;
    Ok(())
}

/// A file to stage an output in, in the directory the rename will land in.
///
/// A temporary file is created readable only by its owner and the rename
/// carries that mode along, where `File::create` left an existing file's mode
/// alone and let the umask decide a new one. So the mode is chosen at `open`
/// time, which is both what the umask applies to and early enough that a
/// destination narrower than the umask is never briefly a wider file with
/// contents in it.
///
/// The rename installs a new inode, so the destination's owner and group are
/// reapplied where this process is allowed to; an ACL or extended attribute
/// set on the destination itself is not carried across at all. What the
/// output directory confers - set-group-id, a default ACL - reaches the
/// replacement as it reached the original.
#[cfg(unix)]
fn new_output_file(staging_dir: &Path, destination: &Path) -> std::io::Result<NamedTempFile> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let existing = std::fs::metadata(destination).ok();
    let mode = existing
        .as_ref()
        .map(|metadata| metadata.permissions().mode() & 0o777);
    let staged = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(mode.unwrap_or(0o666)))
        .tempfile_in(staging_dir)?;
    if let Some(mode) = mode {
        // `open` subtracted the umask, which must not narrow a mode the
        // destination already had.
        staged
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    if let Some(existing) = existing {
        // Reassigning an owner takes privilege this process need not have,
        // and a build which `File::create` completed must not start failing
        // over it. So this is best effort: the owner and group are restored
        // where that is permitted, and where it is not the replacement keeps
        // the ones the directory gave it, which may not be the ones the
        // destination had.
        let _ =
            std::os::unix::fs::fchown(staged.as_file(), Some(existing.uid()), Some(existing.gid()));
    }
    Ok(staged)
}

/// Windows has no mode to carry: a temporary file inherits the directory's
/// ACL exactly as `File::create` did.
#[cfg(not(unix))]
fn new_output_file(staging_dir: &Path, _destination: &Path) -> std::io::Result<NamedTempFile> {
    NamedTempFile::new_in(staging_dir)
}

/// What is pinned here is where the staging file is made and what the rename
/// carries. What is not: that the rename itself is atomic, which is the
/// filesystem's contract and not observable from a test, and that a process
/// killed between the write and the rename leaves the old file - the same
/// contract, from the other side. The test below stands in for it by dropping
/// a staged file rather than by killing anything.
#[cfg(test)]
mod tests {
    use super::{new_output_file, write_atomically};
    use std::io::Write;

    /// A rename is only atomic within one filesystem, so the staging file has
    /// to be made in the directory the output will land in and not in the
    /// system temporary directory.
    ///
    #[test]
    fn the_staging_file_is_made_where_the_output_will_land() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("gen0.cxx");
        let staged = new_output_file(dir.path(), &destination).unwrap();
        assert_eq!(staged.path().parent(), Some(dir.path()));
    }

    /// A write which does not reach its rename leaves the previous output
    /// where it was, and leaves nothing else behind either.
    #[test]
    fn an_unfinished_write_leaves_the_previous_output_alone() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("gen0.cxx");
        std::fs::write(&destination, "the previous output").unwrap();

        let mut staged = new_output_file(dir.path(), &destination).unwrap();
        staged.write_all(b"half of the next").unwrap();
        drop(staged);

        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            "the previous output"
        );
        let left_behind: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(left_behind, ["gen0.cxx"]);
    }

    /// And a write which does reach its rename replaces the output, leaving
    /// no staging file behind.
    #[test]
    fn a_finished_write_replaces_the_output() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("gen0.cxx");
        std::fs::write(&destination, "the previous output").unwrap();
        write_atomically(&destination, b"the next output").unwrap();
        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            "the next output"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    /// A rename installs a new file where the old one was, so a mode the
    /// destination had - a build directory deliberately kept private, say -
    /// has to be put back on it. A temporary file is owner-only by default,
    /// which is what would otherwise be left there.
    #[cfg(unix)]
    #[test]
    fn the_mode_the_destination_had_is_kept() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("gen0.cxx");
        std::fs::write(&destination, "the previous output").unwrap();
        std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o640)).unwrap();
        write_atomically(&destination, b"the next output").unwrap();
        assert_eq!(
            std::fs::metadata(&destination)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
    }

    /// A symlinked output is written through, as a truncating write was:
    /// replacing the link with a file of its own would leave whatever it
    /// pointed at stale, and the registry which guards these writes keeps its
    /// record under the path the link leads to.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_output_is_written_through() {
        let dir = tempfile::tempdir().unwrap();
        let backing = dir.path().join("backing.h");
        std::fs::write(&backing, "the previous output").unwrap();
        let destination = dir.path().join("cxxgen.h");
        std::os::unix::fs::symlink(&backing, &destination).unwrap();

        write_atomically(&destination, b"the next output").unwrap();

        assert_eq!(
            std::fs::read_to_string(&backing).unwrap(),
            "the next output"
        );
        assert!(
            std::fs::symlink_metadata(&destination)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link was replaced instead of written through"
        );
    }

    /// A symlink which leads back to itself has no file at the end of it to
    /// write, and a rename does not follow the link it lands on - so it is
    /// reported rather than replaced by a file of its own.
    #[cfg(unix)]
    #[test]
    fn a_symlink_which_leads_back_to_itself_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("cxxgen.h");
        std::os::unix::fs::symlink(&destination, &destination).unwrap();

        write_atomically(&destination, b"the next output")
            .expect_err("a symlink loop was written over");
        assert!(
            std::fs::symlink_metadata(&destination)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link was replaced by a file"
        );
    }

    /// A file which is new gets the mode `File::create` would have given it,
    /// rather than the owner-only mode a temporary file is made with -
    /// otherwise every generated file would be unreadable by anything but the
    /// account which built it. Compared against a file this test creates the
    /// old way rather than against a constant, since the umask decides both.
    #[cfg(unix)]
    #[test]
    fn a_new_file_gets_the_mode_it_always_did() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let expected = dir.path().join("as-it-was.cxx");
        std::fs::File::create(&expected).unwrap();
        let destination = dir.path().join("gen0.cxx");
        write_atomically(&destination, b"the first output").unwrap();
        let mode_of =
            |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode_of(&destination), mode_of(&expected));
    }
}
