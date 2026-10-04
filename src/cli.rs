use crate::{api, disk::Store, settings::Settings};
use clap::{Parser, Subcommand};
use serde::Serialize;
use std::{
    fs::OpenOptions,
    io::{BufRead, BufReader, Read, Write},
    os::unix::fs::OpenOptionsExt,
    os::unix::net::UnixStream,
    path::PathBuf,
};

#[derive(Debug, Parser)]
#[command(name = "apollo-updated")]
struct DaemonArgs {
    #[command(subcommand)]
    command: DaemonCommand,
}

#[derive(Debug, Subcommand)]
enum DaemonCommand {
    Serve,
    SupervisorBroker,
    VerifyRoot {
        path: PathBuf,
        #[arg(long)]
        reject_keys_from: Option<PathBuf>,
    },
}

#[derive(Debug, Parser)]
#[command(name = "apollo-updatectl")]
pub struct ClientArgs {
    #[command(subcommand)]
    command: ClientCommand,
}

#[derive(Debug, Subcommand)]
enum ClientCommand {
    Status,
    Check {
        package: Option<String>,
    },
    Update {
        package: Option<String>,
        #[arg(long)]
        all: bool,
    },
    Rollback {
        package: String,
    },
    History {
        package: String,
    },
    Verify {
        package: String,
    },
    Gc,
    Doctor,
}

#[derive(Serialize)]
struct Request<'a> {
    command: &'a str,
    package: Option<&'a str>,
}

pub async fn run() -> anyhow::Result<()> {
    match DaemonArgs::try_parse() {
        Ok(args) => match args.command {
            DaemonCommand::Serve => {
                let settings = Settings::load()?;
                let store = Store::open(&settings.data_root)?;
                let supervisor = crate::supervisor::SystemdSupervisor;
                crate::lifecycle::recover_packages(&store, &supervisor, &settings.packages)?;
                let listener = api::activated_socket()?;
                api::serve(settings, store, listener).await
            }
            DaemonCommand::SupervisorBroker => {
                let settings = Settings::load()?;
                let (listener, listener_fds) =
                    crate::supervisor::broker::activated_sockets(&settings)?;
                crate::supervisor::broker::serve(settings, listener, listener_fds)
            }
            DaemonCommand::VerifyRoot {
                path,
                reject_keys_from,
            } => {
                if !path.is_absolute() {
                    anyhow::bail!("trust root path must be absolute");
                }
                let bytes = read_root_file(&path)?;
                if let Some(reject_path) = reject_keys_from {
                    if !reject_path.is_absolute() {
                        anyhow::bail!("excluded trust root path must be absolute");
                    }
                    let excluded = read_root_file(&reject_path)?;
                    crate::tuf_client::validate_trust_anchor_excluding(&bytes, &excluded)?;
                } else {
                    crate::tuf_client::validate_trust_anchor(&bytes)?;
                }
                println!("valid TUF trust root");
                Ok(())
            }
        },
        Err(error)
            if error.kind() == clap::error::ErrorKind::UnknownArgument
                || error.kind() == clap::error::ErrorKind::InvalidSubcommand =>
        {
            run_client()
        }
        Err(error) => Err(error.into()),
    }
}

fn read_root_file(path: &std::path::Path) -> anyhow::Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > crate::tuf_client::MAX_TRUST_ROOT_BYTES {
        anyhow::bail!("trust root must be a regular file no larger than 1 MiB");
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(crate::tuf_client::MAX_TRUST_ROOT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > crate::tuf_client::MAX_TRUST_ROOT_BYTES {
        anyhow::bail!("trust root exceeds configured bound");
    }
    Ok(bytes)
}

fn run_client() -> anyhow::Result<()> {
    let args = ClientArgs::parse();
    let config = Settings::load()?;
    let (command, package) = match &args.command {
        ClientCommand::Status => ("status", None),
        ClientCommand::Check { package } => ("check", package.as_deref()),
        ClientCommand::Update { package, all } => {
            if *all {
                ("update_all", None)
            } else {
                ("update", package.as_deref())
            }
        }
        ClientCommand::Rollback { package } => ("rollback", Some(package.as_str())),
        ClientCommand::History { package } => ("history", Some(package.as_str())),
        ClientCommand::Verify { package } => ("verify", Some(package.as_str())),
        ClientCommand::Gc => ("gc", None),
        ClientCommand::Doctor => ("doctor", None),
    };
    let mut stream = UnixStream::connect(&config.socket_path)?;
    serde_json::to_writer(&mut stream, &Request { command, package })?;
    stream.write_all(b"\n")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let reply = serde_json::from_str::<serde_json::Value>(&line)?;
    println!("{}", serde_json::to_string_pretty(&reply)?);
    if reply.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        anyhow::bail!(
            "updater request failed: {}",
            reply
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("invalid daemon response")
        );
    }
    Ok(())
}

pub fn updatectl_main() -> anyhow::Result<()> {
    run_client()
}
