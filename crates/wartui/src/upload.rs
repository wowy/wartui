//! `wartui upload` — send a capture to WDGWars and WiGLE.
//!
//! What is uploaded, the per-site bodies and cutoffs, the record each upload leaves in the
//! capture and the refusal of test data are [`wartui_app::upload`]'s. This command adds the
//! question, the terminal check and the exit code. [`text`] words an upload for this command and
//! the fleet view's `u` alike.
//!
//! Nothing is sent without the operator seeing what will go and saying yes, because an upload
//! cannot be taken back. One question covers every site. Without a terminal to ask on, that is
//! known before any body is built, so it is refused first.
//!
//! Each ready site is sent to in turn, each finished before the next starts, so WDGWars' job is
//! followed before WiGLE's send starts, which has no deadline. A script that sees a non-zero exit
//! retries, so the exit is non-zero only when a site was refused before it queued the upload,
//! WDGWars reports its import failed, or how its job ended could not be recorded
//! ([`Outcome::failed`]). The retry is safe: a site that queued reads "nothing new".

pub(crate) mod text;

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args as ClapArgs, ValueEnum};
use wartui_app::upload::wdgwars::Waited;
use wartui_app::upload::{
    Consent, Endpoints, Options, Outcome, Prepare, Progress, SiteError, prepare, upload,
};
use wartui_core::export::ExportSummary;
use wartui_core::store::UploadSite;

use crate::export::{Selection, note_excluded, note_quality, note_unknown_kind, note_unpositioned};
use text::{
    Site, describe, end_unrecorded_text, import_failed_text, queued_unrecorded_text, refusal_text,
    send_error_text, site_error_text, site_refusal_text, state_name, unfollowed_text, utc_minute,
};

#[derive(ClapArgs, Debug)]
pub struct Args {
    #[command(flatten)]
    selection: Selection,

    /// Send to this site only. Every site with credentials by default.
    #[arg(long, value_name = "SITE")]
    to: Option<To>,

    /// Upload without asking first.
    #[arg(long, short = 'y')]
    yes: bool,

    /// Send everything the selection covers, ignoring what earlier uploads sent.
    #[arg(long)]
    resend: bool,
}

/// A site `--to` names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum To {
    Wdgwars,
    Wigle,
}

impl To {
    fn site(self) -> UploadSite {
        match self {
            Self::Wdgwars => UploadSite::Wdgwars,
            Self::Wigle => UploadSite::Wigle,
        }
    }
}

pub fn run(args: Args) -> Result<()> {
    let (config, config_path) = args.selection.config()?;
    let db = args.selection.capture("reading")?;
    if !args.yes && !std::io::stdin().is_terminal() {
        bail!("standard input is not a terminal, so nothing can answer; pass --yes to upload");
    }
    let options = Options {
        to: args.to.map(To::site),
        recapture: args.selection.recapture(),
        resend: args.resend,
        excluded_ssids: args.selection.filter(&config).excluded_ssids,
        ..Options::new(env!("CARGO_PKG_VERSION"))
    };
    let outcomes = prepare(&config.api_keys, &db, &options)
        .map_err(|refusal| anyhow!(refusal_text(&refusal, &db, config_path.as_deref())))?;
    let names = text::names(&db, &outcomes);

    let mut failed = Vec::new();
    let mut ready = Vec::new();
    for (at, (site, outcome)) in outcomes.into_iter().enumerate() {
        if at > 0 {
            eprintln!();
        }
        let label = site.label();
        match outcome {
            Prepare::Ready(prepared) => {
                eprint!("{label}: {}", describe(&prepared, &names));
                notes(prepared.summary());
                ready.push(prepared);
            }
            Prepare::NothingNew { previous, summary } => {
                eprintln!(
                    "{label}: everything in {} was uploaded ({} {}, {}); --resend sends it again",
                    db.display(),
                    site.job(),
                    previous.job,
                    utc_minute(previous.uploaded_at_ms)
                );
                notes(&summary);
            }
            Prepare::NoRows(summary) => {
                eprintln!("{label}: {} has no rows to upload", db.display());
                notes(&summary);
            }
            Prepare::Refused(refusal) => {
                let why = site_refusal_text(site, &refusal, config_path.as_deref());
                eprintln!("not uploaded to {label}: {why}");
                failed.push(site);
            }
        }
    }
    if ready.is_empty() {
        return failures(&failed);
    }
    let labels: Vec<&str> = ready.iter().map(|prepared| prepared.site().label()).collect();
    if !args.yes && !confirm(&labels.join(" and "))? {
        eprintln!("not uploaded");
        return failures(&failed);
    }

    // In `SITES` order, each finished before the next starts, so WDGWars' job is followed
    // without waiting on WiGLE's send, which has no deadline. The operator has said yes, or
    // passed `--yes`, to every one of them.
    let endpoints = Endpoints::live();
    for prepared in &ready {
        let site = prepared.site();
        let outcome = upload(prepared, Consent::to(prepared), &db, &endpoints, on_stderr());
        report(site, &outcome, &db);
        if outcome.failed() {
            failed.push(site);
        }
    }
    failures(&failed)
}

