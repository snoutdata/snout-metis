//! The leader's loop: every tick, decide from the spec and the agents' reports, and replicate the
//! decision when it differs from the last one.
//!
//! Only the leader decides, and a member that has just become leader waits one full
//! `host_timeout_secs` first: until then its view of the hosts is not complete, and deciding from
//! it would take a host it simply has not heard from yet for a lost one (the agents report to every
//! member, so in the steady state a new leader already has them; a member that has just started
//! does not).
//!
//! The database primary's region, which the table prefers (MT4), is taken from the agents' database
//! reports (`database.rs`), the last known primary kept in the replicated decision, and only then
//! the member's config.

use super::server::Member;
use super::types::{Command, Decision, State};
use crate::database::{
	DatabasePrimary, HostDatabases, HostProjects, PrimaryFrom, ProjectPrimary, ProjectReport,
	database_primary, project_primaries,
};
use crate::decide::decide;
use crate::replay::Logged;
use crate::spec::{Facts, HostFacts, Spec};
use openraft::ServerState;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;

pub fn now_ms() -> u64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_millis() as u64)
		.unwrap_or(0)
}

/// One decision, if this member is the leader and has listened long enough. Returns whether it
/// replicated a new decision.
pub async fn tick(member: &Member) -> bool {
	let state = member.raft.metrics().borrow().state;
	let mut leader_since = member.leader_since.lock().await;
	if state != ServerState::Leader {
		*leader_since = None;
		return false;
	}
	let now = now_ms();
	let since = *leader_since.get_or_insert(now);
	drop(leader_since);
	let replicated = member.state.state().await;
	let Some(spec) = replicated.spec else {
		return false;
	};
	let listened_from = since.max(member.started_ms);
	if now.saturating_sub(listened_from) < spec.host_timeout_secs.saturating_mul(1000) {
		return false;
	}
	let reports = member.reports.lock().await.clone();
	let last = replicated
		.decision
		.as_ref()
		.and_then(|d| d.database.clone());
	let primary = primary(member, &spec, now, last.as_ref())
		.await
		.map(|(p, _)| p);
	let last_projects = replicated
		.decision
		.as_ref()
		.map(|d| d.projects.clone())
		.unwrap_or_default();
	let projects = projects(member, &spec, now, &last_projects).await;
	let report_only = member.report_only.lock().await.clone();
	let facts = Facts {
		now_ms: now,
		hosts: spec
			.hosts
			.iter()
			.filter_map(|h| {
				reports.get(&h.name).map(|(at, copies)| HostFacts {
					name: h.name.clone(),
					last_heartbeat_ms: *at,
					copies: copies.clone(),
					report_only: report_only.contains(&h.name),
				})
			})
			.collect(),
		database_primary_region: primary
			.as_ref()
			.map(|p| p.region.clone())
			.or_else(|| member.config.database_primary_region.clone()),
		// A project's own primary, for its environment's copies (the fleet's projects; the control
		// plane's services are the environment `control-plane`, which no ref is).
		database_hosts: projects
			.iter()
			.map(|(project, p)| (project.clone(), p.host.clone()))
			.collect(),
		// What the last decision carried on: the rebalancer's move in flight and the rollbacks.
		moves: replicated
			.decision
			.as_ref()
			.map(|d| d.outcome.moves.clone())
			.unwrap_or_default(),
		rollbacks: replicated
			.decision
			.as_ref()
			.map(|d| d.outcome.rollbacks.clone())
			.unwrap_or_default(),
	};
	let outcome = decide(&spec, &facts);
	if replicated.decision.as_ref().is_some_and(|d| {
		d.outcome.placement == outcome.placement
			&& d.outcome.table == outcome.table
			&& d.outcome.heard == outcome.heard
			&& d.outcome.moves == outcome.moves
			&& d.outcome.rollbacks == outcome.rollbacks
			&& d.outcome.shortfalls == outcome.shortfalls
			&& d.database == primary
			&& d.projects == projects
	}) {
		return false;
	}
	let summary = describe(&outcome);
	let moved = primary
		.as_ref()
		.filter(|p| {
			last.as_ref()
				.is_none_or(|l| l.host != p.host || l.epoch != p.epoch)
		})
		.map(|p| {
			format!(
				"database primary now {} in region {} (epoch {})",
				p.host, p.region, p.epoch
			)
		});
	let project_moves: Vec<String> = projects
		.iter()
		.filter(|(project, p)| last_projects.get(*project) != Some(*p))
		.map(|(project, p)| {
			format!(
				"project {project} primary now {} (epoch {})",
				p.host, p.epoch
			)
		})
		.chain(
			last_projects
				.keys()
				.filter(|project| !projects.contains_key(*project))
				.map(|project| format!("project {project} has no known primary")),
		)
		.collect();
	let logged = Logged {
		at_ms: now,
		by: member.config.id,
		facts,
		outcome: outcome.clone(),
	};
	let decision = Decision {
		at_ms: now,
		by: member.config.id,
		outcome,
		database: primary,
		projects,
	};
	let written = member
		.raft
		.client_write(Command::Decided(decision))
		.await
		.is_ok();
	if written {
		let log = member.decisions.clone();
		let recorded = tokio::task::spawn_blocking(move || log.write(&spec, logged)).await;
		if let Err(e) = recorded.map_err(std::io::Error::other).and_then(|r| r) {
			member
				.events
				.note("decision", format!("not written to the decision log: {e}"));
		}
		if let Some(text) = moved {
			member.events.note("database", text);
		}
		for text in project_moves {
			member.events.note("database", text);
		}
		member.events.note("decision", summary);
	}
	written
}

