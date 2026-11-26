//! The bindings generator, built from this crate's `uniffi` so the two never disagree:
//! `cargo run -p wartui-ffi --features cli --bin uniffi-bindgen -- generate --library …`.

fn main() {
    uniffi::uniffi_bindgen_main();
}