/// Print how the upload to `site` of `db` ended: a finished job's counts on standard output,
/// everything else on standard error.
fn report(site: UploadSite, outcome: &Outcome, db: &Path) {
    match outcome {
        Outcome::SendFailed(error) => {
            eprintln!("{} upload failed: {}", site.label(), send_error_text(site, error, db));
        }
        Outcome::Wigle { queued, unrecorded } => {
            eprintln!(
                "uploaded; WiGLE queued it as transaction {}. Its import carries on; check \
                 https://wigle.net/uploads for the result.",
                queued.trans_id
            );
            if let Some(warning) = &queued.warning {
                eprintln!("WiGLE warns: {warning}");
            }
            // WiGLE has the file and the row is a cutoff, so this is no failed upload.
            if let Some(error) = unrecorded {
                eprintln!("{}", queued_unrecorded_text(&queued.trans_id, error));
            }
        }
        Outcome::Wdgwars { job, url, waited, unrecorded } => {
            match waited {
                Waited::Done(counts) => print!("{}", text::outcome(counts)),
                Waited::Unfollowed(reason) => eprintln!(
                    "WDGWars accepted the upload as job {job}. Its import carries on; check {url} \
                     or your profile on wdgwars.pl for the result. wartui stopped following it: \
                     {}.",
                    unfollowed_text(reason)
                ),
                Waited::Failed(message) => eprintln!("{}", import_failed_text(*job, message)),
            }
            if let Some(error) = unrecorded {
                eprintln!("{}", end_unrecorded_text(*job, error));
            }
        }
    }
}

/// A `say` for [`upload`] that prints the job WDGWars queued, each change of its state and each
/// distinct poll error.
fn on_stderr() -> impl FnMut(Progress) {
    let mut said = None;
    let mut last_error: Option<SiteError> = None;
    move |progress| match progress {
        Progress::Queued { job } => eprintln!("uploaded; WDGWars queued it as job {job}"),
        Progress::State { job, state } => {
            if said != Some(state) {
                eprintln!("job {job}: {}", state_name(state));
                said = Some(state);
            }
        }
        Progress::PollFailed { job, error } => {
            if last_error.as_ref() != Some(&error) {
                let message = site_error_text(UploadSite::Wdgwars, &error);
                eprintln!("job {job}: asking after it failed ({message}); trying again");
                last_error = Some(error);
            }
        }
    }
}

/// An error naming every site in `failed`, or `Ok` when it is empty.
fn failures(failed: &[UploadSite]) -> Result<()> {
    if failed.is_empty() {
        return Ok(());
    }
    let labels: Vec<&str> = failed.iter().map(|site| site.label()).collect();
    bail!("the upload to {} failed", labels.join(" and "))
}

/// What `export` says of the rows a summary left out.
fn notes(summary: &ExportSummary) {
    note_unpositioned(summary);
    note_unknown_kind(summary);
    note_excluded(summary);
    note_quality(summary);
}

/// Ask on the terminal whether to upload to `sites`. `run` has already refused when standard
/// input is not one.
fn confirm(sites: &str) -> Result<bool> {
    eprint!("Upload to {sites}? [y/N] ");
    std::io::stderr().flush().context("writing the question")?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer).context("reading the answer")?;
    Ok(matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::To;

    #[test]
    fn upload_args_take_to_when_site_named() {
        for (name, to) in [("wdgwars", To::Wdgwars), ("wigle", To::Wigle)] {
            let cli = crate::Cli::try_parse_from(["wartui", "upload", "--to", name]).unwrap();
            let Some(crate::Command::Upload(args)) = cli.command else { panic!("not upload") };
            assert_eq!(args.to, Some(to));
            assert_eq!(args.selection.recapture(), None);
        }
        let cli = crate::Cli::try_parse_from(["wartui", "upload"]).unwrap();
        let Some(crate::Command::Upload(args)) = cli.command else { panic!("not upload") };
        assert_eq!(args.to, None);
    }

    #[test]
    fn upload_args_refuse_to_when_site_unknown() {
        assert!(crate::Cli::try_parse_from(["wartui", "upload", "--to", "openwifi"]).is_err());
    }
}
