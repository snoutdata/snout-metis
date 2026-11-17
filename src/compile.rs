//! From a placement to what each host's agent runs: the `hostServices` of its desired-state file
//! (MT5).
//!
//! Metis writes only the entries of the services its spec names. Everything else in the file is
//! carried over as it is: `projects` is the database layer's (MT1), and a host service the spec does
//! not name (a host's realtime or storage) is not Metis's. A host runs at most one copy
//! of a service until the proxy can route to more than one, since every copy of a host
//! service is published on the same fixed host port.

use crate::decide::Assignment;
use crate::spec::Spec;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// A `HostService` entry as the host agent reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostServiceEntry {
	pub name: String,
	pub image: String,
	pub port: u16,
	pub host_port: u16,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub health_path: Option<String>,
	pub memory_mb: u32,
	/// Always written, empty or not: an older agent could not make a service whose entry
	/// had no `env`, and a newer one reads a missing one as empty.
	#[serde(default)]
	pub env: BTreeMap<String, String>,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub command: Vec<String>,
	/// Names only, as in the spec: the values are files on the host (`RunSpec::secret_env`).
	#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
	pub secret_env: BTreeMap<String, String>,
	#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
	pub files: BTreeMap<String, String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub joins: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub env_from: Option<EntryEnvFrom>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub database_url: Option<EntryDatabaseUrl>,
}

/// `RunSpec::env_from`, as the agent reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryEnvFrom {
	pub project: String,
	pub service: String,
}

/// `RunSpec::database_url`, as the agent reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryDatabaseUrl {
	pub env: String,
	pub role: String,
	pub password_secret: String,
	pub host: String,
	pub port: u16,
	pub database: String,
}

/// The entries one host's agent is told to run, from the placement, for the services that have a
/// `run` block.
pub fn host_services(
	spec: &Spec,
	placement: &[Assignment],
	host: &str,
) -> Result<Vec<HostServiceEntry>, String> {
	let mut out: Vec<HostServiceEntry> = Vec::new();
	let mut ports = BTreeSet::new();
	for assignment in placement.iter().filter(|a| a.host == host) {
		let Some(service) = spec
			.services
			.iter()
			.find(|s| s.environment == assignment.environment && s.name == assignment.service)
		else {
			continue;
		};
		let Some(run) = &service.run else {
			continue;
		};
		let id = service.id();
		if assignment.count > 1 || out.iter().any(|e| e.name == service.name) {
			return Err(format!(
				"{id}: more than one copy on {host}; one per host until the proxy routes to more"
			));
		}
		if run.host_port != 0 && !ports.insert(run.host_port) {
			return Err(format!(
				"{id}: host port {} on {host} is taken by another service",
				run.host_port
			));
		}
		out.push(HostServiceEntry {
			name: service.name.clone(),
			image: format!("{}:{}", run.image_repo, assignment.version),
			port: run.port,
			host_port: run.host_port,
			health_path: run.health_path.clone(),
			memory_mb: run.memory_mb,
			env: run.env.clone(),
			command: run.command.clone(),
			secret_env: run.secret_env.clone(),
			files: run.files.clone(),
			joins: run.joins.clone(),
			env_from: run.env_from.as_ref().map(|from| EntryEnvFrom {
				project: from.project.clone(),
				service: from.service.clone(),
			}),
			database_url: run.database_url.as_ref().map(|url| EntryDatabaseUrl {
				env: url.env.clone(),
				role: url.role.clone(),
				password_secret: url.password_secret.clone(),
				host: url.host.clone(),
				port: url.port,
				database: url.database.clone(),
			}),
		});
	}
	// A copy that shares another's network namespace is nothing without it: refused here, so the
	// agent keeps the file it has (MT2) rather than make a container with nowhere to live.
	for entry in &out {
		if let Some(owner) = &entry.joins
			&& !out.iter().any(|other| &other.name == owner)
		{
			return Err(format!(
				"{}: joins {owner}, which is not placed on {host}",
				entry.name
			));
		}
	}
	out.sort_by(|a, b| a.name.cmp(&b.name));
	Ok(out)
}

/// What changes in one host's `hostServices`, by service name.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HostChange {
	pub added: Vec<String>,
	pub changed: Vec<String>,
	pub removed: Vec<String>,
}

impl HostChange {
	pub fn is_empty(&self) -> bool {
		self.added.is_empty() && self.changed.is_empty() && self.removed.is_empty()
	}
}

