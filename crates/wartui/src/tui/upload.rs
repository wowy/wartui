//! Uploading from the fleet view: `u`.
//!
//! The steps are `wartui upload`'s own: [`prepare`], [`send`], [`wdgwars::wait`], [`finish`].
//! The view sends what the command would, to every site with credentials, and records it the
//! same way.
//!
//! Each step runs on its own thread, so the view keeps drawing. One thread prepares every site,
//! since building a body takes seconds on a long capture. After `y`, each site sends on a
//! thread of its own, because sending takes as long as the uplink and the two end separately.
//! The threads report over a channel the view drains before each frame, so a report shows
//! within a quarter of a second, and each site keeps a footer line of its own.
//!
//! One confirm covers every site. With one site involved it is that site's full summary; with
//! two, each gets a compact section, because two full summaries do not fit a short terminal.
//!
//! Nothing here reaches the engine. An upload is between the capture file and the sites. The
//! store's WAL gives the read its snapshot, as it does for `export` on a running capture.
//!
//! The credentials are read when `u` is pressed, not at startup, so a key pasted in settings
//! works without a restart. Nothing is sent until the operator presses `y` on what will go.
//!
//! Quitting needs no handling. A send still in flight is abandoned with nothing queued at
//! the site, and an upload the site queued is already recorded in the capture.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::Instant;

use anyhow::Result;
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph};
use serde_json::{Map, Value};
use wartui_core::store::UploadSite;

use super::{Settings, centered_rect};
use crate::config;
use crate::upload::{
    Job, Options, Prepare, Prepared, Sent, Simulated, Site, finish, involved, prepare, send, size,
    wdgwars, wigle,
};

/// Where `u` uploads to: this run's capture, and the sites.
#[derive(Debug, Clone, Default)]
pub struct UploadTarget {
    pub db: PathBuf,
    /// [`wdgwars::BASE`], or a local server under test.
    pub wdgwars_base: String,
    /// [`wigle::BASE`], or a local server under test.
    pub wigle_base: String,
}

/// The width a site's label is padded to in the two-site confirm.
const LABEL_WIDTH: usize = 9;

/// The upload `u` started, and the footer lines saying how it stands.
#[derive(Default)]
pub(super) struct Upload {
    stage: Stage,
    /// The line while preparing, or why the whole upload failed. Kept until the next `u`.
    general: Option<String>,
    /// Each site's line, kept until the next `u`.
    wdgwars: Option<String>,
    wigle: Option<String>,
}

#[derive(Default)]
enum Stage {
    #[default]
    Idle,
    Preparing(Receiver<Result<Vec<(UploadSite, Prepare)>>>),
    /// Every site involved, at least one of them ready.
    Confirming(Vec<(UploadSite, Prepare)>),
    /// Each line a site's thread sends replaces that site's line. The stage ends when every
    /// thread has hung up.
    Sending(Receiver<(UploadSite, String)>),
}

impl std::fmt::Debug for Upload {
    // By hand, so the credentials in `Prepared` are never printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let stage = match self.stage {
            Stage::Idle => "idle",
            Stage::Preparing(_) => "preparing",
            Stage::Confirming(_) => "confirming",
            Stage::Sending(_) => "sending",
        };
        f.debug_struct("Upload")
            .field("stage", &stage)
            .field("status", &self.status_lines())
            .finish()
    }
}

