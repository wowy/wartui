//! Uploading from the fleet view: `u`.
//!
//! The steps are `wartui upload`'s own: [`prepare`], [`send`], [`wait`], [`finish`]. The
//! view sends what the command would and records it the same way.
//!
//! Each step runs on its own thread, so the view keeps drawing. Building the body takes
//! seconds on a long capture, and sending takes as long as the uplink. The threads report
//! over a channel the view drains before each frame, so a report shows within a quarter of a
//! second.
//!
//! Nothing here reaches the engine. An upload is between the capture file and the site. The
//! store's WAL gives the read its snapshot, as it does for `export` on a running capture.
//!
//! The key is read when `u` is pressed, not at startup, so a key pasted in settings works
//! without a restart. Nothing is sent until the operator presses `y` on what will go.
//!
//! Quitting needs no handling. A send still in flight is abandoned with nothing queued at
//! the site, and a job the site queued is already recorded in the capture.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::Instant;

use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Map, Value};
use wartui_core::export::ExportFilter;

use ratatui::Frame;
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph};

use super::{Settings, centered_rect};
use crate::upload::{
    Client, GIVE_UP, JobFailed, Prepare, Prepared, Progress, Simulated, Waited, counts, finish,
    prepare, send, size, wait,
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
    /// Each line the thread sends replaces the status. It hangs up when done.
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
            Err(error) if error.is::<Simulated>() => Some(error.to_string()),
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

/// The upload's confirm, centred like settings and sized to its text.
pub(super) fn draw_confirm_modal(frame: &mut Frame<'_>, lines: &[String]) {
    let widest = lines.iter().map(|line| line.chars().count()).max().unwrap_or(0);
    let width = u16::try_from(widest + 4).unwrap_or(u16::MAX);
    let height = u16::try_from(lines.len() + 2).unwrap_or(u16::MAX);
    let area = centered_rect(width, height, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::bordered().title(" upload to WDGWars ");
    let inner = block.inner(area).inner(ratatui::layout::Margin::new(1, 0));
    frame.render_widget(block, area);
    let lines: Vec<Line<'_>> = lines.iter().map(|line| Line::from(line.as_str())).collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tokio::sync::mpsc;
    use wartui_core::engine::Snapshot;

    use crate::config;
    use crate::tui::ui::Ui;

    use super::*;
    use crate::tui::draw;
    use crate::tui::fixtures::*;

    /// A view whose `u` uploads `db`, with a key, to a local server at `base`.
    fn uploading(db: PathBuf, base: String) -> Ui {
        let mut saved = config::Config::default();
        saved.api_keys.wdgwars = "secret".to_owned();
        let upload = UploadTarget { db, base };
        Ui { settings: Settings { saved, upload, ..Settings::default() }, ..Ui::default() }
    }

    /// A capture holding two positioned networks.
    fn two_rows(dir: &tempfile::TempDir) -> PathBuf {
        use crate::testing::{capture, sighting};
        capture(
            dir,
            vec![
                sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0)),
                sighting([0x10, 0, 0, 0, 0, 2], EPOCH_MS + 1_000, Some(37.1)),
            ],
        )
    }

    /// Drain the upload threads, as each frame does, until `done` holds. Fails after
    /// `secs` seconds.
    fn poll_until(ui: &mut Ui, snapshot: &Snapshot, secs: u64, done: impl Fn(&Ui) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while !done(ui) {
            assert!(Instant::now() < deadline, "timed out: {:?}", ui.upload);
            std::thread::sleep(Duration::from_millis(20));
            ui.poll_upload(snapshot);
        }
    }

    fn press_u(ui: &mut Ui, snapshot: &Snapshot) {
        let (tx, _rx) = mpsc::channel(4);
        ui.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE), snapshot, &tx);
    }

    #[test]
    fn upload_key_shows_notice_when_no_api_key() {
        let snapshot = busy();
        let mut ui = Ui::default();
        press_u(&mut ui, &snapshot);
        assert_eq!(
            ui.notice(snapshot.now_ms),
            Some("no WDGWars API key; paste one in settings (c)")
        );
        assert!(!ui.upload.confirming());
    }

    #[test]
    fn upload_key_opens_confirm_with_row_count_when_capture_has_rows() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = busy();
        let mut ui = uploading(two_rows(&dir), "http://127.0.0.1:9".to_owned());
        press_u(&mut ui, &snapshot);
        assert_eq!(ui.upload.status(), Some("upload: preparing…"));
        poll_until(&mut ui, &snapshot, 10, |ui| ui.upload.confirming());

        let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut ui)).expect("drawing");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("2 rows to upload"), "{rendered}");
        assert!(rendered.contains("compressed"), "{rendered}");
        assert!(rendered.contains("y upload · any other key cancels"), "{rendered}");
    }

    #[test]
    fn upload_confirm_sends_nothing_when_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let snapshot = busy();
        let mut ui = uploading(two_rows(&dir), base);
        press_u(&mut ui, &snapshot);
        poll_until(&mut ui, &snapshot, 10, |ui| ui.upload.confirming());

        ui.on_confirm_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), &snapshot);
        assert!(!ui.upload.confirming());
        assert_eq!(ui.notice(snapshot.now_ms), Some("not uploaded"));
        std::thread::sleep(Duration::from_millis(200));
        listener.set_nonblocking(true).unwrap();
        assert!(listener.accept().is_err(), "the upload was sent anyway");
    }

    #[test]
    fn upload_footer_shows_job_result_when_import_done() {
        let dir = tempfile::tempdir().unwrap();
        let (base, server) = crate::testing::serve_sequence(&[
            ("202 Accepted", r#"{"ok":true,"job_id":42}"#),
            (
                "200 OK",
                r#"{"ok":true,"job_id":42,"status":"done","result":{"imported":1200,"captured":17}}"#,
            ),
        ]);
        let snapshot = busy();
        let mut ui = uploading(two_rows(&dir), base);
        press_u(&mut ui, &snapshot);
        poll_until(&mut ui, &snapshot, 10, |ui| ui.upload.confirming());
        ui.on_confirm_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE), &snapshot);
        assert!(ui.upload.status().is_some_and(|s| s.starts_with("upload: sending ")));
        // Through `wait`'s first poll, two seconds in.
        poll_until(&mut ui, &snapshot, 20, |ui| {
            ui.upload.status().is_some_and(|s| s.contains("imported"))
        });

        let mut terminal = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut ui)).expect("drawing");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("upload: job 42 imported 1,200  captured 17"), "{rendered}");
        server.join().unwrap();
    }

    #[test]
    fn upload_footer_shows_error_when_send_fails() {
        let dir = tempfile::tempdir().unwrap();
        let (base, server) =
            crate::testing::serve_once("401 Unauthorized", r#"{"ok":false,"error":"bad key"}"#);
        let snapshot = busy();
        let mut ui = uploading(two_rows(&dir), base);
        press_u(&mut ui, &snapshot);
        poll_until(&mut ui, &snapshot, 10, |ui| ui.upload.confirming());
        ui.on_confirm_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE), &snapshot);
        poll_until(&mut ui, &snapshot, 10, |ui| {
            ui.upload.status().is_some_and(|s| s.starts_with("upload failed:"))
        });
        assert_eq!(
            ui.upload.status(),
            Some("upload failed: WDGWars rejected the API key: bad key")
        );
        server.join().unwrap();
    }
}
