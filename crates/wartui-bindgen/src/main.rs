//! The bindings generator, on the same `uniffi` as `wartui-ffi` so the two never disagree. It is
//! a crate of its own because in library mode it reads only the built library and `uniffi.toml`,
//! so building it compiles none of the binding's stack:
//! `cargo run -p wartui-bindgen -- generate --library …`.

fn main() {
    uniffi::uniffi_bindgen_main();
}