impl Upload {
    /// `u`: start preparing, or say why not. Returns the notice to show, if any.
    ///
    /// `excluded` is the engine's list in force, not `settings.saved`'s, which is stale
    /// when saving `wartui.toml` failed.
    pub(super) fn press(&mut self, settings: &Settings, excluded: &[String]) -> Option<String> {
        if !matches!(self.stage, Stage::Idle) {
            return Some("an upload is already under way".to_owned());
        }
        self.general = None;
        self.wdgwars = None;
        self.wigle = None;
        if involved(&settings.saved).is_empty() {
            return Some("no upload credentials; paste a key in settings (c)".to_owned());
        }
        let config = settings.saved.clone();
        let config_path = settings.config_path.clone();
        let db = settings.upload.db.clone();
        let options = Options {
            excluded_ssids: config::excluded_ssid_bytes(excluded).collect(),
            ..Options::default()
        };
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let prepared = prepare(&config, config_path.as_deref(), &db, &options);
            let _ = tx.send(prepared);
        });
        self.stage = Stage::Preparing(rx);
        self.general = Some("upload: preparing…".to_owned());
        None
    }

    /// Take what the threads have reported. Returns the notice to show, if any.
    pub(super) fn poll(&mut self) -> Option<String> {
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
                let mut lines = Vec::new();
                let mut done = false;
                loop {
                    match rx.try_recv() {
                        Ok(line) => lines.push(line),
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            done = true;
                            break;
                        }
                    }
                }
                for (site, line) in lines {
                    *self.line_mut(site) = Some(line);
                }
                if done {
                    self.stage = Stage::Idle;
                }
                None
            }
            Stage::Idle | Stage::Confirming(_) => None,
        }
    }

    fn prepared(&mut self, prepared: Result<Vec<(UploadSite, Prepare)>>) -> Option<String> {
        self.stage = Stage::Idle;
        self.general = None;
        let outcomes = match prepared {
            Ok(outcomes) => outcomes,
            Err(error) if error.is::<Simulated>() => return Some(error.to_string()),
            Err(error) => {
                self.general = Some(format!("upload failed: {error:#}"));
                return None;
            }
        };
        for (site, outcome) in &outcomes {
            if let Prepare::Refused(error) = outcome {
                *self.line_mut(*site) = Some(format!("upload: {} failed: {error:#}", site.label()));
            }
        }
        if outcomes.iter().any(|(_, outcome)| matches!(outcome, Prepare::Ready(_))) {
            self.stage = Stage::Confirming(outcomes);
            return None;
        }
        nothing_to_send(&outcomes)
    }

    /// The footer line of `site`.
    fn line_mut(&mut self, site: UploadSite) -> &mut Option<String> {
        match site {
            UploadSite::Wdgwars => &mut self.wdgwars,
            UploadSite::Wigle => &mut self.wigle,
        }
    }

    /// Whether the confirm modal is open, and so takes every key.
    pub(super) fn confirming(&self) -> bool {
        matches!(self.stage, Stage::Confirming(_))
    }

    /// A key while the confirm modal is open: `y` sends to every ready site, anything else
    /// cancels. Returns the notice to show, if any.
    pub(super) fn on_confirm_key(
        &mut self,
        key: KeyEvent,
        target: &UploadTarget,
    ) -> Option<String> {
        let Stage::Confirming(outcomes) = std::mem::take(&mut self.stage) else { return None };
        if key.code != KeyCode::Char('y') || key.modifiers != KeyModifiers::NONE {
            return Some("not uploaded".to_owned());
        }
        let (tx, rx) = channel();
        for (site, outcome) in outcomes {
            let Prepare::Ready(prepared) = outcome else { continue };
            *self.line_mut(site) =
                Some(format!("upload: {} sending {}", site.label(), size(prepared.body.len())));
            let (tx, target) = (tx.clone(), target.clone());
            std::thread::spawn(move || {
                upload(&prepared, &target, |line| {
                    let _ = tx.send((site, line));
                });
            });
        }
        self.stage = Stage::Sending(rx);
        None
    }

    /// The confirm modal's title, while it is open.
    pub(super) fn confirm_title(&self) -> Option<String> {
        let Stage::Confirming(outcomes) = &self.stage else { return None };
        Some(match outcomes.as_slice() {
            [(site, _)] => format!(" upload to {} ", site.label()),
            _ => " upload ".to_owned(),
        })
    }

    /// The confirm modal's lines, while it is open.
    pub(super) fn confirm_lines(&self) -> Option<Vec<String>> {
        let Stage::Confirming(outcomes) = &self.stage else { return None };
        let mut lines = Vec::new();
        if let [(_, Prepare::Ready(prepared))] = outcomes.as_slice() {
            lines.extend(prepared.describe().lines().map(str::to_owned));
        } else {
            for (at, (site, outcome)) in outcomes.iter().enumerate() {
                if at > 0 {
                    lines.push(String::new());
                }
                section(&mut lines, *site, outcome);
            }
        }
        lines.push(String::new());
        let ready = outcomes.iter().filter(|(_, o)| matches!(o, Prepare::Ready(_))).count();
        lines.push(if ready > 1 {
            "y upload both · any other key cancels".to_owned()
        } else {
            "y upload · any other key cancels".to_owned()
        });
        Some(lines)
    }

    /// The footer lines: the one while preparing or for a failure of the whole upload, then
    /// each site's, WDGWars first.
    pub(super) fn status_lines(&self) -> Vec<&str> {
        [&self.general, &self.wdgwars, &self.wigle]
            .into_iter()
            .filter_map(Option::as_deref)
            .collect()
    }
}

