//! Uploading from the fleet view: `u`.
//!
//! The steps are `wartui upload`'s own — [`prepare`], [`send`], [`wait`], [`finish`] — so the
//! view sends what the command would and records it the same way. Each runs on its own
//! thread: building the body takes seconds on a long capture and sending it takes as long as
//! the uplink does, and the view has to keep drawing the capture meanwhile. The threads report
//! over a channel the view drains before each frame, and the view redraws on every snapshot,
//! so a report shows within a quarter of a second.
//!
//! Nothing here reaches the engine. An upload is between the capture file and the site; the
//! store's WAL gives the read its snapshot, as it does for `export` against a running capture.
//!
//! The key is read when `u` is pressed rather than at startup, so a key pasted in settings
//! works without a restart. Nothing is sent without the operator pressing `y` on what will go.
//!
//! Quitting needs no handling. A send still in flight is abandoned, and the site has queued
//! nothing; a job the site queued is already recorded in the capture.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::Instant;

use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Map, Value};
use wartui_core::export::ExportFilter;

use crate::tui::Settings;
use crate::upload::{
    Client, GIVE_UP, JobFailed, Prepare, Prepared, Progress, Waited, counts, finish, prepare, send,
    size, wait,
};

/// Where `u` uploads to: this run's capture, and the site.
#[derive(Debug, Clone, Default)]
pub struct UploadTarget {
    pub db: PathBuf,
    /// [`crate::upload::BASE`], or a local server under test.
    pub base: String,
}

/// The upload `u` started, and the footer line saying how it stands.
#[derive(Default)]
pub(crate) struct Upload {
    stage: Stage,
    /// Kept after the upload ends, until the next `u`.
    status: Option<String>,
}

#[derive(Default)]
enum Stage {
    #[default]
    Idle,
    Preparing(Receiver<Result<Prepare>>),
    Confirming(Box<Prepared>),
    /// Each line the thread sends replaces the status; it hangs up when done.
    Sending(Receiver<String>),
}

impl std::fmt::Debug for Upload {
    // By hand, so the key in `Prepared` is never printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let stage = match self.stage {
            Stage::Idle => "idle",
            Stage::Preparing(_) => "preparing",
            Stage::Confirming(_) => "confirming",
            Stage::Sending(_) => "sending",
        };
        f.debug_struct("Upload").field("stage", &stage).field("status", &self.status).finish()
    }
}

