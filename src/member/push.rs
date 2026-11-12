//! The table, pushed to the routers. A router never asks a member for anything: every member sends it the table from its own copy of the last
//! decision when the table changes, so a lost leader stops nothing (MT2), and a router keeps the last
//! one on disk and goes on routing with every member down.
//!
//! Who gets it is a file (`routers_file`) that the operator's tooling writes when it
//! launches or retires a router; it is read again before each round,
//! so a router comes and goes with no restart. Each router is reached over TLS at its public address
//! with the certificate it made at boot, PINNED by its sha256 (no CA, no name), and is sent its
//! region's token (`router_tokens`). A router admits the members' three addresses and nothing else
//! (its firewall).
//!
//! A router is sent the table when it changes (anything but `decided_at_ms`), when it is new, after a
//! push it did not take, and once a minute besides, so a router that was down catches up without
//! waiting for a change. Three members send it the same table: a router keeps the one with the latest
//! `decided_at_ms` and answers the others that it has it.
//!
//! With `routes_from`, the table also carries what a router admits and routes a call by (`access`:
//! every project's route and custom domain, Lepis's gap 2), which each member reads from the control
//! plane every `ACCESS_EVERY_MS` with its own reader token; a router keeps the newest by its `at_ms`
//! and never asks the control plane where a project is.
//!
//! What changed, not every route each time: each `access` carries a `digest` of its content
//! (`access_digest`), and a router answers a push with the digest it now holds. When this member still
//! has that content among its last reads, it sends `access_delta` instead (the routes and domains
//! changed or added since, and the ones gone, `access_delta`), which the router applies only onto
//! exactly that digest; any other base is a 409, and the whole `access` goes at once. The whole one
//! goes to a new router, after a miss, to a router that names no digest (an older build), and every
//! `FULL_EVERY_MS` besides.

use super::config::MemberConfig;
use super::server::Member;
use super::types::Decision;
use rustls::DigitallySignedStruct;
use rustls::SignatureScheme;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use serde::Deserialize;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// How often a member looks at its table and its routers.
pub const CHECK_MS: u64 = 2000;
/// A router that has the table is sent it again this often all the same.
pub const AGAIN_MS: u64 = 60_000;
/// A router that did not take a push is tried again no sooner than this.
pub const RETRY_MS: u64 = 10_000;
const TIMEOUT: Duration = Duration::from_secs(5);
/// How often a member reads the routes it pushes (`routes_from`).
pub const ACCESS_EVERY_MS: u64 = 5000;
/// A router is sent the whole `access`, never a delta, at least this often.
pub const FULL_EVERY_MS: u64 = 600_000;
/// How many of its last distinct reads a member keeps, to send a router what changed since its own.
pub const KEEP_READS: usize = 8;

/// One router a member pushes to, as `router-add` wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RouterPeer {
	/// `sr-<8>`.
	pub name: String,
	/// Whose token it is sent (`router_tokens`).
	pub region: String,
	/// `https://<public address>:7444`.
	pub url: String,
	/// The sha256 of its certificate (DER), lowercase hex.
	pub cert_sha256: String,
}

#[derive(Debug, Default, Deserialize)]
struct RoutersFile {
	#[serde(default)]
	routers: Vec<RouterPeer>,
}

/// The routers in `path`, checked: a file that does not pass is not used at all.
pub fn read_routers(path: &Path) -> Result<Vec<RouterPeer>, String> {
	let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
	let file: RoutersFile =
		serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
	check_routers(&file.routers)?;
	Ok(file.routers)
}

pub fn check_routers(routers: &[RouterPeer]) -> Result<(), String> {
	let mut names = BTreeSet::new();
	for router in routers {
		let named = router.name.len() == 11
			&& router.name.starts_with("sr-")
			&& router.name[3..]
				.bytes()
				.all(|b| b.is_ascii_digit() || b.is_ascii_lowercase());
		if !named {
			return Err(format!("{}: not a router's name", router.name));
		}
		if !names.insert(router.name.as_str()) {
			return Err(format!("router {} is listed twice", router.name));
		}
		if !router.url.starts_with("https://") {
			return Err(format!(
				"router {}: {} is not https",
				router.name, router.url
			));
		}
		if pin(&router.cert_sha256).is_none() {
			return Err(format!(
				"router {}: its certificate is not a sha256",
				router.name
			));
		}
		if router.region.is_empty() {
			return Err(format!("router {}: no region", router.name));
		}
	}
	Ok(())
}

