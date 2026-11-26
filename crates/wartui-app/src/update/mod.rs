//! Firmware updates over the link a front end already holds: the nodes', over the node bus
//! through the bridge ([`nodes`]), and the P4 bridge's own ([`bridge`]).
//!
//! **Nothing unchecked is sent.** An image is a [`image::NodeImage`] or an
//! [`image::BridgeImage`] only once it has passed every check its receiver's bootloader would
//! make, and more ([`image`]). A release's images are fetched and checked against the release's
//! `SHA256SUMS` before they are kept ([`release`]).
//!
//! **The front end keeps what is its own**: which link and which bridge, where an image comes
//! from (a file, a build, or a release), the operator's yes, and every word the operator reads.
//! Flashing a board over USB through espflash is a desktop's alone, and is not here.

pub mod bridge;
pub mod image;
pub mod nodes;
pub mod release;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
