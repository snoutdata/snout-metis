//! The agents' listener over TLS (`agent_tls`): the fleet's
//! agents reach their members over the internet, so their tokens never cross it in the clear. Its
//! router holds the agents' routes and nothing else (`server.rs`). A handshake that does not finish
//! in ten seconds is dropped.

use axum::Router;
use rustls::ServerConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

const HANDSHAKE: Duration = Duration::from_secs(10);

pub fn server_config(cert: &Path, key: &Path) -> Result<Arc<ServerConfig>, String> {
	let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
		.map_err(|e| format!("{}: {e}", cert.display()))?
		.collect::<Result<_, _>>()
		.map_err(|e| format!("{}: {e}", cert.display()))?;
	if certs.is_empty() {
		return Err(format!("{}: no certificate in the file", cert.display()));
	}
	let key = PrivateKeyDer::from_pem_file(key).map_err(|e| format!("{}: {e}", key.display()))?;
	let mut config =
		ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
			.with_safe_default_protocol_versions()
			.map_err(|e| e.to_string())?
			.with_no_client_auth()
			.with_single_cert(certs, key)
			.map_err(|e| e.to_string())?;
	config.alpn_protocols = vec![b"http/1.1".to_vec()];
	Ok(Arc::new(config))
}

/// Serves `app` on `listener` over TLS until the task is aborted.
pub async fn serve(listener: TcpListener, config: Arc<ServerConfig>, app: Router) {
	let acceptor = TlsAcceptor::from(config);
	loop {
		let Ok((tcp, _)) = listener.accept().await else {
			continue;
		};
		let (acceptor, app) = (acceptor.clone(), app.clone());
		tokio::spawn(async move {
			let _ = tcp.set_nodelay(true);
			let Ok(Ok(stream)) = tokio::time::timeout(HANDSHAKE, acceptor.accept(tcp)).await else {
				return;
			};
			let service = hyper_util::service::TowerToHyperService::new(app);
			let _ = hyper::server::conn::http1::Builder::new()
				.serve_connection(hyper_util::rt::TokioIo::new(stream), service)
				.await;
		});
	}
}