fn pin(hex: &str) -> Option<Vec<u8>> {
	if hex.len() != 64
		|| !hex
			.bytes()
			.all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
	{
		return None;
	}
	(0..32)
		.map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok())
		.collect()
}

/// The table as every reader is given it, a member's `/table` and a router's push alike.
pub fn table_body(config: &MemberConfig, decision: &Decision) -> serde_json::Value {
	// Where each host's proxy answers, so a proxy can forward a call to a copy on another host
	// (MT4): its WireGuard address, from the member's `about`.
	let hosts: BTreeMap<&str, &str> = config
		.about
		.iter()
		.map(|h| (h.host.as_str(), h.wg.as_str()))
		.collect();
	// Each project's primary copy and where its host's proxy answers, so a door holding a standby
	// sends the call there.
	let projects: BTreeMap<&str, serde_json::Value> = decision
		.projects
		.iter()
		.map(|(project, p)| {
			let mut entry = json!({"primary": p.host, "epoch": p.epoch});
			if let Some(wg) = hosts.get(p.host.as_str()) {
				entry["wg"] = json!(wg);
			}
			(project.as_str(), entry)
		})
		.collect();
	json!({
		"decided_at_ms": decision.at_ms,
		"table": decision.outcome.table,
		"hosts": hosts,
		"projects": projects,
		// The hosts the decision heard: a door on the fleet sends a spread project's calls to a copy
		// only when its home is not among them.
		"heard": decision.outcome.heard,
	})
}

/// What a push carries besides its times: when this differs, the table or the routes changed. A
/// read of the routes that found nothing new is not a change.
pub fn content(body: &serde_json::Value) -> String {
	let mut body = body.clone();
	if let Some(fields) = body.as_object_mut() {
		fields.remove("decided_at_ms");
		if let Some(access) = fields.get_mut("access").and_then(|a| a.as_object_mut()) {
			access.remove("at_ms");
		}
	}
	body.to_string()
}

/// The control plane's routes, if the answer is them: `at_ms` and a `routes` object.
pub fn access_answer(body: serde_json::Value) -> Option<serde_json::Value> {
	let ok = body["at_ms"].as_u64().is_some_and(|at| at > 0) && body["routes"].is_object();
	ok.then_some(body)
}

