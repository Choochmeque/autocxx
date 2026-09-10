// Copyright 2026 Vladimir Pankratov
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Which generated file belongs to which `include_cpp!`, for as long as the
//! process generating them runs.
//!
//! Cargo hands every [`crate::Builder`] in a build script the same `OUT_DIR`
//! and tells none of them about the others. A generated file is named after
//! the block it came from - `autocxx-ffi-default-gen.rs` for a block with no
//! `name!` - so two input files whose blocks are both unnamed name one file
//! between them, and the builder which runs second decides what *both* of
//! their macros will `include!`. Nothing on either side compares the two, so
//! the loser's `include_cpp!` compiles against the winner's bindings.
//!
//! A build script is one process, so the whole set of writes is visible from
//! here and that can be reported instead. Nothing crosses a process boundary:
//! a rebuild starts empty and, given the same build script, chooses the same
//! names again.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// A file this module writes into each generation directory it records. A
/// record can then tell the directory it describes from a different directory
/// which has since been made under the same path - which is what a test
/// harness's temporary directories do - because a directory which was removed
/// took this with it.
///
/// Its own file, rather than one of the generated ones: those are the files a
/// user or a cleaning step has a reason to remove, and removing one of them
/// says nothing about the directory. What is *in* it is not read back. A
/// second process building into the same directory writes this too, and
/// treating a value it did not write as a directory it did not make would
/// throw away a record of files which are still there.
const RUN_MARKER: &str = ".autocxx-build-run";

/// What goes in the marker, for whoever finds it and wonders.
const RUN_MARKER_CONTENTS: &str = "\
Written by autocxx while generating the code in this directory, so that a\n\
build which finds this directory can tell it from a directory since made\n\
under the same name. Removing it costs nothing but that.\n";

/// The generated files one directory has been given in this process.
struct GenDirRecord {
    /// Whether the marker was written. Where it could not be - a directory
    /// which cannot be added to - the record cannot notice a directory remade
    /// under its path, and is kept rather than dropped: guarding is the
    /// point, and a directory nothing can be written to is not one a build is
    /// about to fill.
    marker_written: bool,
    /// Every path claimed or written, under the path it reaches, so that a
    /// filename autocxx picks for itself can be moved along rather than taken
    /// twice.
    taken: BTreeSet<PathBuf>,
    /// What has been written, under the path it reached rather than the path
    /// asked for: on a filesystem which ignores case, two spellings are one
    /// file.
    written: BTreeMap<PathBuf, GeneratedFile>,
}

struct GeneratedFile {
    /// Which builder wrote it. See [`OutputRegistry::builder`].
    builder: u64,
    /// A hash rather than the bytes: this record outlives every build in the
    /// process, and the integration suite runs thousands of them. It never
    /// leaves the process, so std's freedom to change the algorithm between
    /// releases costs nothing here.
    content: u64,
    /// The Rust file whose `include_cpp!` this was generated for.
    origin: PathBuf,
}

static GENERATED: Mutex<BTreeMap<PathBuf, GenDirRecord>> = Mutex::new(BTreeMap::new());

/// A build script which panicked has already failed; a poisoned lock must not
/// turn every later builder in the same process - the integration suite runs
/// them in thousands, and one failed assertion poisons this - into a second,
/// unrelated panic.
fn lock() -> MutexGuard<'static, BTreeMap<PathBuf, GenDirRecord>> {
    GENERATED.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Two blocks whose generated code would land in one file.
#[derive(Debug)]
pub(crate) struct OutputCollision {
    /// The file both of them reach, spelled as the filesystem spells it.
    pub(crate) path: PathBuf,
    /// The Rust file whose block got there first.
    pub(crate) first: PathBuf,
    /// The Rust file whose block would have overwritten it.
    pub(crate) second: PathBuf,
}

/// One builder's view of the generation directory it shares with the others.
pub(crate) struct OutputRegistry {
    gen_dir: PathBuf,
    /// This builder, as against another builder generating for the same Rust
    /// file: what a record is forgotten by is which builder wrote it, and two
    /// builders can be given one input.
    builder: u64,
    origin: PathBuf,
}