/// `site`'s section of the two-site confirm: its label, then its compact summary or why it has
/// nothing to send.
fn section(lines: &mut Vec<String>, site: UploadSite, outcome: &Prepare) {
    let label = format!("{:<LABEL_WIDTH$}", site.label());
    match outcome {
        Prepare::Ready(prepared) => {
            let brief = prepared.brief();
            let mut brief = brief.lines();
            lines.push(format!("{label}{}", brief.next().unwrap_or_default()));
            lines.extend(brief.map(str::to_owned));
        }
        Prepare::NothingNew { previous, .. } => {
            lines.push(format!("{label}nothing new since {} {}", site.job(), previous.job));
        }
        Prepare::NoRows(_) => lines.push(format!("{label}no positioned rows to upload yet")),
        Prepare::Refused(_) => lines.push(format!("{label}cannot upload; see the footer")),
    }
}

/// The notice when no site in `outcomes` is ready. A refused site's reason is in the footer.
fn nothing_to_send(outcomes: &[(UploadSite, Prepare)]) -> Option<String> {
    if let [(site, outcome)] = outcomes {
        return match outcome {
            Prepare::NothingNew { previous, .. } => {
                Some(format!("nothing new since {} {}", site.job(), previous.job))
            }
            Prepare::NoRows(_) => Some("no positioned rows to upload yet".to_owned()),
            Prepare::Ready(_) | Prepare::Refused(_) => None,
        };
    }
    let mut rest =
        outcomes.iter().filter(|(_, outcome)| !matches!(outcome, Prepare::Refused(_))).peekable();
    rest.peek()?;
    if rest.all(|(_, outcome)| matches!(outcome, Prepare::NoRows(_))) {
        Some("no positioned rows to upload yet".to_owned())
    } else {
        Some("nothing new since the last uploads".to_owned())
    }
}

/// Send `prepared` to its site, then follow a WDGWars job and record how it ended, saying each
/// step through `say`.
fn upload(prepared: &Prepared, target: &UploadTarget, say: impl Fn(String)) {
    let label = prepared.site().label();
    match send(prepared, &target.db, &target.wdgwars_base, &target.wigle_base) {
        Err(error) => say(format!("upload: {label} failed: {error:#}")),
        Ok(Sent::Wigle(queued)) => say(queued_line(&queued)),
        Ok(Sent::Wdgwars { client, job }) => follow(&client, &job, say),
    }
}

/// WiGLE's queued transaction on one line, with its warning when it sent one.
fn queued_line(queued: &wigle::Queued) -> String {
    let mut line = format!("upload: WiGLE queued as {}; see wigle.net/uploads", queued.trans_id);
    if let Some(warning) = &queued.warning {
        line.push_str(&format!("; WiGLE warns: {warning}"));
    }
    line
}

/// Follow WDGWars' `job` and record how it ended, saying each step through `say`.
fn follow(client: &wdgwars::Client, job: &Job, say: impl Fn(String)) {
    let id = job.id;
    say(format!("upload: WDGWars job {id} queued"));
    let started = Instant::now();
    let waited = wdgwars::wait(
        id,
        wdgwars::GIVE_UP,
        || client.poll(id),
        std::thread::sleep,
        || started.elapsed(),
        |progress| match progress {
            wdgwars::Progress::State(name) => say(format!("upload: WDGWars job {id} {name}")),
            wdgwars::Progress::PollError(message) => say(format!(
                "upload: WDGWars job {id}: asking after it failed ({message}); trying again"
            )),
        },
    );
    let mut line = match &waited {
        Ok(wdgwars::Waited::Done(result)) => done_line(id, result),
        Ok(wdgwars::Waited::Unfollowed(reason)) => {
            format!("upload: WDGWars job {id} queued; check wdgwars.pl for the result ({reason})")
        }
        Err(error) => {
            let message = error
                .downcast_ref::<wdgwars::JobFailed>()
                .map_or_else(|| format!("{error:#}"), |failed| failed.message.clone());
            format!("upload: WDGWars job {id} failed: {message}")
        }
    };
    if let Err(error) = finish(job, &waited) {
        line.push_str(&format!("; {error:#}"));
    }
    say(line);
}

