use serde::Serialize;

#[derive(Serialize)]
struct Event<'a> {
    event: &'static str,
    operation: &'a str,
    package_id: Option<&'a str>,
    outcome: &'a str,
}

pub fn write(operation: &str, package: Option<&str>, outcome: &str) {
    let event = Event {
        event: "apollo_updated_operation",
        operation,
        package_id: package,
        outcome,
    };
    if let Ok(line) = serde_json::to_string(&event) {
        eprintln!("{line}");
    }
}
