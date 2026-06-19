//! Desktop binary entry point.
//!
//! The app lives in the `navgator` library (`src/lib.rs`). On desktop this thin binary calls
//! `desktop_main()`; on Android the entry is the library's `android_main` (built as a cdylib
//! and loaded by android-activity's NativeActivity), so there is no binary there.
fn main() {
    navgator::desktop_main();
}
