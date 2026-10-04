use crate::{
    contract::{PackageContract, Readiness},
    supervisor::Supervisor,
};
use anyhow::{Context, ensure};
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub(super) fn verify_health(
    supervisor: &dyn Supervisor,
    contract: &PackageContract,
    generation: &crate::supervisor::GenerationHandle,
    package: &crate::settings::AllowedPackage,
) -> anyhow::Result<()> {
    ensure!(
        package.allows_readiness(&contract.health.readiness),
        "health probe is not authorized by the local package policy"
    );

    let deadline = Instant::now() + Duration::from_millis(u64::from(contract.health_timeout_ms));
    let stable_for = Duration::from_millis(u64::from(
        contract
            .stabilization_ms
            .max(contract.health.stabilization_ms),
    ));
    let mut stable_since = None;
    let mut failures = 0_u32;
    loop {
        let pid_healthy = supervisor
            .health_of_generation(generation, remaining_ms(deadline))
            .unwrap_or(false);
        let passed = pid_healthy
            && match &contract.health.readiness {
                Readiness::ProcessAlive | Readiness::SupervisorReady => true,
                Readiness::UnixProbe {
                    socket,
                    request,
                    expected,
                    timeout_ms,
                } => unix_probe(socket, request, expected, *timeout_ms).unwrap_or(false),
                Readiness::AllowlistedExecutable {
                    path,
                    argv,
                    timeout_ms,
                    expected_exit,
                } => {
                    run_health_executable(path, argv, *timeout_ms, *expected_exit).unwrap_or(false)
                }
            };
        if passed {
            failures = 0;
            let since = stable_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= stable_for {
                return Ok(());
            }
        } else {
            failures = failures.saturating_add(1);
            if failures >= contract.health.failure_threshold {
                anyhow::bail!("candidate health failure threshold reached");
            }
            stable_since = None;
        }
        if Instant::now() >= deadline {
            anyhow::bail!("candidate health timeout");
        }
        thread::sleep(
            Duration::from_millis(25).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

fn unix_probe(
    socket: &std::path::Path,
    request: &[u8],
    expected: &[u8],
    timeout_ms: u32,
) -> anyhow::Result<bool> {
    let mut stream = UnixStream::connect(socket)?;
    let timeout = Duration::from_millis(u64::from(timeout_ms.max(1)));
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(request)?;
    let mut reply = vec![0; expected.len().saturating_add(1)];
    let count = stream.read(&mut reply)?;
    Ok(&reply[..count] == expected)
}

fn run_health_executable(
    path: &std::path::Path,
    argv: &[String],
    timeout_ms: u32,
    expected_exit: i32,
) -> anyhow::Result<bool> {
    let mut child = Command::new(path)
        .args(argv)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("start allowlisted health executable")?;
    let deadline = Instant::now() + Duration::from_millis(u64::from(timeout_ms.max(1)));
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status.code() == Some(expected_exit));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn remaining_ms(deadline: Instant) -> u32 {
    u32::try_from(
        deadline
            .saturating_duration_since(Instant::now())
            .as_millis(),
    )
    .unwrap_or(u32::MAX)
    .max(1)
}
