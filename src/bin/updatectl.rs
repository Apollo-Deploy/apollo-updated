fn main() {
    if let Err(error) = apollo_updated::cli::updatectl_main() {
        eprintln!("apollo-updatectl: {error:#}");
        std::process::exit(1);
    }
}