/// The digest of what a read of the routes says, its time aside: equal for equal content, whichever
/// member read it and when.
pub fn access_digest(access: &serde_json::Value) -> String {
	let content = json!({"routes": access["routes"], "domains": access["domains"]});
	let digest = ring::digest::digest(&ring::digest::SHA256, content.to_string().as_bytes());
	digest.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// What changed from `base` (whose digest is `base_digest`) to `now` (which carries its own `digest`):
/// the routes and domains changed or added, and the keys gone.
pub fn access_delta(
	base: &serde_json::Value,
	base_digest: &str,
	now: &serde_json::Value,
) -> serde_json::Value {
	let changed = |field: &str| {
		let empty = serde_json::Map::new();
		let was = base[field].as_object().unwrap_or(&empty);
		let is = now[field].as_object().unwrap_or(&empty);
		let set: serde_json::Map<String, serde_json::Value> = is
			.iter()
			.filter(|(k, v)| was.get(k.as_str()) != Some(v))
			.map(|(k, v)| (k.clone(), v.clone()))
			.collect();
		let gone: Vec<&String> = was
			.keys()
			.filter(|k| !is.contains_key(k.as_str()))
			.collect();
		(json!(set), json!(gone))
	};
	let (routes, removed) = changed("routes");
	let (domains, removed_domains) = changed("domains");
	json!({
		"base_digest": base_digest,
		"digest": now["digest"],
		"at_ms": now["at_ms"],
		"routes": routes,
		"removed": removed,
		"domains": domains,
		"removed_domains": removed_domains,
	})
}

/// What a router is sent: `full` as it is, or with `access_delta` in place of `access` when the router
/// holds a digest (`holds`) that is among this member's last reads and a whole one is not due.
pub fn push_body(
	full: &serde_json::Value,
	holds: Option<&str>,
	full_due: bool,
	reads: &VecDeque<(String, serde_json::Value)>,
) -> serde_json::Value {
	let (Some(access), Some(held)) = (full.get("access"), holds) else {
		return full.clone();
	};
	if full_due {
		return full.clone();
	}
	let Some((_, base)) = reads.iter().find(|(digest, _)| digest == held) else {
		return full.clone();
	};
	let mut body = full.clone();
	let delta = access_delta(base, held, access);
	if let Some(fields) = body.as_object_mut() {
		fields.remove("access");
		fields.insert("access_delta".into(), delta);
	}
	body
}

/// Reads the routes every `ACCESS_EVERY_MS` into `latest`, keeping the last good one; a read that
/// fails is said once, and once more when one succeeds again.
async fn read_access(
	member: Member,
	source: super::config::RoutesSource,
	latest: Arc<tokio::sync::Mutex<Option<serde_json::Value>>>,
) {
	let Ok(client) = reqwest::Client::builder()
		.timeout(Duration::from_secs(10))
		.build()
	else {
		member
			.events
			.note("router", "no HTTP client: the routes are never read");
		return;
	};
	let mut failing: Option<String> = None;
	loop {
		if member.stopping.load(Ordering::SeqCst) {
			return;
		}
		let read = async {
			let response = client
				.get(&source.url)
				.bearer_auth(&source.token)
				.send()
				.await
				.map_err(|e| e.to_string())?;
			if !response.status().is_success() {
				return Err(format!("{}", response.status()));
			}
			let body: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;
			access_answer(body).ok_or_else(|| "not the routes".to_string())
		}
		.await;
		match read {
			Ok(body) => {
				if failing.take().is_some() {
					member.events.note("router", "the routes are read again");
				}
				*latest.lock().await = Some(body);
			}
			Err(e) => {
				if failing.as_deref() != Some(e.as_str()) {
					member.events.note(
						"router",
						format!("the routes could not be read ({e}); the last ones are pushed"),
					);
				}
				failing = Some(e);
			}
		}
		tokio::time::sleep(Duration::from_millis(ACCESS_EVERY_MS)).await;
	}
}

/// What was last sent a router, and whether it took it.
#[derive(Debug, Clone)]
pub struct Sent {
	pub content: String,
	pub at: Instant,
	pub taken: bool,
	/// The digest of the access the router said it holds, when it says (`access_digest`).
	pub holds: Option<String>,
	/// When it was last sent the whole access.
	pub full_at: Option<Instant>,
}

/// How a router answered a push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
	/// Taken (or it had it), holding the access with this digest when it says.
	Taken(Option<String>),
	/// A delta onto a digest it does not hold: the whole access, now.
	NeedFull,
}

/// Whether a router is due the table now.
pub fn due(sent: Option<&Sent>, content: &str, now: Instant) -> bool {
	match sent {
		None => true,
		Some(s) if !s.taken => now.duration_since(s.at) >= Duration::from_millis(RETRY_MS),
		Some(s) => {
			s.content != content || now.duration_since(s.at) >= Duration::from_millis(AGAIN_MS)
		}
	}
}