impl GenDirRecord {
    /// Whether this record still describes what is under `marker`.
    fn describes_the_directory_at(&self, marker: &Path) -> bool {
        !self.marker_written || marker.exists()
    }
}

impl OutputRegistry {
    /// `gen_dir` is the directory holding all of one builder's output, and
    /// `origin` the Rust file being generated for.
    pub(crate) fn open(gen_dir: &Path, origin: &Path) -> Self {
        let gen_dir = canonical(gen_dir);
        let marker = gen_dir.join(RUN_MARKER);
        let mut generated = lock();
        if generated
            .get(&gen_dir)
            .is_some_and(|record| !record.describes_the_directory_at(&marker))
        {
            generated.remove(&gen_dir);
        }
        generated
            .entry(gen_dir.clone())
            .or_insert_with(|| GenDirRecord {
                marker_written: std::fs::write(&marker, RUN_MARKER_CONTENTS).is_ok(),
                taken: BTreeSet::new(),
                written: BTreeMap::new(),
            });
        static BUILDERS: AtomicU64 = AtomicU64::new(0);
        Self {
            gen_dir,
            builder: BUILDERS.fetch_add(1, Ordering::Relaxed),
            origin: origin.to_path_buf(),
        }
    }

    /// Takes `path` for this builder, or answers `false` if another builder
    /// has it.
    ///
    /// For the filenames autocxx chooses rather than the user - `gen0.cxx`,
    /// `cxxgen.h` - whose numbering starts again with every builder. Moving
    /// one along is invisible to the user, and keeps two builders whose
    /// blocks *are* named apart from colliding over names no `name!`
    /// controls. Files named after a block are not claimed this way: moving
    /// those would hide the collision rather than report it.
    pub(crate) fn claim(&self, path: &Path) -> bool {
        let destination = resolve_destination(path);
        let mut generated = lock();
        let Some(record) = generated.get_mut(&self.gen_dir) else {
            return true;
        };
        record.taken.insert(destination)
    }

    /// Registers a write before it happens, refusing one which would take
    /// another block's file.
    ///
    /// Before, not after: a file whose contents already match is left alone
    /// so as not to move its timestamp, and a check made after that decision
    /// would never see the first writer at all.
    pub(crate) fn record(&self, path: &Path, content: &[u8]) -> Result<(), OutputCollision> {
        let destination = resolve_destination(path);
        let content = hash_of(content);
        let mut generated = lock();
        let Some(record) = generated.get_mut(&self.gen_dir) else {
            return Ok(());
        };
        if let Some(first) = record.written.get(&destination) {
            // The same bytes are not a collision: whichever block claims the
            // name, the file says the same thing.
            if first.content != content {
                return Err(OutputCollision {
                    path: destination,
                    first: first.origin.clone(),
                    second: self.origin.clone(),
                });
            }
            // The file stays the first writer's: it is there because of that
            // write, and a later writer of the same bytes taking it over
            // would be able to forget it.
            record.taken.insert(destination);
            return Ok(());
        }
        record.taken.insert(destination.clone());
        record.written.insert(
            destination,
            GeneratedFile {
                builder: self.builder,
                content,
                origin: self.origin.clone(),
            },
        );
        Ok(())
    }

    /// Drops the record of a write which then failed, so that a caller which
    /// handles the failure and writes the file again is not refused by the
    /// record of its own unfinished attempt. The name stays claimed: another
    /// builder stepping onto a number this one was using would be no better
    /// for having been given it.
    pub(crate) fn forget(&self, path: &Path) {
        let destination = resolve_destination(path);
        let mut generated = lock();
        let Some(record) = generated.get_mut(&self.gen_dir) else {
            return;
        };
        if record
            .written
            .get(&destination)
            .is_some_and(|file| file.builder == self.builder)
        {
            record.written.remove(&destination);
        }
    }
}

