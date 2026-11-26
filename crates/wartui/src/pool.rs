//! `wartui pool` — read or set the channel pool the bridge holds.
//!
//! The bridge keeps the pool on its own flash and boots into it, so it holds across the bridge's
//! reboots and with no host attached, and `run`'s planner deals the pool it reports.
//! `wartui.toml` holds no copy. The bridge is resolved to one board as `wartui fan` resolves it.
//! Every P4 holds a pool, carrier or not.
//!
//! With a pool, the command waits a moment before it reads the pool back, so a failed save's
//! fault prints first.

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Args as ClapArgs, ValueEnum};
use wartui_app::ops;
use wartui_proto::mac;
use wartui_proto::plan::ChannelPool;

use crate::status::{NO_STATUS, link_error, print_fault};

/// A channel pool, as the command line spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Pool {
    /// FCC-permitted unlicensed channels: 2.4 GHz 1–11 and 5 GHz 36–165.
    Us,
    /// ETSI-permitted unlicensed channels: 2.4 GHz 1–13 and 5 GHz 36–140.
    Eu,
    /// Every channel a node can tune: 2.4 GHz 1–13 and all of 5 GHz.
    All,
}

impl From<Pool> for ChannelPool {
    fn from(pool: Pool) -> Self {
        match pool {
            Pool::Us => Self::Us,
            Pool::Eu => Self::Eu,
            Pool::All => Self::All,
        }
    }
}

#[derive(ClapArgs)]
pub struct Args {
    /// The pool to set. The bridge keeps it across its reboots. Without it, prints the pool.
    #[arg(value_name = "POOL")]
    pool: Option<Pool>,

    /// The bridge, as a device path, the board's address, or the end of it (`00:08`). Detected
    /// if omitted.
    #[arg(long, value_name = "PATH|MAC")]
    bridge: Option<String>,

    #[command(flatten)]
    link_flag: crate::config::LinkFlag,

    /// Read the remembered bridge from this file instead of the default `wartui.toml`. A file
    /// that does not load is no bridge remembered. Never written; see
    /// `crates/wartui/README.md` § "Config file".
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

pub async fn run(args: Args) -> Result<()> {
    let (mut link, info) =
        super::connect_one(args.bridge.as_deref(), args.link_flag.link, args.config.as_deref())
            .await?;

    let wanted = args.pool.map(ChannelPool::from);
    let (status, taken) = match wanted {
        Some(pool) => {
            println!("setting the pool to {pool} on bridge {}", mac::full(&info.mac));
            let set = ops::set_pool(&mut link, pool, &mut print_fault).await;
            let set = set.map_err(|error| link_error(error, NO_STATUS))?;
            (set.status, set.taken)
        }
        None => {
            let status = ops::status(&mut link, ops::REPLY_TIMEOUT, &mut print_fault).await;
            (status.map_err(|error| link_error(error, NO_STATUS))?, true)
        }
    };
    println!("pool       {}", status.pool);
    if !taken {
        bail!("the bridge did not take the pool");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::ValueEnum;
    use wartui_proto::plan::ChannelPool;

    use super::Pool;

    #[test]
    fn pool_cmd_maps_each_spelling_to_its_pool_when_parsing() {
        for (spelling, pool) in
            [("us", ChannelPool::Us), ("eu", ChannelPool::Eu), ("all", ChannelPool::All)]
        {
            let parsed = Pool::from_str(spelling, false).expect("a pool");
            assert_eq!(ChannelPool::from(parsed), pool, "{spelling}");
        }
        assert!(Pool::from_str("jp", false).is_err());
    }
}
