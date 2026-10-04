use super::{GenerationHandle, GenerationRecord, SystemdManager, systemd_units};
use anyhow::{Context, ensure};

pub(super) fn handle_for_record(
    manager: &SystemdManager,
    service: &str,
    generation_id: &str,
) -> anyhow::Result<GenerationHandle> {
    let package = manager.package(service)?;
    let record = manager
        .registry
        .services
        .get(service)
        .and_then(|state| state.generations.get(generation_id))
        .context("generation is not registered for service")?;
    ensure!(
        record.plan.id == generation_id,
        "generation registry key mismatch"
    );
    ensure!(
        record.plan.package_id == package.package_id,
        "registered package mismatch"
    );
    ensure!(
        record.pid > 0 && !record.invocation_id.is_empty(),
        "generation has no live identity"
    );
    Ok(GenerationHandle {
        service: service.to_owned(),
        package_id: record.plan.package_id.clone(),
        generation_id: record.plan.id.clone(),
        pid: record.pid,
        invocation_id: record.invocation_id.clone(),
        version: record.plan.version.clone(),
        digest: record.plan.digest.clone(),
    })
}

pub(super) fn record_for_handle(
    manager: &SystemdManager,
    handle: &GenerationHandle,
) -> anyhow::Result<GenerationRecord> {
    let package = manager.package(&handle.service)?;
    ensure!(
        package.package_id == handle.package_id,
        "package identity mismatch"
    );
    let record = manager
        .registry
        .services
        .get(&handle.service)
        .and_then(|state| state.generations.get(&handle.generation_id))
        .context("generation handle is not registered")?;
    ensure!(
        record.plan.id == handle.generation_id
            && record.plan.package_id == handle.package_id
            && record.plan.version == handle.version
            && record.plan.digest == handle.digest
            && record.pid == handle.pid
            && record.invocation_id == handle.invocation_id,
        "stale or mismatched generation handle"
    );
    Ok(record.clone())
}

pub(super) fn is_alive(
    manager: &SystemdManager,
    handle: &GenerationHandle,
) -> anyhow::Result<bool> {
    let Ok(record) = record_for_handle(manager, handle) else {
        return Ok(false);
    };
    if super::current_pid(&record.plan.service_unit)? != handle.pid {
        return Ok(false);
    }
    Ok(systemd_units::property(&record.plan.service_unit, "InvocationID")? == handle.invocation_id)
}