/// The database primary from this member's own database reports and the last one known.
async fn primary(
	member: &Member,
	spec: &Spec,
	now: u64,
	last: Option<&DatabasePrimary>,
) -> Option<(DatabasePrimary, PrimaryFrom)> {
	let reports: Vec<HostDatabases> = member
		.databases
		.lock()
		.await
		.iter()
		.map(|(host, (at, databases))| HostDatabases {
			host: host.clone(),
			received_ms: *at,
			databases: databases.clone(),
		})
		.collect();
	database_primary(spec, now, &reports, last)
}

/// Each project's primary from this member's own reports: a host's project report, or for a host
/// whose agent sends none yet (an older build), its guarded copies' database report.
pub async fn projects(
	member: &Member,
	spec: &Spec,
	now: u64,
	last: &BTreeMap<String, ProjectPrimary>,
) -> BTreeMap<String, ProjectPrimary> {
	let mut reports: BTreeMap<String, HostProjects> = member
		.databases
		.lock()
		.await
		.iter()
		.map(|(host, (at, databases))| {
			(
				host.clone(),
				HostProjects {
					host: host.clone(),
					received_ms: *at,
					projects: databases.iter().map(ProjectReport::from).collect(),
				},
			)
		})
		.collect();
	for (host, (at, projects)) in member.projects.lock().await.iter() {
		reports.insert(
			host.clone(),
			HostProjects {
				host: host.clone(),
				received_ms: *at,
				projects: projects.clone(),
			},
		);
	}
	let reports: Vec<HostProjects> = reports.into_values().collect();
	project_primaries(spec, now, &reports, last)
}

/// `/status`'s `database`: where this member takes the primary to be now, and from what. Any member
/// answers it from its own reports, so a follower shows what it would decide from.
pub async fn database_status(member: &Member, state: &State, now: u64) -> Value {
	let last = state.decision.as_ref().and_then(|d| d.database.as_ref());
	let found = match &state.spec {
		Some(spec) => primary(member, spec, now, last).await,
		None => None,
	};
	match (found, &member.config.database_primary_region) {
		(Some((p, from)), _) => json!({
			"cluster": p.cluster,
			"primary_host": p.host,
			"region": p.region,
			"epoch": p.epoch,
			"from": from,
		}),
		(None, Some(region)) => json!({
			"cluster": null,
			"primary_host": null,
			"region": region,
			"epoch": null,
			"from": "config",
		}),
		(None, None) => Value::Null,
	}
}

