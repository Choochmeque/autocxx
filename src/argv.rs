// Copyright 2026 Vladimir Pankratov
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Building the `argc`/`argv` pair that a C or C++ entry point expects.

use core::fmt;
use std::os::raw::{c_char, c_int};

/// Why a set of arguments could not be turned into a C `argv`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ArgvError {
    /// An argument contains a zero byte. C reads a `char*` up to the first
    /// one, so the argument could only be passed on with everything after it
    /// silently missing.
    InteriorNul {
        /// Which argument, counting from zero.
        index: usize,
        /// Where in that argument's bytes the zero byte is.
        position: usize,
    },
    /// More arguments than `argc` can count.
    TooManyArguments {
        /// How many were offered.
        count: usize,
    },
}

impl fmt::Display for ArgvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArgvError::InteriorNul { index, position } => write!(
                f,
                "argument {index} has a zero byte at offset {position}, so C would not see past it"
            ),
            ArgvError::TooManyArguments { count } => {
                write!(f, "{count} arguments is more than a C int can count")
            }
        }
    }
}

impl std::error::Error for ArgvError {}

/// An owned `argc`/`argv` pair, for calling a C or C++ function declared the
/// way `main` is: `f(int argc, char** argv)`.
///
/// The arguments are copied into storage this type owns, laid out as C expects
/// them: one NUL-terminated string each, addressed by an array of `char*` with
/// a NULL entry after the last one. That storage lives exactly as long as the
/// `ArgvHolder` does, which is why the holder is a value you keep rather than a
/// function you pass a closure to - plenty of C++ entry points (`QApplication`
/// is the well-known one) keep the pointer they were handed and read it again
/// later, so an `argv` freed at the end of the call would be a dangling one for
/// the rest of the program.
///
/// # autocxx will not generate the call for you
///
/// autocxx cannot bind a function whose parameter is `char**`. It turns down a
/// pointer whose pointee is itself a pointer - "Pointer pointed to another
/// pointer, which is not yet supported" - so such a function does not reach the
/// generated `ffi` module at all. Nothing here changes that. What this type
/// builds is the *argument*, for a call you declare yourself: through a
/// hand-written `cxx::bridge`, through a plain `extern "C"` block, or by adding
/// a C++ shim whose signature autocxx does understand (one taking a
/// `rust::Slice<const rust::Str>`, say, and assembling the `char**` on the C++
/// side).
///
/// # Example
///
/// ```rust,ignore
/// use autocxx::ArgvHolder;
/// use std::os::raw::{c_char, c_int};
///
/// // Declared by hand, because autocxx will not describe a `char**`.
/// extern "C" {
///     fn engine_init(argc: c_int, argv: *mut *mut c_char);
/// }
///
/// let mut args = ArgvHolder::new(["engine", "--headless"])?;
/// // SAFETY: `argc` and `argv` describe each other, and `args` owns the
/// // storage until it is dropped at the end of this scope.
/// unsafe { engine_init(args.argc(), args.argv()) };
/// # Ok::<(), autocxx::ArgvError>(())
/// ```
///
/// # What C may do with it
///
/// Everything a real `argv` permits: read the strings, write over their bytes,
/// and reorder the array - `getopt` permutes it in place. None of that
/// disturbs the holder, which frees from a private record of what it allocated
/// rather than from the array C was given.
///
/// What it may not do is keep the pointer past the holder's lifetime, or write
/// past the end of an argument's own bytes. Those are the caller's to promise,
/// at the `unsafe` call itself; building and freeing the storage is what this
/// type takes off them.
///
/// `ArgvHolder` is neither `Send` nor `Sync`, which it inherits from the raw
/// pointers it holds.
pub struct ArgvHolder {
    /// The array C is given: one `char*` per argument, then the NULL. C may
    /// reorder this, so it is not what `Drop` frees from.
    argv: Vec<*mut c_char>,
    /// Every allocation made, with its exact size, in the order it was made.
    /// Private, so neither reordering `argv` nor overwriting an argument's
    /// bytes can turn into a mismatched free - and recording the size is what
    /// makes overwriting the NUL harmless, where `strlen` at drop time would
    /// not survive it.
    owned: Vec<(*mut u8, usize)>,
}