impl Upload {
    /// `u`: start preparing, or say why not. Returns the notice to show, if any.
    pub(crate) fn press(&mut self, settings: &Settings) -> Option<String> {
        if !matches!(self.stage, Stage::Idle) {
            return Some("an upload is already under way".to_owned());
        }
        self.status = None;
        if settings.saved.api_keys.wdgwars.trim().is_empty() {
            return Some("no WDGWars API key; paste one in settings (c)".to_owned());
        }
        let config = settings.saved.clone();
        let config_path = settings.config_path.clone();
        let db = settings.upload.db.clone();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let prepared =
                prepare(&config, config_path.as_deref(), &db, ExportFilter::default(), false);
            let _ = tx.send(prepared);
        });
        self.stage = Stage::Preparing(rx);
        self.status = Some("upload: preparing…".to_owned());
        None
    }

    /// Take what the threads have reported. Returns the notice to show, if any.
    pub(crate) fn poll(&mut self) -> Option<String> {
        match &self.stage {
            Stage::Preparing(rx) => match rx.try_recv() {
                Ok(prepared) => self.prepared(prepared),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => {
                    self.stage = Stage::Idle;
                    None
                }
            },
            Stage::Sending(rx) => {
                loop {
                    match rx.try_recv() {
                        Ok(line) => self.status = Some(line),
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            self.stage = Stage::Idle;
                            break;
                        }
                    }
                }
                None
            }
            Stage::Idle | Stage::Confirming(_) => None,
        }
    }

    fn prepared(&mut self, prepared: Result<Prepare>) -> Option<String> {
        self.stage = Stage::Idle;
        self.status = None;
        match prepared {
            Ok(Prepare::Ready(prepared)) => {
                self.stage = Stage::Confirming(Box::new(prepared));
                None
            }
            Ok(Prepare::NothingNew { previous, .. }) => {
                Some(format!("nothing new since job {}", previous.job_id))
            }
            Ok(Prepare::NoRows(_)) => Some("no positioned rows to upload yet".to_owned()),
            Err(error) => {
                self.status = Some(format!("upload failed: {error:#}"));
                None
            }
        }
    }

    /// Whether the confirm modal is open, and so takes every key.
    pub(crate) fn confirming(&self) -> bool {
        matches!(self.stage, Stage::Confirming(_))
    }

    /// A key while the confirm modal is open: `y` sends, anything else cancels. Returns the
    /// notice to show, if any.
    pub(crate) fn on_confirm_key(
        &mut self,
        key: KeyEvent,
        target: &UploadTarget,
    ) -> Option<String> {
        let Stage::Confirming(prepared) = std::mem::take(&mut self.stage) else { return None };
        if key.code != KeyCode::Char('y') || key.modifiers != KeyModifiers::NONE {
            return Some("not uploaded".to_owned());
        }
        self.status = Some(format!("upload: sending {}", size(prepared.body.len())));
        let (tx, rx) = channel();
        let (db, base) = (target.db.clone(), target.base.clone());
        std::thread::spawn(move || upload(&prepared, &db, &base, &tx));
        self.stage = Stage::Sending(rx);
        None
    }

    /// The confirm modal's lines, while it is open.
    pub(crate) fn confirm_lines(&self) -> Option<Vec<String>> {
        let Stage::Confirming(prepared) = &self.stage else { return None };
        let mut lines: Vec<String> = prepared.describe().lines().map(str::to_owned).collect();
        lines.push(String::new());
        lines.push("y upload · any other key cancels".to_owned());
        Some(lines)
    }

    /// The footer line, while there is one.
    pub(crate) fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }
}

/// Send `prepared`, follow the job, and record how it ended, saying each step on `tx`.
fn upload(prepared: &Prepared, db: &std::path::Path, base: &str, tx: &Sender<String>) {
    let say = |line: String| {
        let _ = tx.send(line);
    };
    let client = Client::new(base, prepared.key.clone());
    let sent = match send(&client, prepared, db) {
        Ok(sent) => sent,
        Err(error) => return say(format!("upload failed: {error:#}")),
    };
    let job = sent.job;
    say(format!("upload: job {job} queued"));
    let started = Instant::now();
    let waited = wait(
        job,
        GIVE_UP,
        || client.poll(job),
        std::thread::sleep,
        || started.elapsed(),
        |progress| match progress {
            Progress::State(name) => say(format!("upload: job {job} {name}")),
            Progress::PollError(message) => {
                say(format!("upload: job {job}: asking after it failed ({message}); trying again"))
            }
        },
    );
    let mut line = match &waited {
        Ok(Waited::Done(result)) => done_line(job, result),
        Ok(Waited::Unfollowed(reason)) => {
            format!("upload: job {job} queued; check wdgwars.pl for the result ({reason})")
        }
        Err(error) => {
            let message = error
                .downcast_ref::<JobFailed>()
                .map_or_else(|| format!("{error:#}"), |failed| failed.message.clone());
            format!("upload: job {job} failed: {message}")
        }
    };
    if let Err(error) = finish(&sent, &waited) {
        line.push_str(&format!("; {error:#}"));
    }
    say(line);
}

/// A finished job's counts on one line.
fn done_line(job: u64, result: &Map<String, Value>) -> String {
    let counts: Vec<String> =
        counts(result).into_iter().map(|(key, value)| format!("{key} {value}")).collect();
    if counts.is_empty() {
        return format!("upload: job {job} done; WDGWars sent no counts");
    }
    format!("upload: job {job} {}", counts.join("  "))
}
