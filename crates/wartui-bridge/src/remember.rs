//! Which board answered the link protocol last time.
//!
//! **State, not configuration.** Nothing in here is edited by an operator and
//! nothing is lost by deleting it: the file is derived from what the machine
//! found, and throwing it away costs one slower start while the boards are swept
//! again. `--bridge` is the thing an operator chooses and keeps, and that belongs
//! on the command line where it can be read. Anything that wants to be remembered
//! *and* decided belongs there too, not here.
//!
//! **Every failure to read or write it is ignored.** A read-only home, no `$HOME`
//! at all, a directory where the file should be — each costs a sweep and nothing
//! else. A capture must never fail over a cached address, and the view owns the
//! terminal, so there is nowhere to complain to but the log.
//!
//! The file holds one line: an address spelled the way [`ports::mac_text`] spells
//! it and the way a `by-id` symlink carries it, so it can be grepped against
//! `wartui sniff` output and against `/dev/serial/by-id`. There is no format and
//! no version field — the parse is "does this line hold an address", and a line
//! that does not is the same as no file, which is also what a torn write leaves.
//! That is why one `write_all` is enough and a temporary file and a rename would
//! be ceremony over a value rebuilt in seconds.

use std::path::{Path, PathBuf};

use wartui_proto::link::Mac;

use crate::ports;

/// Where the address is kept between runs.
#[derive(Debug, Clone, Default)]
pub struct BridgeMemory {
    /// `None` when the host offers nowhere to keep it, which is not an error.
    path: Option<PathBuf>,
}

impl BridgeMemory {
    /// The file under this host's state directory.
    #[must_use]
    pub fn discover() -> Self {
        Self { path: state_dir().map(|dir| dir.join("bridge")) }
    }

    /// A file at a path of your choosing, which is how this is tested.
    #[must_use]
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: Some(path.into()) }
    }

    /// Remember nothing, and go on remembering nothing.
    #[must_use]
    pub const fn none() -> Self {
        Self { path: None }
    }

    /// The address last written, when it is still readable and still an address.
    #[must_use]
    pub fn recall(&self) -> Option<Mac> {
        let path = self.path.as_ref()?;
        let text = std::fs::read_to_string(path).ok()?;
        ports::parse_mac(text.trim())
    }

    /// Write `mac` over whatever was there.
    ///
    /// Writing the same address again is skipped rather than done: this is called
    /// once per connection, and a capture that reconnects across a loose cable
    /// would otherwise rewrite the file all evening for no change.
    pub fn remember(&self, mac: Mac) {
        let Some(path) = &self.path else { return };
        if self.recall() == Some(mac) {
            return;
        }
        if let Err(error) = write(path, mac) {
            tracing::debug!(path = %path.display(), %error, "could not remember the bridge");
        }
    }

    /// Forget whatever was there, so the next run sweeps instead.
    pub fn forget(&self) {
        let Some(path) = &self.path else { return };
        if let Err(error) = std::fs::remove_file(path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::debug!(path = %path.display(), %error, "could not forget the bridge");
        }
    }
}

fn write(path: &Path, mac: Mac) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{}\n", ports::mac_text(&mac)))
}

/// This host's directory for state a program rebuilds when it has to.
///
/// `XDG_STATE_HOME` is honoured only when it is absolute: the specification says a
/// relative value is invalid, and taking one literally would scatter a `wartui`
/// directory through whichever directory a capture was started from.
fn state_dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        let home = std::env::var_os("HOME")?;
        return Some(Path::new(&home).join("Library/Application Support/wartui"));
    }
    if let Some(state) = std::env::var_os("XDG_STATE_HOME") {
        let state = PathBuf::from(state);
        if state.is_absolute() {
            return Some(state.join("wartui"));
        }
    }
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".local/state/wartui"))
}
