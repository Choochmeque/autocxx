This example puts a non-trivial C++ type - one whose constructor and
destructor C++ expects to run, and which it may not be safe to move - on the
Rust stack, using the `moveit!` macro, and calls methods on it there.
