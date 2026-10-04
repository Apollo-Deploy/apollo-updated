#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    if let Err(error) = apollo_updated::cli::run().await {
        eprintln!("apollo-updated: {error:#}");
        std::process::exit(1);
    }
}