/// The host's desired-state file with Metis's entries in place, and what changed. `current` is the
/// file as the host has it; every key but `hostServices`, and every entry of a service the spec
/// does not name, is kept exactly.
pub fn merge(
	spec: &Spec,
	current: &str,
	entries: &[HostServiceEntry],
) -> Result<(String, HostChange), String> {
	let mut file: Value = serde_json::from_str(current)
		.map_err(|e| format!("the desired-state file does not parse: {e}"))?;
	let Some(object) = file.as_object_mut() else {
		return Err("the desired-state file is not an object".into());
	};
	let ours: BTreeSet<&str> = spec
		.services
		.iter()
		.filter(|s| s.run.is_some())
		.map(|s| s.name.as_str())
		.collect();
	let existing: Vec<Value> = match object.get("hostServices") {
		None | Some(Value::Null) => Vec::new(),
		Some(Value::Array(items)) => items.clone(),
		Some(_) => return Err("\"hostServices\" is not an array".into()),
	};

	let mut change = HostChange::default();
	let mut kept: Vec<Value> = Vec::new();
	let mut before: BTreeMap<String, Value> = BTreeMap::new();
	for item in existing {
		let name = item
			.get("name")
			.and_then(Value::as_str)
			.unwrap_or_default()
			.to_string();
		if ours.contains(name.as_str()) {
			before.insert(name, item);
		} else {
			kept.push(item);
		}
	}
	for entry in entries {
		let value = serde_json::to_value(entry).map_err(|e| e.to_string())?;
		match before.remove(&entry.name) {
			None => change.added.push(entry.name.clone()),
			Some(old) if old != value => change.changed.push(entry.name.clone()),
			Some(_) => {}
		}
		kept.push(value);
	}
	change.removed = before.into_keys().collect();
	kept.sort_by(|a, b| {
		let name = |v: &Value| {
			v.get("name")
				.and_then(Value::as_str)
				.unwrap_or_default()
				.to_string()
		};
		name(a).cmp(&name(b))
	});
	object.insert("hostServices".into(), Value::Array(kept));
	let text = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;
	Ok((text + "\n", change))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::spec::{Facts, HostSpec, Resources, RunSpec, ServiceSpec, VersionShare};

	/// A control plane's `functions` entry as an agent runs it on remote-1 (it carries no secret).
	const LIVE_FUNCTIONS: &str = r#"{"name":"functions","image":"registry.example.com/snout-functions:0.2.1","port":9000,"hostPort":9000,"healthPath":"/_snoutpod/health","memoryMb":256,"env":{"SNOUT_FUNCTIONS_BUILDER":"docker.io/denoland/deno:distroless-2.9.7"},"command":["start","--main-service","/snoutfn/main","--port","9000"]}"#;

	fn functions(copies: u32) -> ServiceSpec {
		ServiceSpec {
			name: "functions".into(),
			environment: "control-plane".into(),
			copies,
			versions: vec![VersionShare {
				version: "0.2.1".into(),
				share: 1,
			}],
			per_copy: Resources {
				millicpu: 250,
				memory_mb: 256,
			},
			one_clock: false,
			dedicated_to: None,
			run: Some(RunSpec {
				image_repo: "registry.example.com/snout-functions".into(),
				port: 9000,
				host_port: 9000,
				health_path: Some("/_snoutpod/health".into()),
				memory_mb: 256,
				env: [(
					"SNOUT_FUNCTIONS_BUILDER".to_string(),
					"docker.io/denoland/deno:distroless-2.9.7".to_string(),
				)]
				.into(),
				command: ["start", "--main-service", "/snoutfn/main", "--port", "9000"]
					.map(String::from)
					.to_vec(),
				..Default::default()
			}),
			placement: Default::default(),
			routed: true,
			scale: None,
			policy: Default::default(),
			limit: None,
			priority: 0,
			rollout: None,
		}
	}

	fn spec(services: Vec<ServiceSpec>) -> Spec {
		let host = |name: &str| HostSpec {
			name: name.into(),
			region: "r".into(),
			fault_domain: name.into(),
			serves: true,
			draining: false,
			serves_home: false,
			frozen: false,
			finishing: false,
			dedicated_to: None,
			server: None,
			owner: None,
			room: None,
			capacity: Resources {
				millicpu: 2000,
				memory_mb: 2048,
			},
		};
		Spec {
			host_timeout_secs: 60,
			hosts: vec![host("remote-1"), host("home-2")],
			services,
			database_cluster: None,
			agents: Vec::new(),
			pressure_percent: None,
			rebalance: None,
		}
	}

	fn assignment(host: &str, version: &str, count: u32) -> Assignment {
		Assignment {
			host: host.into(),
			environment: "control-plane".into(),
			service: "functions".into(),
			version: version.into(),
			count,
		}
	}

	#[test]
	fn what_it_compiles_is_what_the_control_plane_runs() {
		let s = spec(vec![functions(1)]);
		let entries =
			host_services(&s, &[assignment("remote-1", "0.2.1", 1)], "remote-1").expect("compiles");
		let live: HostServiceEntry = serde_json::from_str(LIVE_FUNCTIONS).expect("parses");
		assert_eq!(entries, vec![live]);
		let current = format!(
			r#"{{"hostName":"remote-1","projects":[{{"spec":{{"ref":"cpprojectref1"}}}}],"hostServices":[{LIVE_FUNCTIONS}]}}"#
		);
		let (merged, change) = merge(&s, &current, &entries).expect("merges");
		assert!(change.is_empty(), "{change:?}");
		let before: Value = serde_json::from_str(&current).expect("parses");
		let after: Value = serde_json::from_str(&merged).expect("parses");
		assert_eq!(before, after);
	}

	#[test]
	fn an_entry_with_no_environment_still_writes_env() {
		// An older agent could not make an entry with no `env` at all.
		let mut bare = functions(1);
		if let Some(run) = bare.run.as_mut() {
			run.env.clear();
		}
		let s = spec(vec![bare]);
		let entries =
			host_services(&s, &[assignment("home-2", "0.2.1", 1)], "home-2").expect("compiles");
		let text = serde_json::to_string(&entries).expect("serializes");
		assert!(text.contains(r#""env":{}"#), "{text}");
		// And an entry written without one, by an older member, still reads.
		let read: HostServiceEntry = serde_json::from_str(
			r#"{"name":"x","image":"i:1","port":1,"hostPort":2,"memoryMb":64}"#,
		)
		.expect("parses");
		assert!(read.env.is_empty());
	}

	#[test]
	fn the_database_layer_and_unnamed_services_are_left_alone() {
		let s = spec(vec![functions(1)]);
		let current = r#"{"hostName":"home-2","projects":[{"spec":{"ref":"x"},"role":"standby"}],"hostServices":[{"name":"realtime","image":"r:1","port":4000,"hostPort":4000,"memoryMb":128}]}"#;
		let entries =
			host_services(&s, &[assignment("home-2", "0.2.2", 1)], "home-2").expect("compiles");
		let (merged, change) = merge(&s, current, &entries).expect("merges");
		assert_eq!(change.added, vec!["functions".to_string()]);
		let after: Value = serde_json::from_str(&merged).expect("parses");
		assert_eq!(after["projects"][0]["role"], "standby");
		assert_eq!(after["hostServices"][1]["name"], "realtime");
		assert_eq!(
			after["hostServices"][0]["image"],
			"registry.example.com/snout-functions:0.2.2"
		);
	}

	#[test]
	fn a_service_placed_elsewhere_is_removed_from_this_host() {
		let s = spec(vec![functions(1)]);
		let current = format!(r#"{{"projects":[],"hostServices":[{LIVE_FUNCTIONS}]}}"#);
		let entries =
			host_services(&s, &[assignment("home-2", "0.2.1", 1)], "remote-1").expect("compiles");
		assert!(entries.is_empty());
		let (_, change) = merge(&s, &current, &entries).expect("merges");
		assert_eq!(change.removed, vec!["functions".to_string()]);
	}

	#[test]
	fn two_copies_on_one_host_are_refused_until_the_proxy_can_route_them() {
		let s = spec(vec![functions(2)]);
		let err = host_services(&s, &[assignment("home-2", "0.2.1", 2)], "home-2").unwrap_err();
		assert!(err.contains("one per host"));
		let two_versions = [
			assignment("home-2", "0.2.1", 1),
			assignment("home-2", "0.2.2", 1),
		];
		assert!(host_services(&s, &two_versions, "home-2").is_err());
	}

	#[test]
	fn decide_then_compile_keeps_functions_where_it_runs() {
		use crate::spec::{CopyFacts, CopyState, HostFacts};
		let s = spec(vec![functions(1)]);
		let facts = Facts {
			now_ms: 10_000,
			hosts: vec![
				HostFacts {
					name: "remote-1".into(),
					last_heartbeat_ms: 9_000,
					copies: vec![CopyFacts {
						environment: "control-plane".into(),
						service: "functions".into(),
						version: "0.2.1".into(),
						state: CopyState::Running,
						load: None,
						running_since_ms: None,
					}],
					report_only: false,
				},
				HostFacts {
					name: "home-2".into(),
					last_heartbeat_ms: 9_000,
					copies: vec![],
					report_only: false,
				},
			],
			database_primary_region: None,
			database_hosts: Default::default(),
			moves: Vec::new(),
			rollbacks: Default::default(),
		};
		let outcome = crate::decide(&s, &facts);
		assert!(outcome.notes.is_empty(), "{:?}", outcome.notes);
		assert_eq!(
			host_services(&s, &outcome.placement, "remote-1")
				.expect("compiles")
				.len(),
			1
		);
		assert!(
			host_services(&s, &outcome.placement, "home-2")
				.expect("compiles")
				.is_empty()
		);
	}

	#[test]
	fn the_secret_names_reach_the_agents_entry_in_its_own_casing() {
		let mut service = functions(1);
		let run = service.run.as_mut().expect("run");
		run.secret_env
			.insert("SOME_PASSWORD".into(), "some-file".into());
		run.files.insert("/etc/ca.pem".into(), "ca.pem".into());
		let s = spec(vec![service]);
		let entries =
			host_services(&s, &[assignment("home-2", "0.2.1", 1)], "home-2").expect("compiles");
		let value = serde_json::to_value(&entries[0]).expect("serializes");
		assert_eq!(value["secretEnv"]["SOME_PASSWORD"], "some-file");
		assert_eq!(value["files"]["/etc/ca.pem"], "ca.pem");
		assert!(value.get("joins").is_none() && value.get("databaseUrl").is_none());
	}
}
