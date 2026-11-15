#[tokio::main]
async fn main() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    atlassian_cli::updater::cleanup_stale_old_binary();

    if let Err(err) = atlassian_cli::cli::run().await {
        eprintln!("오류: {err:#}");
        std::process::exit(1);
    }
}
