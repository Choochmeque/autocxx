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
        Ok(Self {
            file,
            outputs: Vec::new(),
            dependencies: Vec::new(),
            depfile_dir: depfile.parent().unwrap().to_path_buf(),
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
    // TODO: the .expect() below is reachable from user input: --outdir
    // accepts non-UTF-8 paths (panics at to_str). Should propagate a clean
    // error like main.rs does, but relativize's callers don't return Result
    // yet.
    fn relativize(&self, path: &Path) -> String {
        // A .d file is Make syntax, where backslash is the escape character,
        // so Windows separators would produce a file no consumer parses
        // back to these paths. Forward slashes are understood by make and
        // ninja on every platform, Windows included, so the depfile speaks
        // them regardless of what the OS calls its own.
        match pathdiff::diff_paths(path, &self.depfile_dir) {
            Some(relative) => relative
                .components()
                .map(|c| utf8(c.as_os_str()))
                .collect::<Vec<_>>()
                .join("/"),
            // There is not always a relative path to give: a Windows
            // dependency on another drive root, or a dependency and a depfile
            // of which only one is relative to the working directory - which
            // the .rs inputs can be while the preprocessor's header paths are
            // absolute. Make and ninja both take an absolute dependency, so
            // the path stands as it is rather than the depfile being
            // abandoned. Its root, and on Windows its drive prefix, is not a
            // component the join above could put back together.
            None => utf8(path.as_os_str()).replace('\\', "/"),
        }
    }
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

    /// A dependency with no relative path to the depfile - here one relative
    /// to the working directory where the depfile is not - is written out
    /// absolute rather than bringing the run down. `--depfile` is normally
    /// given absolute while an input `.rs` is named relatively.
    #[test]
    fn test_dependency_with_no_relative_path() {
        let tmp_dir = tempdir().unwrap();
        let f = tmp_dir.path().join("depfile.d");
        let mut df = Depfile::new(&f).unwrap();
        df.add_output(&tmp_dir.path().join("a/b"));
        df.add_dependency(Path::new("src/main.rs"));
        df.write().unwrap();

        let mut f = File::open(&f).unwrap();
        let mut contents = String::new();
        f.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "a/b: src/main.rs\n\n");
    }
}