fn hash_of(content: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

/// The file a name reaches: the spelling the filesystem itself gives it, since
/// two spellings it treats as one name one file, and the far end of a symlink,
/// since a write goes through one.
///
/// Unresolvable is not an error here. The key only has to be the same for two
/// writers of one file and different for writers of two, and a path nothing
/// can be learnt about answers that as well as it can be answered; a path
/// which cannot be opened at all fails at the write with the reason.
pub(crate) fn resolve_destination(path: &Path) -> PathBuf {
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return resolved;
    }
    // Not there to be resolved, which is the ordinary case of a file not
    // written yet. Its directory is, and a link whose target does not exist
    // yet still has to be followed.
    let path = follow_links(path);
    path.parent()
        .zip(path.file_name())
        .and_then(|(parent, name)| Some(std::fs::canonicalize(parent).ok()?.join(name)))
        .unwrap_or(path)
}

/// A symlink chain walked by hand, which is what `canonicalize` will not do
/// for a link whose target does not exist yet.
fn follow_links(path: &Path) -> PathBuf {
    let mut path = path.to_path_buf();
    // Linux gives up at 40 too; a chain longer than that is a loop, and the
    // write which follows fails on it.
    for _ in 0..40 {
        let Ok(target) = std::fs::read_link(&path) else {
            return path; // not a link, or not there at all
        };
        path = match path.parent() {
            Some(parent) if target.is_relative() => parent.join(target),
            _ => target,
        };
    }
    path
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::{OutputRegistry, GENERATED};
    use std::path::{Path, PathBuf};

    /// A generation directory of its own for each test: the record this
    /// module keeps is process-wide, so tests which shared a directory would
    /// see each other's writes.
    struct Gendir {
        _tmp: tempfile::TempDir,
        dir: PathBuf,
    }

    impl Gendir {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path().to_path_buf();
            Self { _tmp: tmp, dir }
        }

        fn builder(&self, origin: &str) -> OutputRegistry {
            OutputRegistry::open(&self.dir, Path::new(origin))
        }
    }

    /// Two builders, two input files, one filename: the second one is told
    /// whose file it is rather than taking it.
    #[test]
    fn a_second_builder_cannot_write_over_the_first() {
        let gendir = Gendir::new();
        let first = gendir.builder("first.rs");
        let second = gendir.builder("second.rs");
        let path = gendir.dir.join("autocxx-ffi-default-gen.rs");
        first.record(&path, b"first bindings").unwrap();
        let collision = second
            .record(&path, b"second bindings")
            .expect_err("the second builder took the first builder's file");
        assert_eq!(collision.first, Path::new("first.rs"));
        assert_eq!(collision.second, Path::new("second.rs"));
        assert!(collision.path.ends_with("autocxx-ffi-default-gen.rs"));
    }

    /// Byte-identical output is not a collision: whichever block claims the
    /// name, the file says the same thing. `cxx.h` is written by every
    /// builder and is the reason this matters.
    #[test]
    fn identical_contents_are_not_a_collision() {
        let gendir = Gendir::new();
        let first = gendir.builder("first.rs");
        let second = gendir.builder("second.rs");
        let path = gendir.dir.join("cxx.h");
        first.record(&path, b"the same header").unwrap();
        second.record(&path, b"the same header").unwrap();
    }

    /// A name autocxx chose for itself goes to one builder, so the next one
    /// can move along to the next number instead of colliding.
    #[test]
    fn a_name_is_claimed_by_one_builder_only() {
        let gendir = Gendir::new();
        let first = gendir.builder("first.rs");
        let second = gendir.builder("second.rs");
        assert!(first.claim(&gendir.dir.join("gen0.cxx")));
        assert!(!second.claim(&gendir.dir.join("gen0.cxx")));
        assert!(second.claim(&gendir.dir.join("gen1.cxx")));
    }

    /// Writing a file claims its name too, so a builder cannot be handed a
    /// number another builder has already written.
    #[test]
    fn writing_a_file_claims_its_name() {
        let gendir = Gendir::new();
        let first = gendir.builder("first.rs");
        let second = gendir.builder("second.rs");
        let path = gendir.dir.join("gen0.cxx");
        first.record(&path, b"first").unwrap();
        assert!(!second.claim(&path));
    }

    /// A record describes a directory, and a directory which has been
    /// removed and remade under the same path - which is what a test
    /// harness's temporary directories do - is not that directory. Its
    /// marker goes with it, so the record is dropped rather than reporting
    /// collisions with files nobody can read.
    #[test]
    fn a_directory_remade_under_one_path_starts_over() {
        let gendir = Gendir::new();
        let first = gendir.builder("first.rs");
        let path = gendir.dir.join("autocxx-ffi-default-gen.rs");
        first.record(&path, b"first bindings").unwrap();
        std::fs::write(&path, "first bindings").unwrap();
        assert!(!first.claim(&path));

        std::fs::remove_dir_all(&gendir.dir).unwrap();
        std::fs::create_dir(&gendir.dir).unwrap();
        let second = gendir.builder("second.rs");
        second
            .record(&path, b"second bindings")
            .expect("the record of a directory which no longer exists was kept");
        assert!(second.claim(&gendir.dir.join("gen0.cxx")));
    }

    /// And a directory which is still there keeps its record, however much of
    /// what is in it is removed: a record is dropped for a directory which is
    /// gone, not for one whose files are.
    #[test]
    fn a_directory_which_lost_a_file_keeps_its_record() {
        let gendir = Gendir::new();
        let first = gendir.builder("first.rs");
        let path = gendir.dir.join("autocxx-ffi-default-gen.rs");
        first.record(&path, b"first bindings").unwrap();
        std::fs::write(&path, "first bindings").unwrap();
        std::fs::remove_file(&path).unwrap();

        let second = gendir.builder("second.rs");
        second
            .record(&path, b"second bindings")
            .expect_err("a record was dropped because one of its files was");
    }

    /// A write which failed leaves nothing behind to refuse the same builder
    /// writing the file again.
    #[test]
    fn a_write_which_did_not_happen_is_forgotten() {
        let gendir = Gendir::new();
        let builder = gendir.builder("first.rs");
        let path = gendir.dir.join("autocxx-ffi-default-gen.rs");
        builder.record(&path, b"the attempt which failed").unwrap();
        builder.forget(&path);
        gendir
            .builder("second.rs")
            .record(&path, b"what was written instead")
            .expect("a write which never happened was held against the next one");
    }

    /// A later builder writing the same bytes does not take the file over,
    /// so its own failed write cannot forget the write which put the file
    /// there. `cxx.h`, which every builder writes identically, is the file
    /// this happens to.
    #[test]
    fn an_identical_write_does_not_take_the_file_over() {
        let gendir = Gendir::new();
        let first = gendir.builder("first.rs");
        let path = gendir.dir.join("cxx.h");
        first.record(&path, b"the same header").unwrap();

        let second = gendir.builder("second.rs");
        second.record(&path, b"the same header").unwrap();
        second.forget(&path);

        gendir
            .builder("third.rs")
            .record(&path, b"a header with the system includes stripped")
            .expect_err("the first builder's file was forgotten by a later writer of it");
    }

    /// Two builders can be given one input file, so which builder wrote a
    /// file is not which file it was generated for.
    #[test]
    fn one_builder_cannot_forget_anothers_write_of_the_same_input() {
        let gendir = Gendir::new();
        let path = gendir.dir.join("autocxx-ffi-default-gen.rs");
        gendir
            .builder("the-same.rs")
            .record(&path, b"first bindings")
            .unwrap();
        let second = gendir.builder("the-same.rs");
        second.forget(&path);
        second
            .record(&path, b"second bindings")
            .expect_err("one builder forgot another builder's file");
    }

    /// But another builder's record is not: `forget` is about the caller's
    /// own failed write.
    #[test]
    fn one_builder_cannot_forget_anothers_write() {
        let gendir = Gendir::new();
        let first = gendir.builder("first.rs");
        let path = gendir.dir.join("autocxx-ffi-default-gen.rs");
        first.record(&path, b"first bindings").unwrap();
        let second = gendir.builder("second.rs");
        second.forget(&path);
        second
            .record(&path, b"second bindings")
            .expect_err("one builder forgot another builder's file");
    }

    /// Two spellings of one generation directory are one generation
    /// directory, since a builder given a relative `custom_gendir` and one
    /// given the absolute path to it are writing into the same place.
    #[test]
    fn two_spellings_of_one_generation_directory() {
        let gendir = Gendir::new();
        let first = gendir.builder("first.rs");
        assert!(first.claim(&gendir.dir.join("gen0.cxx")));
        let spelt_differently = gendir.dir.join(".").join("sub").join("..");
        std::fs::create_dir(gendir.dir.join("sub")).unwrap();
        let second = OutputRegistry::open(&spelt_differently, Path::new("second.rs"));
        assert!(!second.claim(&gendir.dir.join("gen0.cxx")));
        assert!(!second.claim(&spelt_differently.join("gen0.cxx")));
    }

    /// Another process building into the same directory writes the marker
    /// too. That is not a directory remade under the path, and the files this
    /// record describes are still there, so the record is kept.
    #[test]
    fn a_marker_rewritten_by_someone_else_keeps_the_record() {
        let gendir = Gendir::new();
        let path = gendir.dir.join("autocxx-ffi-default-gen.rs");
        gendir
            .builder("first.rs")
            .record(&path, b"first bindings")
            .unwrap();

        std::fs::write(gendir.dir.join(super::RUN_MARKER), "someone else's").unwrap();

        gendir
            .builder("second.rs")
            .record(&path, b"second bindings")
            .expect_err("a record was dropped because another build marked the directory");
    }

    /// Records are per directory: two builders which do not share one have
    /// nothing to say to each other, which is what keeps a test harness
    /// running a builder per temporary directory unaffected.
    #[test]
    fn two_directories_do_not_see_each_other() {
        let one = Gendir::new();
        let other = Gendir::new();
        one.builder("first.rs")
            .record(&one.dir.join("autocxx-ffi-default-gen.rs"), b"first")
            .unwrap();
        other
            .builder("second.rs")
            .record(&other.dir.join("autocxx-ffi-default-gen.rs"), b"second")
            .unwrap();
        assert!(other.builder("third.rs").claim(&other.dir.join("gen0.cxx")));
    }

    /// Two names one filesystem cannot tell apart are one file, and the
    /// second block's bindings would replace the first's exactly as a
    /// repeated name would. Where the filesystem *can* tell them apart there
    /// are two files and nothing to report, so the test asks the filesystem
    /// which it is rather than assuming a platform.
    #[test]
    fn two_spellings_of_one_name() {
        let gendir = Gendir::new();
        let first = gendir.builder("first.rs");
        let second = gendir.builder("second.rs");
        let lower = gendir.dir.join("autocxx-ffi-gen.rs");
        let upper = gendir.dir.join("autocxx-Ffi-gen.rs");
        first.record(&lower, b"first bindings").unwrap();
        // `record` is asked before the write, so the file has to be there for
        // the filesystem to be able to answer about the second spelling.
        std::fs::write(&lower, "first bindings").unwrap();
        let outcome = second.record(&upper, b"second bindings");
        if upper.exists() {
            let collision = outcome.expect_err(
                "one file under two spellings was written twice without a word about it",
            );
            assert_eq!(collision.first, Path::new("first.rs"));
        } else {
            outcome.expect("two files on a filesystem which tells the names apart");
        }
    }

    /// The record is dropped for a directory nobody looks at again, but it is
    /// dropped by the next builder to open that directory and not before, so
    /// what accumulates is one entry per directory. Stated here because the
    /// integration suite opens thousands.
    #[test]
    fn one_entry_per_directory() {
        let gendir = Gendir::new();
        gendir.builder("first.rs");
        gendir.builder("second.rs");
        let entries = GENERATED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .filter(|key| key.starts_with(super::canonical(&gendir.dir)))
            .count();
        assert_eq!(entries, 1);
    }
}