impl ArgvHolder {
    /// Copies the arguments into storage shaped as a C `argv`.
    ///
    /// Anything that is a byte string will do: `&str` and `String` for the
    /// usual case, `&[u8]` or `Vec<u8>` for an argument that is not UTF-8,
    /// which on Unix is what `std::os::unix::ffi::OsStrExt::as_bytes` hands
    /// over. The bytes are passed on unchanged; no encoding is imposed.
    ///
    /// The program name is not supplied for you. C's convention is that
    /// `argv[0]` is the program's own name, so pass it first if the callee
    /// expects one.
    ///
    /// # Errors
    ///
    /// [`ArgvError::InteriorNul`] if an argument contains a zero byte, which C
    /// would read as the end of that argument, and
    /// [`ArgvError::TooManyArguments`] if there are more arguments than `argc`
    /// can count.
    pub fn new<S: AsRef<[u8]>>(args: impl IntoIterator<Item = S>) -> Result<Self, ArgvError> {
        // Built into the holder as we go, so that an early return frees
        // whatever has already been allocated.
        let mut holder = ArgvHolder {
            argv: Vec::new(),
            owned: Vec::new(),
        };
        for (index, arg) in args.into_iter().enumerate() {
            let bytes = arg.as_ref();
            if let Some(position) = bytes.iter().position(|byte| *byte == 0) {
                return Err(ArgvError::InteriorNul { index, position });
            }
            let mut storage = Vec::with_capacity(bytes.len() + 1);
            storage.extend_from_slice(bytes);
            storage.push(0);
            let storage = storage.into_boxed_slice();
            let len = storage.len();
            holder.owned.push((Box::into_raw(storage) as *mut u8, len));
        }
        if c_int::try_from(holder.owned.len()).is_err() {
            return Err(ArgvError::TooManyArguments {
                count: holder.owned.len(),
            });
        }
        holder.argv = holder
            .owned
            .iter()
            .map(|(ptr, _)| ptr.cast::<c_char>())
            .collect();
        holder.argv.push(core::ptr::null_mut());
        Ok(holder)
    }

    /// The argument count, for the `argc` parameter.
    ///
    /// This is `std::os::raw::c_int`, the plain integer a C declaration wants,
    /// not autocxx's [`crate::c_int`] newtype. A generated autocxx signature
    /// asking for the latter takes `autocxx::c_int(holder.argc())`.
    pub fn argc(&self) -> c_int {
        // The count was checked to fit in `new`, and nothing adds to it after.
        (self.argv.len() - 1) as c_int
    }

    /// The argument array, for the `argv` parameter: [`argc`](Self::argc)
    /// pointers followed by NULL.
    ///
    /// Taking `&mut self` because C is free to write through this - to the
    /// array, as `getopt` does, or to the strings it addresses.
    pub fn argv(&mut self) -> *mut *mut c_char {
        self.argv.as_mut_ptr()
    }
}

impl Drop for ArgvHolder {
    fn drop(&mut self) {
        for (ptr, len) in &self.owned {
            // SAFETY: every entry came from `Box::into_raw` on a boxed slice of
            // exactly `len` bytes in `new`, and `owned` is private, so nothing
            // has freed or replaced it since. The size is the one allocated,
            // not one recovered from the bytes, which C may have rewritten.
            unsafe {
                drop(Box::from_raw(core::ptr::slice_from_raw_parts_mut(
                    *ptr, *len,
                )));
            }
        }
    }
}

