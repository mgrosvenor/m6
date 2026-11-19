// m6-platform is the bottom of the m6 stack, so it cannot name itself as a
// build dependency. Including the helper keeps one implementation: the same
// file is `build_info::emit` in the library.
include!("src/build_info/emit.rs");

fn main() {
    emit();
}
