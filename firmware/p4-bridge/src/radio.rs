//! The C6 image this P4 carries, built from `firmware/p4-radio` by `build.rs`.
//!
//! At boot the P4 compares the digest in the C6's hello with [`RADIO_DIGEST`], and sends
//! [`RADIO_IMAGE`] over the SPI link when they differ (`wartui_proto::split`,
//! `src/link.rs`). Installing one P4 image therefore installs the pair, and the two never
//! run builds from different trees for longer than an update takes.

include!(concat!(env!("OUT_DIR"), "/radio.rs"));
