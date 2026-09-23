//! uniffi's bindings generator, pinned to the workspace's uniffi version. Generates the Kotlin
//! API from the built `tidefell-ffi` library:
//!
//! ```sh
//! cargo build -p tidefell-ffi
//! cargo run -p xtask --bin uniffi-bindgen -- generate \
//!     --library target/debug/libtidefell_ffi.so --language kotlin --out-dir <dir>
//! ```

fn main() {
    uniffi::uniffi_bindgen_main()
}
