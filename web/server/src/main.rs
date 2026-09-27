mod api;
mod auth;
mod config;
mod daemon;
mod history;
mod httpsredirect;
mod state;
mod store;
mod tls;

use state::AppState;
use tower_http::services::{ServeDir, ServeFile};
#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    if let Err(e) = run().await {
        tracing::error!("{e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let cfg = config::Config::load()?;
    std::fs::create_dir_all(&cfg.data_dir)
        .map_err(|e| format!("create {}: {e}", cfg.data_dir.display()))?;

    let store = store::Store::load(&cfg.data_dir)?;
    let history = history::History::open(&cfg.data_dir.join("history.db"))?;
    let state = AppState::new(store, cfg.data_dir.clone(), cfg.loopback_http, history);
    // Jobs live in memory, so a restart drops them. Recover the transfers whose
    // bodies arrived in full (they can be pushed without re-uploading) and
    // delete anything incomplete, which could never be resumed safely.
    api::uploads::restore(&state);
    tokio::spawn(history::collect(state.clone()));

    let index = cfg.dist_dir.join("index.html");
    let app = api::build(state.clone())
        .fallback_service(ServeDir::new(&cfg.dist_dir).fallback(ServeFile::new(index)))
        .with_state(state);

    let addr = cfg.addr()?;
    tracing::info!("virt-man-web listening on {addr}");

    if cfg.loopback_http {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| format!("bind {addr}: {e}"))?;
        axum::serve(listener, app)
            .await
            .map_err(|e| format!("server: {e}"))
    } else {
        let (cert, key) = (cfg.cert_path(), cfg.key_path());
        tls::ensure_cert(&cert, &key)?;
        let tls_config = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key)
            .await
            .map_err(|e| format!("failed to load TLS certificate: {e}"))?;
        // Plaintext HTTP on this port gets a 308 to https:// instead of a raw
        // TLS alert record, so http://host:8006 works in a browser.
        let acceptor = httpsredirect::RedirectAcceptor::new(
            axum_server::tls_rustls::RustlsAcceptor::new(tls_config),
            cfg.port,
        );
        axum_server::bind(addr)
            .acceptor(acceptor)
            .serve(app.into_make_service())
            .await
            .map_err(|e| format!("server: {e}"))
    }
}
