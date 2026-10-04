use super::{
    protocol::{BrokerReply, BrokerRequest, MAX_BROKER_MESSAGE},
    systemd_manager::SystemdManager,
};
use crate::settings::Settings;
use anyhow::Context;
use nix::{
    sys::socket::{getsockopt, sockopt::PeerCredentials},
    unistd::User,
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, BufReader, Read, Write},
    os::{
        fd::{AsFd, OwnedFd},
        unix::net::{UnixListener, UnixStream},
    },
};

const API_SOCKET_NAME: &str = "apollo-updated-supervisor";

pub fn activated_sockets(
    settings: &Settings,
) -> anyhow::Result<(UnixListener, BTreeMap<String, OwnedFd>)> {
    anyhow::ensure!(
        std::env::var("LISTEN_PID").ok().as_deref() == Some(&std::process::id().to_string()),
        "systemd broker socket activation PID mismatch"
    );
    let mut expected = BTreeMap::from([(API_SOCKET_NAME.to_owned(), None::<String>)]);
    for package in &settings.packages {
        if package
            .generation
            .as_ref()
            .and_then(|policy| policy.socket_unit.as_ref())
            .is_some()
        {
            let name = super::systemd_units::listener_name(&package.package_id);
            anyhow::ensure!(
                expected
                    .insert(name, Some(package.service_name.clone()))
                    .is_none(),
                "duplicate systemd listener descriptor name"
            );
        }
    }
    let count = std::env::var("LISTEN_FDS")?
        .parse::<usize>()
        .context("invalid systemd LISTEN_FDS")?;
    let names = std::env::var("LISTEN_FDNAMES")
        .context("systemd listener descriptor names are required")?;
    let names = names.split(':').collect::<Vec<_>>();
    anyhow::ensure!(
        count == names.len() && count == expected.len(),
        "systemd activated socket count does not match package policy"
    );
    let mut seen = BTreeSet::new();
    let mut api = None;
    let mut listeners = BTreeMap::new();
    for (index, name) in names.into_iter().enumerate() {
        anyhow::ensure!(
            seen.insert(name),
            "duplicate systemd socket descriptor name"
        );
        let service = expected
            .get(name)
            .with_context(|| format!("unexpected systemd socket descriptor name: {name}"))?;
        // LISTEN_PID and the configured count/name table validated this descriptor.
        let fd = crate::supervisor::fd_transfer::take_inherited_fd(3 + index as i32)?;
        anyhow::ensure!(
            crate::supervisor::fd_transfer::is_listening_stream(fd.as_fd())?,
            "systemd descriptor is not a listening stream socket"
        );
        if service.is_none() {
            api = Some(fd);
        } else {
            listeners.insert(service.as_ref().unwrap().clone(), fd);
        }
    }
    anyhow::ensure!(
        seen.len() == expected.len(),
        "systemd omitted a configured socket descriptor"
    );
    let api = api.context("updater supervisor API listener is missing")?;
    let api = UnixListener::from(api);
    Ok((api, listeners))
}

pub fn serve(
    settings: Settings,
    listener: UnixListener,
    listeners: BTreeMap<String, OwnedFd>,
) -> anyhow::Result<()> {
    let updater_uid = User::from_name("apollo-updated")?
        .context("dedicated updater account is missing")?
        .uid;
    let mut manager = SystemdManager::open(settings, listeners)?;
    for accepted in listener.incoming() {
        let stream = accepted?;
        if let Err(error) = handle(stream, updater_uid.as_raw(), &mut manager) {
            eprintln!("apollo-updated supervisor broker: {error:#}");
        }
    }
    Ok(())
}

fn handle(
    mut stream: UnixStream,
    updater_uid: u32,
    manager: &mut SystemdManager,
) -> anyhow::Result<()> {
    let peer = getsockopt(&stream, PeerCredentials).context("read broker peer credentials")?;
    anyhow::ensure!(peer.uid() == updater_uid, "unauthorized supervisor client");
    let mut line = Vec::new();
    BufReader::new(&mut stream)
        .take((MAX_BROKER_MESSAGE + 1) as u64)
        .read_until(b'\n', &mut line)?;
    anyhow::ensure!(
        line.len() <= MAX_BROKER_MESSAGE,
        "supervisor request too large"
    );
    let result = serde_json::from_slice::<BrokerRequest>(&line)
        .context("decode supervisor request")
        .and_then(|request| manager.dispatch(request));
    let reply = match result {
        Ok(result) => BrokerReply {
            ok: true,
            result,
            error: None,
        },
        Err(error) => BrokerReply {
            ok: false,
            result: Value::Null,
            error: Some(format!("{error:#}")),
        },
    };
    serde_json::to_writer(&mut stream, &reply)?;
    stream.write_all(b"\n")?;
    Ok(())
}