/// A finished job's counts on one line.
fn done_line(job: u64, result: &Map<String, Value>) -> String {
    let counts: Vec<String> =
        wdgwars::counts(result).into_iter().map(|(key, value)| format!("{key} {value}")).collect();
    if counts.is_empty() {
        return format!("upload: WDGWars job {job} done; WDGWars sent no counts");
    }
    format!("upload: WDGWars job {job} {}", counts.join("  "))
}

/// A confirm titled `title`, centered like settings and sized to its text: the upload's, and
/// the power-cycle's.
pub(super) fn draw_confirm_modal(frame: &mut Frame<'_>, title: &str, lines: &[String]) {
    let widest = lines.iter().map(|line| line.chars().count()).max().unwrap_or(0);
    let width = u16::try_from(widest + 4).unwrap_or(u16::MAX);
    let height = u16::try_from(lines.len() + 2).unwrap_or(u16::MAX);
    let area = centered_rect(width, height, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::bordered().title(title);
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

    use super::*;
    use crate::config;
    use crate::tui::draw;
    use crate::tui::fixtures::*;
    use crate::tui::ui::Ui;

    /// A local server for a site the test should never reach.
    const NOWHERE: &str = "http://127.0.0.1:9";

    /// A view whose `u` uploads `db` with `saved`'s credentials to local servers.
    fn view(db: PathBuf, saved: config::Config, wdgwars_base: &str, wigle_base: &str) -> Ui {
        let upload = UploadTarget {
            db,
            wdgwars_base: wdgwars_base.to_owned(),
            wigle_base: wigle_base.to_owned(),
        };
        Ui::new(Settings { saved, upload, ..Settings::default() })
    }

    /// A view whose `u` uploads `db`, with a WDGWars key alone, to a local server at `base`.
    fn uploading(db: PathBuf, base: String) -> Ui {
        let mut saved = config::Config::default();
        saved.api_keys.wdgwars = "secret".to_owned();
        view(db, saved, &base, NOWHERE)
    }

    /// Credentials for both sites, the WiGLE token left out unless `token`.
    fn both(token: bool) -> config::Config {
        let mut saved = config::Config::default();
        saved.api_keys.wdgwars = "secret".to_owned();
        saved.api_keys.wigle_name = "AIDexamplename".to_owned();
        if token {
            saved.api_keys.wigle_token = "exampletoken789".to_owned();
        }
        saved
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
            assert!(Instant::now() < deadline, "timed out: {:?}", ui.upload());
            std::thread::sleep(Duration::from_millis(20));
            ui.poll_upload(snapshot);
        }
    }

    fn press_u(ui: &mut Ui, snapshot: &Snapshot) {
        let (tx, _rx) = mpsc::channel(4);
        ui.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE), snapshot, &tx);
    }

    fn press_y(ui: &mut Ui, snapshot: &Snapshot) {
        ui.on_confirm_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE), snapshot);
    }

    /// The view drawn at `cols` by `rows`.
    fn rendered(ui: &mut Ui, snapshot: &Snapshot, cols: u16, rows: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(cols, rows)).expect("test backend");
        terminal.draw(|frame| draw(frame, snapshot, ui)).expect("drawing");
        terminal.backend().to_string()
    }

    /// Whether `ui`'s footer holds a line starting with `prefix`.
    fn has_line(ui: &Ui, prefix: &str) -> bool {
        ui.upload().status_lines().iter().any(|line| line.starts_with(prefix))
    }

    #[test]
    fn upload_key_shows_notice_when_no_credentials() {
        let snapshot = busy();
        let mut ui = Ui::default();
        press_u(&mut ui, &snapshot);
        assert_eq!(
            ui.notice(snapshot.now_ms),
            Some("no upload credentials; paste a key in settings (c)")
        );
        assert!(!ui.upload().confirming());
    }

    #[test]
    fn upload_key_opens_confirm_with_row_count_when_capture_has_rows() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = busy();
        let mut ui = uploading(two_rows(&dir), NOWHERE.to_owned());
        press_u(&mut ui, &snapshot);
        assert_eq!(ui.upload().status_lines(), ["upload: preparing…"]);
        poll_until(&mut ui, &snapshot, 10, |ui| ui.upload().confirming());

        let rendered = rendered(&mut ui, &snapshot, 120, 40);
        assert!(rendered.contains("┌ upload to WDGWars ─"), "{rendered}");
        assert!(rendered.contains("2 rows to upload"), "{rendered}");
        assert!(rendered.contains("csv        "), "{rendered}");
        assert!(rendered.contains("compressed"), "{rendered}");
        assert!(rendered.contains("y upload · any other key cancels"), "{rendered}");
        assert!(ui.upload().status_lines().is_empty(), "{:?}", ui.upload());
    }

    #[test]
    fn upload_key_opens_one_confirm_for_both_sites_when_both_configured() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = busy();
        let mut ui = view(two_rows(&dir), both(true), NOWHERE, NOWHERE);
        press_u(&mut ui, &snapshot);
        poll_until(&mut ui, &snapshot, 10, |ui| ui.upload().confirming());

        let rendered = rendered(&mut ui, &snapshot, 120, 40);
        assert!(rendered.contains("┌ upload ─"), "{rendered}");
        assert!(rendered.contains("WDGWars  2 rows to upload"), "{rendered}");
        assert!(rendered.contains("WiGLE    2 rows to upload"), "{rendered}");
        assert!(rendered.contains("y upload both · any other key cancels"), "{rendered}");
        // The compact sections leave out the summary's details.
        assert!(!rendered.contains("positions"), "{rendered}");
    }

    #[test]
    fn upload_confirm_shows_refused_site_when_wigle_token_missing() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = busy();
        let mut ui = view(two_rows(&dir), both(false), NOWHERE, NOWHERE);
        press_u(&mut ui, &snapshot);
        poll_until(&mut ui, &snapshot, 10, |ui| ui.upload().confirming());

        let lines = ui.upload().confirm_lines().unwrap();
        assert!(lines.contains(&"WiGLE    cannot upload; see the footer".to_owned()), "{lines:?}");
        assert_eq!(lines.last().map(String::as_str), Some("y upload · any other key cancels"));
        assert_eq!(ui.upload().confirm_title().as_deref(), Some(" upload "));
        assert!(
            has_line(&ui, "upload: WiGLE failed: WiGLE needs both an API name and an API token"),
            "{:?}",
            ui.upload()
        );
    }

    #[test]
    fn upload_confirm_sends_nothing_when_canceled() {
        let dir = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let snapshot = busy();
        let mut ui = view(two_rows(&dir), both(true), &base, &base);
        press_u(&mut ui, &snapshot);
        poll_until(&mut ui, &snapshot, 10, |ui| ui.upload().confirming());

        ui.on_confirm_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), &snapshot);
        assert!(!ui.upload().confirming());
        assert_eq!(ui.notice(snapshot.now_ms), Some("not uploaded"));
        std::thread::sleep(Duration::from_millis(200));
        listener.set_nonblocking(true).unwrap();
        assert!(listener.accept().is_err(), "the upload was sent anyway");
    }

    #[test]
    fn upload_key_shows_notice_when_neither_site_has_rows() {
        use crate::testing::{capture, sighting};
        let dir = tempfile::tempdir().unwrap();
        let db = capture(&dir, vec![sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, None)]);
        let snapshot = busy();
        let mut ui = view(db, both(true), NOWHERE, NOWHERE);
        press_u(&mut ui, &snapshot);
        poll_until(&mut ui, &snapshot, 10, |ui| ui.upload().status_lines().is_empty());
        assert!(!ui.upload().confirming());
        assert_eq!(ui.notice(snapshot.now_ms), Some("no positioned rows to upload yet"));
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
        poll_until(&mut ui, &snapshot, 10, |ui| ui.upload().confirming());
        press_y(&mut ui, &snapshot);
        assert!(has_line(&ui, "upload: WDGWars sending "), "{:?}", ui.upload());
        // Through `wait`'s first poll, two seconds in.
        poll_until(&mut ui, &snapshot, 20, |ui| has_line(ui, "upload: WDGWars job 42 imported"));

        let rendered = rendered(&mut ui, &snapshot, 200, 40);
        assert!(
            rendered.contains("upload: WDGWars job 42 imported 1,200  captured 17"),
            "{rendered}"
        );
        server.join().unwrap();
    }

    #[test]
    fn upload_footer_shows_line_per_site_when_both_sent() {
        let dir = tempfile::tempdir().unwrap();
        let (wdgwars_base, wdgwars) = crate::testing::serve_sequence(&[
            ("202 Accepted", r#"{"ok":true,"job_id":42}"#),
            ("200 OK", r#"{"ok":true,"job_id":42,"status":"done","result":{"imported":2}}"#),
        ]);
        let (wigle_base, wigle) = crate::testing::serve_once(
            "200 OK",
            r#"{"success":true,"warning":"duplicate file",
                "results":{"transids":[{"transId":"20261122-00017"}]}}"#,
        );
        let snapshot = busy();
        let mut ui = view(two_rows(&dir), both(true), &wdgwars_base, &wigle_base);
        press_u(&mut ui, &snapshot);
        poll_until(&mut ui, &snapshot, 10, |ui| ui.upload().confirming());
        press_y(&mut ui, &snapshot);
        assert!(has_line(&ui, "upload: WDGWars sending "), "{:?}", ui.upload());
        assert!(has_line(&ui, "upload: WiGLE sending "), "{:?}", ui.upload());
        poll_until(&mut ui, &snapshot, 20, |ui| {
            has_line(ui, "upload: WDGWars job 42 imported") && has_line(ui, "upload: WiGLE queued")
        });

        assert_eq!(
            ui.upload().status_lines(),
            [
                "upload: WDGWars job 42 imported 2",
                "upload: WiGLE queued as 20261122-00017; see wigle.net/uploads; WiGLE warns: \
                 duplicate file",
            ]
        );
        wdgwars.join().unwrap();
        let request = String::from_utf8_lossy(&wigle.join().unwrap()).into_owned();
        assert!(request.starts_with("POST /api/v3/upload "), "{request}");
    }

    #[test]
    fn upload_footer_counts_a_line_per_site_when_both_have_status() {
        let snapshot = busy();
        let mut ui = Ui::default();
        let upload = ui.upload_mut();
        upload.wdgwars = Some("upload: WDGWars job 42 queued".to_owned());
        upload.wigle =
            Some("upload: WiGLE queued as 20261122-00017; see wigle.net/uploads".to_owned());

        let rendered = rendered(&mut ui, &snapshot, 120, 40);
        // A footer one line short would clip the second.
        assert!(rendered.contains("upload: WDGWars job 42 queued"), "{rendered}");
        assert!(rendered.contains("upload: WiGLE queued as 20261122-00017"), "{rendered}");
    }

    #[test]
    fn upload_footer_shows_error_when_send_fails() {
        let dir = tempfile::tempdir().unwrap();
        let (base, server) =
            crate::testing::serve_once("401 Unauthorized", r#"{"ok":false,"error":"bad key"}"#);
        let snapshot = busy();
        let mut ui = uploading(two_rows(&dir), base);
        press_u(&mut ui, &snapshot);
        poll_until(&mut ui, &snapshot, 10, |ui| ui.upload().confirming());
        press_y(&mut ui, &snapshot);
        poll_until(&mut ui, &snapshot, 10, |ui| has_line(ui, "upload: WDGWars failed:"));
        assert_eq!(
            ui.upload().status_lines(),
            ["upload: WDGWars failed: WDGWars rejected the API key: bad key"]
        );
        server.join().unwrap();
    }

    #[test]
    fn upload_debug_prints_no_credentials_when_confirming() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = busy();
        let mut ui = view(two_rows(&dir), both(true), NOWHERE, NOWHERE);
        press_u(&mut ui, &snapshot);
        poll_until(&mut ui, &snapshot, 10, |ui| ui.upload().confirming());
        let debug = format!("{:?}", ui.upload());
        assert!(!debug.contains("secret") && !debug.contains("exampletoken789"), "{debug}");
    }
}