pub async fn run(member: Member) {
	let Some(path) = member.config.routers_file.clone() else {
		return;
	};
	let access: Arc<tokio::sync::Mutex<Option<serde_json::Value>>> = Arc::default();
	if let Some(source) = member.config.routes_from.clone() {
		tokio::spawn(read_access(member.clone(), source, access.clone()));
	}
	let mut clients: HashMap<String, reqwest::Client> = HashMap::new();
	let mut sent: HashMap<String, Sent> = HashMap::new();
	let mut reads: VecDeque<(String, serde_json::Value)> = VecDeque::new();
	let mut said = String::new();
	loop {
		tokio::time::sleep(Duration::from_millis(CHECK_MS)).await;
		if member.stopping.load(Ordering::SeqCst) {
			return;
		}
		let routers = match read_routers(&path) {
			Ok(routers) => routers,
			Err(e) => {
				// No file yet is no router yet; a file that does not pass is said once.
				if e != said && path.exists() {
					member
						.events
						.note("router", format!("the routers' file is not used: {e}"));
				}
				said = e;
				continue;
			}
		};
		if !said.is_empty() {
			member.events.note(
				"router",
				format!("{} routers in the routers' file", routers.len()),
			);
			said.clear();
		}
		sent.retain(|name, _| routers.iter().any(|r| &r.name == name));
		clients.retain(|key, _| routers.iter().any(|r| &r.cert_sha256 == key));
		let Some(decision) = member.state.state().await.decision else {
			continue;
		};
		let mut body = table_body(&member.config, &decision);
		if let Some(mut routes) = access.lock().await.clone() {
			let digest = access_digest(&routes);
			if reads.front().map(|(d, _)| d) != Some(&digest) {
				reads.retain(|(d, _)| d != &digest);
				reads.push_front((digest.clone(), routes.clone()));
				reads.truncate(KEEP_READS);
			}
			routes["digest"] = json!(digest);
			body["access"] = routes;
		}
		let now_content = content(&body);
		for router in &routers {
			let previous = sent.get(&router.name);
			if !due(previous, &now_content, Instant::now()) {
				continue;
			}
			let was_taken = previous.map(|s| s.taken);
			let holds = previous.filter(|s| s.taken).and_then(|s| s.holds.clone());
			let full_due = previous.and_then(|s| s.full_at).is_none_or(|at| {
				Instant::now().duration_since(at) >= Duration::from_millis(FULL_EVERY_MS)
			});
			let pushed = push_body(&body, holds.as_deref(), full_due, &reads);
			let delta = pushed.get("access_delta").is_some();
			let mut full_at = previous.and_then(|s| s.full_at);
			let outcome = match member.config.router_tokens.get(&router.region) {
				None => Err(format!("no token for {}", router.region)),
				Some(token) => {
					let client = match clients.get(&router.cert_sha256) {
						Some(client) => Ok(client.clone()),
						None => pinned_client(&router.cert_sha256).inspect(|client| {
							clients.insert(router.cert_sha256.clone(), client.clone());
						}),
					};
					match client {
						Ok(client) => match send(&client, router, token, &pushed).await {
							// Its base was not the router's: the whole access, at once.
							Ok(Answer::NeedFull) if delta => send(&client, router, token, &body)
								.await
								.and_then(|answer| match answer {
									Answer::Taken(holds) => {
										full_at = Some(Instant::now());
										Ok(holds)
									}
									Answer::NeedFull => Err("it refused the whole table".into()),
								}),
							Ok(Answer::NeedFull) => Err("it refused the whole table".into()),
							Ok(Answer::Taken(holds)) => {
								if !delta {
									full_at = Some(Instant::now());
								}
								Ok(holds)
							}
							Err(e) => Err(e),
						},
						Err(e) => Err(e),
					}
				}
			};
			let taken = outcome.is_ok();
			// Said when it changes, not on every round.
			if was_taken != Some(taken) {
				let text = match &outcome {
					Ok(_) => format!("{} has the table", router.name),
					Err(e) => format!("{} did not take the table: {e}", router.name),
				};
				member.events.note("router", text);
			}
			sent.insert(
				router.name.clone(),
				Sent {
					content: now_content.clone(),
					at: Instant::now(),
					taken,
					holds: outcome.ok().flatten(),
					full_at,
				},
			);
		}
	}
}

async fn send(
	client: &reqwest::Client,
	router: &RouterPeer,
	token: &str,
	body: &serde_json::Value,
) -> Result<Answer, String> {
	let response = client
		.post(format!("{}/table", router.url.trim_end_matches('/')))
		.bearer_auth(token)
		.json(body)
		.timeout(TIMEOUT)
		.send()
		.await
		.map_err(|e| e.to_string())?;
	let status = response.status();
	if status == reqwest::StatusCode::CONFLICT {
		return Ok(Answer::NeedFull);
	}
	if !status.is_success() {
		return Err(format!("it answered {status}"));
	}
	// An older router answers 204 and names no digest: it is sent the whole access each time.
	let said: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);
	Ok(Answer::Taken(
		said["access_digest"].as_str().map(str::to_string),
	))
}

/// A client that trusts exactly one certificate, by its sha256, whatever it names.
pub fn pinned_client(cert_sha256: &str) -> Result<reqwest::Client, String> {
	let sha256 = pin(cert_sha256).ok_or("not a sha256")?;
	let provider = Arc::new(rustls::crypto::ring::default_provider());
	let mut config = rustls::ClientConfig::builder_with_provider(provider.clone())
		.with_safe_default_protocol_versions()
		.map_err(|e| e.to_string())?
		.dangerous()
		.with_custom_certificate_verifier(Arc::new(Pinned { sha256, provider }))
		.with_no_client_auth();
	config.alpn_protocols = vec![b"http/1.1".to_vec()];
	reqwest::Client::builder()
		.use_preconfigured_tls(config)
		.connect_timeout(TIMEOUT)
		.build()
		.map_err(|e| e.to_string())
}

