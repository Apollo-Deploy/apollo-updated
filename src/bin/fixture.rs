fn main() {
    if let Err(error) = apollo_updated::supervisor::fixture::run_fixture_daemon() {
        eprintln!("fixture daemon failed: {error:#}");
        std::process::exit(1);
    }
}
