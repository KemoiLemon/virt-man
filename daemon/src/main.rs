mod api;
mod auth;
mod config;
mod disks;
mod firewall;
mod ipam;
mod libvirt;
mod migrate;
mod rfb;
mod titles;
mod tls;

use std::os::unix::fs::PermissionsExt;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    if let Err(e) = run().await {
        tracing::error!("{e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    // reqwest (URL fetches) is built without its own TLS provider; install
    // the aws-lc-rs one axum-server already uses, or building a client panics.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    config::ensure_privileges()?;

    let cfg = config::Config::load()?;
    let auth = auth::Auth::from_file(&cfg.token_path)?;

    // Re-apply any persisted firewall rules at startup: nftables is not
    // persistent across reboot, so the daemon restores the virt-man table.
    let fw_path = std::path::PathBuf::from(firewall::FIREWALL_PATH);
    if fw_path.exists()
        && let Ok(store) = firewall::load(&fw_path)
        && let Err(e) = api::firewall::apply_store(store).await
    {
        tracing::warn!("failed to re-apply firewall rules: {e}");
    }

    // TLS: configured cert/key or generated self-signed.
    let (cert, key) = cfg.tls_cert_key();
    if cfg.tls.cert.is_none() || cfg.tls.key.is_none() {
        tls::ensure_cert(&cert, &key)?;
    }
    let tls_config = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key)
        .await
        .map_err(|e| format!("failed to load TLS certificate: {e}"))?;

    // IPAM: DHCP and DNS for guest networks, in place of libvirt's dnsmasq.
    let ipam = ipam::server::Ipam::load(ipam::store::IPAM_PATH.into())?;
    tokio::spawn(ipam::server::supervise(ipam.clone()));
    // Rename guests, networks and pools to their UUIDs, now and as they
    // become free to rename (a guest once it is shut off). Off until the web
    // server addresses everything by UUID: renaming first would leave its
    // name-keyed links, grants and ownership pointing at nothing.
    // ponytail: gated by env var until the web side ships; remove the gate then.
    if std::env::var("VIRT_MAN_MIGRATE_UUIDS").is_ok_and(|v| v == "1") {
        tokio::spawn(migrate::run(ipam.clone()));
    }
    // Access log: guest connections from the kernel's connection tracker.
    // Live packet captures stop themselves when nobody is watching.
    tokio::spawn(api::capture::reaper());
    ipam::flows::enable_accounting();
    {
        let owners = ipam.clone();
        ipam::flows::listen(ipam.flows.clone(), move |ip| owners.owner(ip));
    }
    let app = api::build(auth, ipam);

    // Unix socket listener.
    let sock_path = cfg.unix_socket_path();
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    let _ = std::fs::remove_file(&sock_path);
    let uds = tokio::net::UnixListener::bind(&sock_path)
        .map_err(|e| format!("failed to bind unix socket {}: {e}", sock_path.display()))?;
    // World-accessible socket; the token is the actual gate.
    let _ = std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o666));
    // tokio::bind sets O_NONBLOCK; converting back to std keeps it, which
    // axum_server::from_unix requires when re-registering with tokio.
    let uds = uds
        .into_std()
        .map_err(|e| format!("unix listener conversion failed: {e}"))?;
    tracing::info!("unix socket listening on {}", sock_path.display());

    // TLS HTTP listener.
    let http_addr = cfg.http_socket_addr()?;
    tracing::info!("https listening on {http_addr}");

    let tcp_handle = axum_server::Handle::new();
    let unix_handle = axum_server::Handle::new();

    let (tcp_sig, unix_sig) = (tcp_handle.clone(), unix_handle.clone());
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutting down");
        tcp_sig.graceful_shutdown(None);
        unix_sig.graceful_shutdown(None);
    });

    tokio::try_join!(
        axum_server::bind_rustls(http_addr, tls_config)
            .handle(tcp_handle)
            .serve(app.clone().into_make_service()),
        axum_server::from_unix(uds)
            .map_err(|e| format!("unix socket error: {e}"))?
            .handle(unix_handle)
            .serve(app.into_make_service()),
    )
    .map_err(|e| format!("server error: {e}"))?;

    Ok(())
}
