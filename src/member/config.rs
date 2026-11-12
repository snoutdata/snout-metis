//! A member's configuration: a file on its host, never in the repo, since it holds the tokens.

use super::types::NodeId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberConfig {
	/// This member's id, a key of `members`.
	pub id: NodeId,
	/// Where it listens, `10.0.0.1:7480` (its WireGuard address, never a public one).
	pub listen: String,
	pub data_dir: PathBuf,
	/// Every member by id, as the others reach it: `http://10.0.0.1:7480`.
	pub members: BTreeMap<NodeId, String>,
	/// What members present to each other.
	pub member_token: String,
	/// What an operator presents (`metis apply`, `status`, `log`).
	pub operator_token: String,
	/// What a watcher presents to read `status` and `log` and nothing else (a console's
	/// feed). Optional: without it only the operator reads them.
	#[serde(default)]
	pub observer_token: Option<String>,
	/// Where each host is, for a watcher's picture: its cloud, region and addresses. Information
	/// only; nothing decides from it.
	#[serde(default)]
	pub about: Vec<HostAbout>,
	/// What each host's agent presents, by host name: an agent reports for, and reads the desired
	/// state of, its own host only.
	pub agent_tokens: BTreeMap<String, String>,
	/// The routers this member pushes the table to: a file of
	/// `{"routers": [{name, region, url, cert_sha256}]}` that the operator's tooling writes as
	/// routers come and go, read again before each push, so a router comes and goes with no
	/// restart. A router never asks a member for anything (`push.rs`).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub routers_file: Option<PathBuf>,
	/// What this member presents to a region's routers when it pushes, by region.
	#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
	pub router_tokens: BTreeMap<String, String>,
	/// Where this member reads what a router admits and routes by (the control plane's
	/// `cloud-metis-router-routes`), to push it with the table as `access` (`push.rs`). None: the
	/// table goes without it, and a router asks the control plane as before.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub routes_from: Option<RoutesSource>,
	/// How often the leader decides.
	#[serde(default = "default_tick_ms")]
	pub tick_ms: u64,
	/// Raft's heartbeat and election timeouts. The members can be across regions (tens of
	/// milliseconds apart), so the defaults leave room for that.
	#[serde(default = "default_heartbeat_ms")]
	pub heartbeat_ms: u64,
	#[serde(default = "default_election_min_ms")]
	pub election_min_ms: u64,
	#[serde(default = "default_election_max_ms")]
	pub election_max_ms: u64,
	/// Where the database primary is, so the table prefers copies near it (MT4), when the agents have
	/// not said: their reports of the guarded copies' roles win, and the last primary they gave is
	/// kept through a failover (`database.rs`). Only with no word at all is this the region.
	#[serde(default)]
	pub database_primary_region: Option<String>,
	/// Where the leader reads the spec, for a cluster whose spec is not applied by an operator: the
	/// fleet's members read `cloud_metis_fleet_spec()` through the Snout Function
	/// `cloud-metis-fleet-spec` with a token of their own.
	/// None, the control plane's members: an operator applies it (`/spec`).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub spec_from: Option<SpecSource>,
	/// A second listener for the agents alone, over TLS, on an address they can reach: the fleet's
	/// agents reach their members over the internet, never the members' private network. It serves
	/// `/report`, `/desired` and `/table` and nothing else; members and operators stay on `listen`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub agent_tls: Option<AgentTls>,
	/// Pass each agent report on to the other members. The fleet's agents reach only the members
	/// with a public address (not `home-2`), and a leader that hears no host would take every
	/// host for lost; relayed, every member hears every host.
	#[serde(default, skip_serializing_if = "std::ops::Not::not")]
	pub relay_reports: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTls {
	/// `0.0.0.0:7443`.
	pub listen: String,
	/// PEM files: the certificate chain (the members' own CA's leaf, naming the public address) and
	/// its key.
	pub cert: PathBuf,
	pub key: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecSource {
	/// `https://<functions origin>/functions/v1/cloud-metis-fleet-spec`.
	pub url: String,
	/// This member's own token for it (`cloud_metis_reader_issue`), never a cloud credential.
	pub token: String,
	/// How often the leader reads it.
	#[serde(default = "default_spec_every_secs")]
	pub every_secs: u64,
	/// Where the leader posts each decision's placement, with the same token: the control plane's
	/// `cloud-metis-fleet-placement`, which keeps it so each copy host is told its copies and
	/// given their spec. None: the placement stays with the members.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub placement_url: Option<String>,
}

/// The control plane's routes for the routers, read by every member with its own reader token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutesSource {
	/// `https://<functions origin>/functions/v1/cloud-metis-router-routes`.
	pub url: String,
	/// This member's reader token (`cloud_metis_reader_issue`), never a cloud credential.
	pub token: String,
}

fn default_spec_every_secs() -> u64 {
	30
}

/// One host as a watcher draws it. `member` is its id when it votes, none for a peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostAbout {
	pub host: String,
	#[serde(default)]
	pub member: Option<NodeId>,
	pub cloud: String,
	pub region: String,
	pub wg: String,
	#[serde(default)]
	pub public: Option<String>,
	#[serde(default)]
	pub private: Option<String>,
}

fn default_tick_ms() -> u64 {
	5000
}

fn default_heartbeat_ms() -> u64 {
	500
}

fn default_election_min_ms() -> u64 {
	2000
}

fn default_election_max_ms() -> u64 {
	4000
}

impl MemberConfig {
	pub fn check(&self) -> Result<(), String> {
		if !self.members.contains_key(&self.id) {
			return Err(format!("member {} is not in its own members list", self.id));
		}
		if self.members.len().is_multiple_of(2) {
			return Err(format!(
				"{} members: an even number cannot break a tie, use an odd one (MT8)",
				self.members.len()
			));
		}
		for (name, token) in [
			("member_token", &self.member_token),
			("operator_token", &self.operator_token),
		] {
			if token.len() < 32 {
				return Err(format!("{name} is shorter than 32 characters"));
			}
		}
		if self.observer_token.as_ref().is_some_and(|t| t.len() < 32) {
			return Err("observer_token is shorter than 32 characters".into());
		}
		if self.agent_tokens.values().any(|t| t.len() < 32) {
			return Err("an agent token is shorter than 32 characters".into());
		}
		if self.router_tokens.values().any(|t| t.len() < 32) {
			return Err("a router token is shorter than 32 characters".into());
		}
		if let Some(source) = &self.routes_from {
			let local = ["http://127.0.0.1:", "http://localhost:", "http://[::1]:"]
				.iter()
				.any(|p| source.url.starts_with(p));
			if !source.url.starts_with("https://") && !local {
				return Err(format!("routes_from's {} is not https", source.url));
			}
			if source.token.len() < 32 {
				return Err("routes_from.token is shorter than 32 characters".into());
			}
		}
		if let Some(source) = &self.spec_from {
			// Plain HTTP only to this machine (a test's server): the token crosses the internet.
			for url in std::iter::once(&source.url).chain(source.placement_url.iter()) {
				let local = ["http://127.0.0.1:", "http://localhost:", "http://[::1]:"]
					.iter()
					.any(|p| url.starts_with(p));
				if !url.starts_with("https://") && !local {
					return Err(format!("spec_from's {url} is not https"));
				}
			}
			if source.token.len() < 32 {
				return Err("spec_from.token is shorter than 32 characters".into());
			}
			if source.every_secs == 0 {
				return Err("spec_from.every_secs is 0".into());
			}
		}
		Ok(())
	}
}