impl fmt::Debug for ArgvHolder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Read from the private record, not the array: C may have reordered
        // `argv`, repointed an entry at memory of its own, or overwritten a
        // terminator, and a `strlen`-style read through it could run off the
        // end. Each allocation's recorded size bounds the read instead, so the
        // arguments show in the order they were given, as C would read each
        // one now.
        let args = self.owned.iter().map(|(ptr, len)| {
            // SAFETY: our own live allocation of exactly `len` bytes; C may
            // have rewritten the bytes, but cannot have freed or resized it.
            let bytes = unsafe { core::slice::from_raw_parts(*ptr, *len) };
            let end = bytes
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(bytes.len());
            String::from_utf8_lossy(&bytes[..end]).into_owned()
        });
        f.debug_list().entries(args).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{ArgvError, ArgvHolder};
    use std::ffi::CStr;

    /// Reads the array back the way C would: `argc` pointers, then NULL.
    fn read_back(holder: &mut ArgvHolder) -> Vec<Vec<u8>> {
        let argc = holder.argc();
        let argv = holder.argv();
        // SAFETY: `argv` is the holder's own array, `argc` its length, and the
        // holder outlives this read.
        unsafe {
            assert!(
                (*argv.add(argc as usize)).is_null(),
                "argv is not NULL-terminated"
            );
            (0..argc)
                .map(|i| CStr::from_ptr(*argv.add(i as usize)).to_bytes().to_vec())
                .collect()
        }
    }

    #[test]
    fn builds_the_array_c_expects() {
        let mut holder = ArgvHolder::new(["prog", "--flag", "value"]).unwrap();
        assert_eq!(holder.argc(), 3);
        assert_eq!(
            read_back(&mut holder),
            vec![b"prog".to_vec(), b"--flag".to_vec(), b"value".to_vec()]
        );
    }

    #[test]
    fn no_arguments_is_just_the_terminator() {
        let mut holder = ArgvHolder::new(Vec::<String>::new()).unwrap();
        assert_eq!(holder.argc(), 0);
        // SAFETY: the array is one entry long and that entry is the NULL.
        unsafe { assert!((*holder.argv()).is_null()) };
    }

    #[test]
    fn an_empty_argument_is_an_empty_string_not_a_missing_one() {
        let mut holder = ArgvHolder::new(["prog", "", "after"]).unwrap();
        assert_eq!(holder.argc(), 3);
        assert_eq!(
            read_back(&mut holder),
            vec![b"prog".to_vec(), Vec::new(), b"after".to_vec()]
        );
    }

    #[test]
    fn non_ascii_arguments_pass_through_byte_for_byte() {
        let args = ["café", "日本語", "🦀"];
        let mut holder = ArgvHolder::new(args).unwrap();
        assert_eq!(
            read_back(&mut holder),
            args.iter()
                .map(|a| a.as_bytes().to_vec())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn non_utf8_bytes_pass_through_too() {
        // What `OsStrExt::as_bytes` can hand over on Unix.
        let arg: &[u8] = &[0xff, 0xfe, b'x'];
        let mut holder = ArgvHolder::new([arg]).unwrap();
        assert_eq!(read_back(&mut holder), vec![arg.to_vec()]);
    }

    #[test]
    fn an_interior_nul_is_refused_with_its_position() {
        let err = ArgvHolder::new(["prog", "a\0b"]).unwrap_err();
        assert_eq!(
            err,
            ArgvError::InteriorNul {
                index: 1,
                position: 1
            }
        );
        assert!(err.to_string().contains("argument 1"));
    }

    #[test]
    fn a_nul_at_the_end_of_an_argument_is_still_interior() {
        // C cannot tell this from a shorter argument, so it is refused rather
        // than quietly accepted as already terminated.
        assert_eq!(
            ArgvHolder::new(["prog\0"]).unwrap_err(),
            ArgvError::InteriorNul {
                index: 0,
                position: 4
            }
        );
    }

    #[test]
    fn surviving_what_c_does_to_argv() {
        let mut holder = ArgvHolder::new(["prog", "--flag", "key=value"]).unwrap();
        let argv = holder.argv();
        // SAFETY: the array is the holder's own, three entries and a NULL, and
        // the holder outlives these writes.
        unsafe {
            // Splitting an argument in place, as a parser might: the NUL moves
            // and `strlen` would now report 3 rather than 9.
            *(*argv.add(2)).add(3) = 0;
            // Permuting the array, as `getopt` does.
            argv.add(0).swap(argv.add(2));
        }
        // Freeing works from the private record, so neither of those matters.
        drop(holder);
    }

    #[test]
    fn debug_shows_the_arguments() {
        let holder = ArgvHolder::new(["prog", "--flag"]).unwrap();
        assert_eq!(format!("{holder:?}"), r#"["prog", "--flag"]"#);
    }

    #[test]
    fn owned_storage_is_independent_of_what_it_was_built_from() {
        let mut source = vec![String::from("prog"), String::from("--flag")];
        let mut holder = ArgvHolder::new(&source).unwrap();
        source.clear();
        assert_eq!(
            read_back(&mut holder),
            vec![b"prog".to_vec(), b"--flag".to_vec()]
        );
    }

    #[test]
    fn c_char_signedness_does_not_change_the_bytes() {
        // `c_char` is signed on some targets and unsigned on others; the array
        // is bytes either way.
        let mut holder = ArgvHolder::new([[0x80u8, 0x7fu8].as_slice()]).unwrap();
        let argv = holder.argv();
        // SAFETY: the first entry is our own two-byte-plus-NUL allocation.
        unsafe {
            assert_eq!(*(*argv) as u8, 0x80);
            assert_eq!(*(*argv).add(1) as u8, 0x7f);
            assert_eq!(*(*argv).add(2) as u8, 0);
        }
    }
}
