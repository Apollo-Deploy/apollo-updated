use super::GenerationRecord;
use anyhow::ensure;

pub(super) fn reconcile_record_pid(record: &mut GenerationRecord, systemd_main_pid: u32) -> bool {
    let changed = record.pid != systemd_main_pid;
    record.pid = systemd_main_pid;
    changed
}

pub(super) fn ensure_stoppable_pid(
    requested_pid: u32,
    systemd_main_pid: u32,
) -> anyhow::Result<()> {
    ensure!(
        systemd_main_pid == 0 || systemd_main_pid == requested_pid,
        "generation PID no longer matches systemd MainPID"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ensure_stoppable_pid, reconcile_record_pid};
    use crate::supervisor::systemd_manager::GenerationRecord;
    use crate::supervisor::systemd_units::UnitPlan;
    use std::path::PathBuf;

    #[test]
    fn systemd_absence_clears_persisted_generation_pid() {
        let mut record = GenerationRecord {
            pid: 412,
            invocation_id: "invocation-1".into(),
            plan: UnitPlan {
                id: "fixture-generation".into(),
                package_id: "fixture".into(),
                version: "1.0.0".into(),
                digest: "digest".into(),
                service_unit: "fixture.service".into(),
                control_socket_unit: "fixture.socket".into(),
                control_path: PathBuf::from("/run/fixture.sock"),
                immutable_tree: PathBuf::from("/var/lib/fixture"),
                listener_socket_unit: None,
            },
        };

        assert!(reconcile_record_pid(&mut record, 0));
        assert_eq!(record.pid, 0);
        assert!(!reconcile_record_pid(&mut record, 0));
        assert!(reconcile_record_pid(&mut record, 731));
        assert_eq!(record.pid, 731);
    }

    #[test]
    fn stopped_pid_never_stops_a_replacement_process() {
        assert!(ensure_stoppable_pid(412, 0).is_ok());
        assert!(ensure_stoppable_pid(412, 412).is_ok());
        assert!(ensure_stoppable_pid(412, 731).is_err());
    }
}
