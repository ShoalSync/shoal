//! uniffi's bindings generator, pinned to the workspace's uniffi version. Generates the Kotlin
//! API from the built `shoal-ffi` library:
//!
//! ```sh
//! cargo build -p shoal-ffi
//! cargo run -p xtask --bin uniffi-bindgen -- generate \
//!     --library target/debug/libshoal_ffi.so --language kotlin --out-dir <dir>
//! ```

fn main() {
    uniffi::uniffi_bindgen_main()
}