/// One line for a decision: where each copy runs, then its notes.
fn describe(outcome: &crate::decide::Outcome) -> String {
	let mut parts: Vec<String> = outcome
		.placement
		.iter()
		.map(|p| format!("{} {} x{} on {}", p.service, p.version, p.count, p.host))
		.collect();
	if parts.is_empty() {
		parts.push("nothing placed".into());
	}
	let mut text = parts.join(", ");
	for note in &outcome.notes {
		text.push_str("; ");
		text.push_str(note);
	}
	text
}

/// The hosts whose report a member can let go: no longer in the spec AND silent past the timeout.
/// A host the spec dropped that still reports (a drain) is kept, and so is a host in the spec
/// however long it has been silent (a lost host the operator must see). Without this a member held
/// every host it had ever heard from until it restarted, and `/status` drew each one.
pub fn forgotten<'a>(
	spec: &Spec,
	reports: impl IntoIterator<Item = (&'a String, u64)>,
	now: u64,
	timeout_ms: u64,
) -> Vec<String> {
	reports
		.into_iter()
		.filter(|(host, at)| {
			now.saturating_sub(*at) > timeout_ms && !spec.hosts.iter().any(|h| &h.name == *host)
		})
		.map(|(host, _)| host.clone())
		.collect()
}

/// Notes a host going silent past its timeout, once; `report` notes its return. Forgets a silent
/// host the spec no longer names ([`forgotten`]).
async fn watch_hosts(member: &Member) {
	let spec = member.state.state().await.spec;
	let timeout_ms = spec
		.as_ref()
		.map(|s| s.host_timeout_secs.saturating_mul(1000))
		.unwrap_or(30_000);
	let now = now_ms();
	let reports = member.reports.lock().await.clone();
	let mut silent = member.silent.lock().await;
	for (host, (at, _)) in &reports {
		let age = now.saturating_sub(*at);
		if age > timeout_ms && silent.insert(host.clone()) {
			member
				.events
				.note("host", format!("{host} silent for {} s", age / 1000));
		}
	}
	let Some(spec) = spec else {
		return;
	};
	let gone = forgotten(
		&spec,
		reports.iter().map(|(h, (at, _))| (h, *at)),
		now,
		timeout_ms,
	);
	if gone.is_empty() {
		return;
	}
	let mut held = member.reports.lock().await;
	let mut report_only = member.report_only.lock().await;
	for host in gone {
		// A report that arrived since the copy above is fresh: keep it.
		if held
			.get(&host)
			.is_some_and(|(at, _)| now.saturating_sub(*at) > timeout_ms)
		{
			held.remove(&host);
			silent.remove(&host);
			report_only.remove(&host);
			member
				.events
				.note("host", format!("forgot {host}: not in the spec and silent"));
		}
	}
}

/// Runs [`tick`] until the member stops.
pub async fn run(member: Member) {
	let mut interval = tokio::time::interval(Duration::from_millis(member.config.tick_ms));
	loop {
		interval.tick().await;
		if member.stopping.load(std::sync::atomic::Ordering::SeqCst) {
			return;
		}
		watch_hosts(&member).await;
		tick(&member).await;
	}
}

#[cfg(test)]
mod tests {
	use super::forgotten;
	use crate::spec::Spec;

	fn spec(hosts: &[&str]) -> Spec {
		let hosts: Vec<_> = hosts
			.iter()
			.map(|h| serde_json::json!({"name": h, "region": "r", "fault_domain": "f", "capacity": {"millicpu": 1000, "memory_mb": 1024}}))
			.collect();
		serde_json::from_value(
			serde_json::json!({"host_timeout_secs": 30, "hosts": hosts, "services": []}),
		)
		.unwrap()
	}

	#[test]
	fn forgets_only_a_silent_host_the_spec_dropped() {
		let s = spec(&["kept-silent", "kept-fresh"]);
		let names = [
			"kept-silent",
			"kept-fresh",
			"dropped-fresh",
			"dropped-silent",
		]
		.map(String::from);
		let reports = [
			(&names[0], 0),
			(&names[1], 99_000),
			(&names[2], 99_000),
			(&names[3], 0),
		];
		assert_eq!(
			forgotten(&s, reports, 100_000, 30_000),
			vec!["dropped-silent".to_string()]
		);
	}
}
