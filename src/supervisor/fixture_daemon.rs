use super::{CONTROL_FD, CommandKind, ControlReply, ControlRequest, LISTENER_FD, MAX_CONTROL_LINE};
use anyhow::{Context, ensure};
use std::{
    io::{self, Write},
    os::{
        fd::{AsFd, RawFd},
        unix::net::{UnixListener, UnixStream},
    },
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

/// Entry point for the disposable fixture daemon binary.
#[allow(unsafe_code)]
pub fn run_fixture_daemon() -> anyhow::Result<()> {
    use std::{
        io::{BufRead, BufReader},
        net::{TcpListener, TcpStream},
        os::fd::FromRawFd,
        sync::atomic::{AtomicU64, Ordering},
        thread,
    };

    fn handle_connection(
        mut stream: TcpStream,
        version: String,
        active: std::sync::Arc<AtomicU64>,
    ) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(300)));
        let _ = stream.write_all(format!("accepted:{version}\n").as_bytes());
        let mut reader = BufReader::new(stream.try_clone().expect("clone fixture connection"));
        let mut request = String::new();
        let read = reader.read_line(&mut request);
        if read.is_ok() && request.trim() == "finish" {
            let _ = stream.write_all(format!("finished:{version}\n").as_bytes());
        }
        active.fetch_sub(1, Ordering::AcqRel);
    }

    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    let version = if args.first().is_some_and(|arg| arg == "--version") {
        let version = args.get(1).context("missing fixture version")?.clone();
        args.remove(0);
        args.remove(0);
        version
    } else {
        std::env::var("APOLLO_PACKAGE_VERSION").context("missing fixture version")?
    };
    let mut healthy = true;
    let mut crash_on_activate = false;
    let mut crash_after_activate = false;
    let mut delay_drain = false;
    for arg in args {
        match arg.as_str() {
            "--unhealthy" => healthy = false,
            "--crash-on-activate" => crash_on_activate = true,
            "--crash-after-activate" => crash_after_activate = true,
            "--drain-delay" => delay_drain = true,
            _ => anyhow::bail!("unknown fixture argument"),
        }
    }
    let control_fd = std::env::var("APOLLO_UPDATED_CONTROL_FD")
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
        .unwrap_or(CONTROL_FD);
    let control_is_listener = socket_accepting(control_fd)?;
    let expect_listener = std::env::var("APOLLO_UPDATED_EXPECT_LISTENER_FD").as_deref() == Ok("1");
    let initial_listener = if control_is_listener {
        None
    } else {
        // SAFETY: the in-process fixture supervisor duplicates its TCP listener onto fd 3.
        let listener = unsafe { TcpListener::from_raw_fd(LISTENER_FD) };
        listener.set_nonblocking(true)?;
        Some(listener)
    };
    let listener = std::sync::Arc::new(std::sync::Mutex::new(initial_listener));
    let accepting = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopping = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let active = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let gate = std::sync::Arc::new(std::sync::Mutex::new(()));
    let handlers = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let accept_thread = {
        let accepting = accepting.clone();
        let stopping = stopping.clone();
        let active = active.clone();
        let gate = gate.clone();
        let handlers = handlers.clone();
        let listener = listener.clone();
        let version = version.clone();
        thread::spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                let accepted = {
                    let _gate = gate.lock().unwrap();
                    if !accepting.load(Ordering::Acquire) {
                        None
                    } else {
                        match listener.lock().unwrap().as_ref() {
                            Some(listener) => match listener.accept() {
                                Ok((stream, _)) => {
                                    // Count under the same gate as Drain so no accepted
                                    // connection can be invisible to the drain barrier.
                                    active.fetch_add(1, Ordering::AcqRel);
                                    Some(stream)
                                }
                                Err(error) if error.kind() == io::ErrorKind::WouldBlock => None,
                                Err(_) => None,
                            },
                            None => None,
                        }
                    }
                };
                if let Some(stream) = accepted {
                    let active = active.clone();
                    let version = version.clone();
                    handlers.lock().unwrap().push(thread::spawn(move || {
                        handle_connection(stream, version, active)
                    }));
                } else {
                    thread::sleep(Duration::from_millis(2));
                }
            }
        })
    };

    if control_is_listener {
        // SAFETY: systemd transfers this generation's private control listener on fd 3.
        let control = unsafe { UnixListener::from_raw_fd(control_fd) };
        'connections: for accepted in control.incoming() {
            let mut stream = accepted?;
            stream.set_read_timeout(Some(Duration::from_secs(30)))?;
            stream.set_write_timeout(Some(Duration::from_secs(30)))?;
            ensure_root_peer(&stream)?;
            loop {
                let request_fd = match crate::supervisor::fd_transfer::receive_marker(&stream) {
                    Ok(fd) => fd,
                    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
                    Err(error) => return Err(error.into()),
                };
                let request: ControlRequest = read_request(&mut stream)?;
                if process_request(
                    &mut stream,
                    &request,
                    request_fd,
                    &version,
                    healthy,
                    crash_on_activate,
                    crash_after_activate,
                    delay_drain,
                    expect_listener,
                    &listener,
                    &accepting,
                    &active,
                    &gate,
                    &stopping,
                )? {
                    break 'connections;
                }
            }
        }
    } else {
        // SAFETY: the in-process qualification supervisor maps a private Unix stream onto fd 4.
        let mut control = unsafe { UnixStream::from_raw_fd(control_fd) };
        loop {
            let request_fd = match crate::supervisor::fd_transfer::receive_marker(&control) {
                Ok(fd) => fd,
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(error) => return Err(error.into()),
            };
            let request = read_request(&mut control)?;
            if process_request(
                &mut control,
                &request,
                request_fd,
                &version,
                healthy,
                crash_on_activate,
                crash_after_activate,
                delay_drain,
                expect_listener,
                &listener,
                &accepting,
                &active,
                &gate,
                &stopping,
            )? {
                break;
            }
        }
    }
    stopping.store(true, Ordering::Release);
    accept_thread
        .join()
        .map_err(|_| anyhow::anyhow!("fixture accept thread panicked"))?;
    for handler in handlers.lock().unwrap().drain(..) {
        let _ = handler.join();
    }
    Ok(())
}