#[derive(Debug)]
struct Pinned {
	sha256: Vec<u8>,
	provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for Pinned {
	fn verify_server_cert(
		&self,
		end_entity: &CertificateDer<'_>,
		_intermediates: &[CertificateDer<'_>],
		_server_name: &ServerName<'_>,
		_ocsp_response: &[u8],
		_now: UnixTime,
	) -> Result<ServerCertVerified, rustls::Error> {
		let digest = ring::digest::digest(&ring::digest::SHA256, end_entity.as_ref());
		if digest.as_ref() == self.sha256.as_slice() {
			Ok(ServerCertVerified::assertion())
		} else {
			Err(rustls::Error::General(
				"not the router's pinned certificate".into(),
			))
		}
	}

	fn verify_tls12_signature(
		&self,
		message: &[u8],
		cert: &CertificateDer<'_>,
		dss: &DigitallySignedStruct,
	) -> Result<HandshakeSignatureValid, rustls::Error> {
		rustls::crypto::verify_tls12_signature(
			message,
			cert,
			dss,
			&self.provider.signature_verification_algorithms,
		)
	}

	fn verify_tls13_signature(
		&self,
		message: &[u8],
		cert: &CertificateDer<'_>,
		dss: &DigitallySignedStruct,
	) -> Result<HandshakeSignatureValid, rustls::Error> {
		rustls::crypto::verify_tls13_signature(
			message,
			cert,
			dss,
			&self.provider.signature_verification_algorithms,
		)
	}

	fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
		self.provider
			.signature_verification_algorithms
			.supported_schemes()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn peer(name: &str) -> RouterPeer {
		RouterPeer {
			name: name.into(),
			region: "region-c".into(),
			url: "https://203.0.113.7:7444".into(),
			cert_sha256: "ab".repeat(32),
		}
	}

	#[test]
	fn the_routers_file_is_checked_whole() {
		assert!(check_routers(&[peer("sr-aaaaaaaa"), peer("sr-bbbbbbbb")]).is_ok());
		assert!(
			check_routers(&[peer("sr-aaaaaaaa"), peer("sr-aaaaaaaa")]).is_err(),
			"twice"
		);
		assert!(
			check_routers(&[peer("pod-aaaaaaaa")]).is_err(),
			"a host, not a router"
		);
		let plain = RouterPeer {
			url: "http://203.0.113.7:7444".into(),
			..peer("sr-aaaaaaaa")
		};
		assert!(check_routers(&[plain]).is_err(), "never in the clear");
		let unpinned = RouterPeer {
			cert_sha256: "AB".repeat(32),
			..peer("sr-aaaaaaaa")
		};
		assert!(check_routers(&[unpinned]).is_err(), "lowercase hex");
		let nowhere = RouterPeer {
			region: String::new(),
			..peer("sr-aaaaaaaa")
		};
		assert!(check_routers(&[nowhere]).is_err(), "no region, no token");
	}

	#[test]
	fn a_router_is_sent_the_table_when_it_changed_when_new_after_a_miss_and_each_minute() {
		let start = Instant::now();
		let later = |ms: u64| start + Duration::from_millis(ms);
		assert!(due(None, "t1", start), "new");
		let took = Sent {
			content: "t1".into(),
			at: start,
			taken: true,
			holds: None,
			full_at: None,
		};
		assert!(!due(Some(&took), "t1", later(CHECK_MS)), "unchanged");
		assert!(due(Some(&took), "t2", later(CHECK_MS)), "changed");
		assert!(
			due(Some(&took), "t1", later(AGAIN_MS)),
			"once a minute besides"
		);
		let missed = Sent {
			taken: false,
			..took
		};
		assert!(!due(Some(&missed), "t1", later(CHECK_MS)), "not hammered");
		assert!(due(Some(&missed), "t1", later(RETRY_MS)), "tried again");
	}

	#[test]
	fn only_the_time_is_not_a_change() {
		let a = json!({"decided_at_ms": 1, "table": {"p": []}, "heard": ["pod-a"]});
		let b = json!({"decided_at_ms": 2, "table": {"p": []}, "heard": ["pod-a"]});
		let c = json!({"decided_at_ms": 2, "table": {"p": []}, "heard": ["pod-a", "pod-b"]});
		assert_eq!(content(&a), content(&b));
		assert_ne!(content(&b), content(&c));
	}

	#[test]
	fn a_routes_read_that_found_nothing_new_is_not_a_change_and_a_new_route_is() {
		let at = |at: u64, routes: serde_json::Value| json!({"decided_at_ms": 1, "table": {}, "heard": [], "access": {"at_ms": at, "routes": routes, "domains": {}}});
		let a = at(10, json!({"abcdefghjkmnp": {"state": "ready"}}));
		let b = at(20, json!({"abcdefghjkmnp": {"state": "ready"}}));
		let c = at(30, json!({"abcdefghjkmnp": {"state": "paused"}}));
		assert_eq!(content(&a), content(&b));
		assert_ne!(content(&b), content(&c));
	}

	#[test]
	fn equal_routes_have_one_digest_whenever_and_by_whomever_read() {
		let a =
			json!({"at_ms": 10, "routes": {"abcdefghjkmnp": {"state": "ready"}}, "domains": {}});
		let b =
			json!({"at_ms": 99, "routes": {"abcdefghjkmnp": {"state": "ready"}}, "domains": {}});
		let c =
			json!({"at_ms": 99, "routes": {"abcdefghjkmnp": {"state": "paused"}}, "domains": {}});
		assert_eq!(access_digest(&a), access_digest(&b));
		assert_ne!(access_digest(&a), access_digest(&c));
		assert_eq!(access_digest(&a).len(), 64);
	}

	#[test]
	fn a_delta_names_what_changed_was_added_and_went() {
		let base = json!({"at_ms": 10, "routes": {"aaaaaaaaaaaaa": {"state": "ready"}, "bbbbbbbbbbbbb": {"state": "ready"}, "ccccccccccccc": {"state": "ready"}}, "domains": {"a.example": {"ref": "aaaaaaaaaaaaa"}}});
		let now = json!({"at_ms": 20, "digest": "d2", "routes": {"aaaaaaaaaaaaa": {"state": "ready"}, "bbbbbbbbbbbbb": {"state": "paused"}, "ddddddddddddd": {"state": "ready"}}, "domains": {}});
		let delta = access_delta(&base, "d1", &now);
		assert_eq!(delta["base_digest"], "d1");
		assert_eq!(delta["digest"], "d2");
		assert_eq!(delta["at_ms"], 20);
		assert_eq!(
			delta["routes"],
			json!({"bbbbbbbbbbbbb": {"state": "paused"}, "ddddddddddddd": {"state": "ready"}}),
			"an unchanged route is not sent"
		);
		assert_eq!(delta["removed"], json!(["ccccccccccccc"]));
		assert_eq!(delta["domains"], json!({}));
		assert_eq!(delta["removed_domains"], json!(["a.example"]));
	}

	#[test]
	fn a_router_is_sent_a_delta_only_onto_a_read_this_member_has_and_the_whole_access_otherwise() {
		let base =
			json!({"at_ms": 10, "routes": {"aaaaaaaaaaaaa": {"state": "ready"}}, "domains": {}});
		let d1 = access_digest(&base);
		let mut reads = VecDeque::new();
		reads.push_front((d1.clone(), base));
		let mut access =
			json!({"at_ms": 20, "routes": {"aaaaaaaaaaaaa": {"state": "paused"}}, "domains": {}});
		access["digest"] = json!(access_digest(&access));
		let full = json!({"decided_at_ms": 1, "table": {}, "heard": [], "access": access});
		let delta = push_body(&full, Some(&d1), false, &reads);
		assert!(delta.get("access").is_none());
		assert_eq!(delta["access_delta"]["base_digest"], json!(d1));
		assert_eq!(delta["table"], json!({}), "the table goes whole");
		assert_eq!(
			push_body(&full, None, false, &reads),
			full,
			"a router naming no digest"
		);
		assert_eq!(
			push_body(&full, Some("unknown"), false, &reads),
			full,
			"a digest this member never read"
		);
		assert_eq!(
			push_body(&full, Some(&d1), true, &reads),
			full,
			"a whole one due"
		);
		let bare = json!({"decided_at_ms": 1, "table": {}, "heard": []});
		assert_eq!(
			push_body(&bare, Some(&d1), false, &reads),
			bare,
			"no access, nothing to delta"
		);
	}

	#[test]
	fn only_the_routes_are_taken_as_the_routes() {
		assert!(access_answer(json!({"at_ms": 5, "routes": {}, "domains": {}})).is_some());
		assert!(access_answer(json!({"at_ms": 0, "routes": {}})).is_none());
		assert!(access_answer(json!({"error": "Unknown Metis member token"})).is_none());
		assert!(access_answer(json!({"at_ms": 5, "routes": []})).is_none());
	}
}
