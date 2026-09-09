// Copyright 2022 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::{
    ffi::OsStr,
    fs::File,
    io::Write,
    path::{Path, PathBuf},
};

/// Type which knows how to write a .d file. All outputs depend on all
/// dependencies.
pub(crate) struct Depfile {
    file: File,
    outputs: Vec<String>,
    dependencies: Vec<String>,
    depfile_dir: PathBuf,
}

impl Depfile {
    pub(crate) fn new(depfile: &Path) -> std::io::Result<Self> {
        let file = File::create(depfile)?;
        let dir = depfile.parent().unwrap_or(Path::new(""));
        Ok(Self {
            file,
            outputs: Vec::new(),
            dependencies: Vec::new(),
            depfile_dir: absolutize(if dir.as_os_str().is_empty() {
                Path::new(".")
            } else {
                dir
            }),
        })
    }

    pub(crate) fn add_dependency(&mut self, dependency: &Path) {
        self.dependencies.push(self.relativize(dependency))
    }

    pub(crate) fn add_output(&mut self, output: &Path) {
        self.outputs.push(self.relativize(output))
    }

    pub(crate) fn write(&mut self) -> std::io::Result<()> {
        let dependency_list = self.dependencies.join(" \\\n  ");
        for output in &self.outputs {
            self.file
                .write_all(format!("{output}: {dependency_list}\n\n").as_bytes())?
        }
        Ok(())
    }

    /// Return a string giving a relative path from the depfile.
    ///
    /// Every path is made absolute first. The paths reaching here come from
    /// two places with two conventions - the preprocessor reports headers
    /// absolute, while an input `.rs` is named on the command line however
    /// the caller pleased - and a depfile whose entries are relative to two
    /// different directories names files which are not the ones read.
    // TODO: the .expect() in utf8 is reachable from user input: --outdir
    // accepts non-UTF-8 paths. Should propagate a clean error like main.rs
    // does, but relativize's callers don't return Result yet.
    fn relativize(&self, path: &Path) -> String {
        let path = absolutize(path);
        // Only within one root does diff_paths have a relative path to give.
        // Asked across two Windows drives it does not say so: it returns
        // components which rebuild an absolute path, and the join below would
        // splice that into the parent-directory hops it also returned.
        let relative = (path.components().next() == self.depfile_dir.components().next())
            .then(|| pathdiff::diff_paths(&path, &self.depfile_dir))
            .flatten();
        match relative {
            // A .d file is Make syntax, where backslash is the escape
            // character, so Windows separators would produce a file no
            // consumer parses back to these paths. Forward slashes are
            // understood by make and ninja on every platform, Windows
            // included, so the depfile speaks them regardless of what the OS
            // calls its own.
            Some(relative) => relative
                .components()
                .map(|c| utf8(c.as_os_str()))
                .collect::<Vec<_>>()
                .join("/"),
            // Make and ninja both take an absolute dependency, so a path with
            // no relative form is written whole rather than the depfile being
            // abandoned. Its root, and on Windows its drive prefix, is not
            // something the component join could put back together.
            None => utf8(path.as_os_str()).replace('\\', "/"),
        }
    }
}

/// Lexically: a depfile records the paths a build read, not what they point at
/// today, and resolving symlinks would name files the build never opened.
fn absolutize(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

fn utf8(path: &OsStr) -> String {
    path.to_str()
        .expect("Unable to represent the file path in a UTF8 encoding")
        .to_string()
}

#[cfg(test)]
mod tests {
    use std::{fs::File, io::Read, path::Path};

    use tempfile::tempdir;

    use super::Depfile;

    #[test]
    fn test_simple_depfile() {
        let tmp_dir = tempdir().unwrap();
        let f = tmp_dir.path().join("depfile.d");
        let mut df = Depfile::new(&f).unwrap();
        df.add_output(&tmp_dir.path().join("a/b"));
        df.add_dependency(&tmp_dir.path().join("c/d"));
        df.add_dependency(&tmp_dir.path().join("e/f"));
        df.write().unwrap();

        let mut f = File::open(&f).unwrap();
        let mut contents = String::new();
        f.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "a/b: c/d \\\n  e/f\n\n");
    }

    #[test]
    fn test_multiple_outputs() {
        let tmp_dir = tempdir().unwrap();
        let f = tmp_dir.path().join("depfile.d");
        let mut df = Depfile::new(&f).unwrap();
        df.add_output(&tmp_dir.path().join("a/b"));
        df.add_output(&tmp_dir.path().join("z"));
        df.add_dependency(&tmp_dir.path().join("c/d"));
        df.add_dependency(&tmp_dir.path().join("e/f"));
        df.write().unwrap();

        let mut f = File::open(&f).unwrap();
        let mut contents = String::new();
        f.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "a/b: c/d \\\n  e/f\n\nz: c/d \\\n  e/f\n\n");
    }

    /// The two conventions the callers use - the preprocessor's absolute
    /// header paths, and an input `.rs` named on the command line relative to
    /// the working directory - describe the same file the same way, and
    /// neither brings the run down. `--depfile` is normally given absolute
    /// while an input is named relatively, so the two really do meet here.
    #[test]
    fn test_dependency_bases_agree() {
        let tmp_dir = tempdir().unwrap();
        let f = tmp_dir.path().join("depfile.d");
        let mut df = Depfile::new(&f).unwrap();
        df.add_output(&tmp_dir.path().join("a/b"));
        df.add_dependency(Path::new("src/main.rs"));
        df.add_dependency(&std::env::current_dir().unwrap().join("src/main.rs"));
        df.write().unwrap();

        let mut f = File::open(&f).unwrap();
        let mut contents = String::new();
        f.read_to_string(&mut contents).unwrap();
        let (named_relatively, named_absolutely) = contents
            .trim_end()
            .split_once(" \\\n  ")
            .expect("both dependencies should be listed");
        assert_eq!(
            named_relatively.split_once(": ").unwrap().1,
            named_absolutely
        );
    }
}