fn read_request(stream: &mut UnixStream) -> anyhow::Result<ControlRequest> {
    use std::io::Read;
    let mut line = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        if stream.read(&mut byte)? == 0 {
            if line.is_empty() {
                anyhow::bail!("control stream closed before its request");
            }
            break;
        }
        if byte[0] == b'\n' {
            break;
        }
        anyhow::ensure!(
            line.len() < MAX_CONTROL_LINE,
            "fixture request exceeded its size limit"
        );
        line.push(byte[0]);
    }
    Ok(serde_json::from_slice(&line)?)
}

fn ensure_root_peer(stream: &UnixStream) -> anyhow::Result<()> {
    let peer = nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::PeerCredentials)?;
    ensure!(peer.uid() == 0, "generation control peer is not root");
    Ok(())
}

fn process_request(
    control: &mut UnixStream,
    request: &ControlRequest,
    request_fd: Option<std::os::fd::OwnedFd>,
    version: &str,
    healthy: bool,
    crash_on_activate: bool,
    crash_after_activate: bool,
    delay_drain: bool,
    expect_listener: bool,
    listener: &std::sync::Arc<std::sync::Mutex<Option<std::net::TcpListener>>>,
    accepting: &Arc<AtomicBool>,
    active: &Arc<AtomicU64>,
    gate: &Arc<Mutex<()>>,
    stopping: &Arc<AtomicBool>,
) -> anyhow::Result<bool> {
    if let Some(fd) = request_fd {
        ensure!(
            request.command == CommandKind::Activate && expect_listener,
            "listener fd arrived outside a configured activation"
        );
        ensure!(
            crate::supervisor::fd_transfer::is_listening_stream(fd.as_fd())?,
            "received app descriptor is not a listening stream"
        );
        ensure!(
            listener.lock().unwrap().is_none(),
            "generation already owns an app listener"
        );
        let app_listener = std::net::TcpListener::from(fd);
        app_listener.set_nonblocking(true)?;
        *listener.lock().unwrap() = Some(app_listener);
    }
    let listener_installed = listener.lock().unwrap().is_some();
    let ok = match request.command {
        CommandKind::Health => true,
        CommandKind::Activate => {
            if crash_on_activate {
                std::process::exit(70);
            }
            if healthy && (!expect_listener || listener_installed) {
                let _gate = gate.lock().unwrap();
                accepting.store(true, Ordering::Release);
                true
            } else {
                false
            }
        }
        CommandKind::Drain => {
            let _gate = gate.lock().unwrap();
            accepting.store(false, Ordering::Release);
            if delay_drain {
                thread::sleep(Duration::from_millis(u64::from(request.timeout_ms) + 250));
                false
            } else {
                true
            }
        }
        CommandKind::Resume => {
            let _gate = gate.lock().unwrap();
            accepting.store(true, Ordering::Release);
            true
        }
        CommandKind::Stop => {
            let _gate = gate.lock().unwrap();
            accepting.store(false, Ordering::Release);
            active.load(Ordering::Acquire) == 0
        }
    };
    let response = ControlReply {
        id: request.id,
        pid: std::process::id(),
        version: version.to_owned(),
        ok,
        healthy,
        accepting: accepting.load(Ordering::Acquire),
        active_connections: active.load(Ordering::Acquire),
        listener_installed,
    };
    serde_json::to_writer(&mut *control, &response)?;
    control.write_all(b"\n")?;
    control.flush()?;
    if request.command == CommandKind::Activate && crash_after_activate && ok {
        std::process::exit(71);
    }
    let should_stop = request.command == CommandKind::Stop && ok;
    if should_stop {
        stopping.store(true, Ordering::Release);
    }
    Ok(should_stop)
}

#[allow(unsafe_code)]
fn socket_accepting(fd: RawFd) -> io::Result<bool> {
    let mut accepting = 0_i32;
    let mut size = std::mem::size_of::<i32>() as libc::socklen_t;
    // SAFETY: getsockopt reads SO_ACCEPTCONN from the systemd/inherited fd.
    let result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ACCEPTCONN,
            (&mut accepting as *mut i32).cast(),
            &mut size,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(accepting != 0)
}
