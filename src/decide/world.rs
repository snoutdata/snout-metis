//! What one decision knows, read once from the spec and the facts: which hosts are heard, which only
//! report or are held as they are, and every reported copy, indexed. Read only: everything that
//! changes while deciding is on the board (`room.rs`).

use super::STEADY_MS;
use super::note::Note;
use super::room::Chosen;
use crate::spec::{CopyFacts, CopyState, Facts, HostSpec, ServiceSpec, Spec};
use std::collections::{BTreeMap, BTreeSet};

pub(super) struct World<'a> {
	pub spec: &'a Spec,
	pub facts: &'a Facts,
	/// Heard within the host timeout.
	pub alive: BTreeSet<&'a str>,
	/// A host whose agent only reports keeps what runs there and is never given a copy: a copy placed
	/// there would never run, and the table would route nowhere for it. A frozen host (full, and must
	/// not change) is held the same way: what runs there stays, nothing new goes there and nothing is
	/// moved off it, while it is heard; a frozen host lost is a host lost.
	pub report_only: BTreeSet<&'a str>,
	/// A host held because its drain was withdrawn: held like a frozen one, except that a move already
	/// in flight off it finishes: moves in flight finish, nothing moves back.
	pub finishing: BTreeSet<&'a str>,
	pub copies: CopyIndex<'a>,
}

impl<'a> World<'a> {
	/// The world, and a note for each host that is silent.
	pub fn new(spec: &'a Spec, facts: &'a Facts, notes: &mut Vec<Note>) -> Self {
		let timeout_ms = spec.host_timeout_secs.saturating_mul(1000);
		let reported: BTreeMap<&str, &crate::spec::HostFacts> =
			facts.hosts.iter().map(|h| (h.name.as_str(), h)).collect();
		let alive: BTreeSet<&str> = spec
			.hosts
			.iter()
			.filter(|h| {
				reported
					.get(h.name.as_str())
					.is_some_and(|f| facts.now_ms.saturating_sub(f.last_heartbeat_ms) <= timeout_ms)
			})
			.map(|h| h.name.as_str())
			.collect();
		for host in &spec.hosts {
			if !alive.contains(host.name.as_str()) {
				// Said of a host that reported once; one never heard is only missing.
				if reported.contains_key(host.name.as_str()) {
					notes.push(Note::Silent {
						host: host.name.clone(),
						secs: spec.host_timeout_secs,
					});
				}
			}
		}
		let report_only = facts
			.hosts
			.iter()
			.filter(|h| h.report_only)
			.map(|h| h.name.as_str())
			.chain(
				spec.hosts
					.iter()
					.filter(|h| h.frozen)
					.map(|h| h.name.as_str()),
			)
			.collect();
		let finishing = spec
			.hosts
			.iter()
			.filter(|h| h.frozen && h.finishing)
			.map(|h| h.name.as_str())
			.collect();
		World {
			spec,
			facts,
			alive,
			report_only,
			finishing,
			copies: CopyIndex::new(facts),
		}
	}

	/// Where a service's database is, as far as this decision knows: the host of its environment's
	/// primary (`Facts::database_hosts`, a fleet project's) when the spec names that host, and the
	/// region of it, or else the one primary region of the facts (the control plane's).
	pub fn database(&self, service: &ServiceSpec) -> Database<'a> {
		let host = self
			.facts
			.database_hosts
			.get(&service.environment)
			.and_then(|name| self.spec.hosts.iter().find(|h| &h.name == name));
		match host {
			Some(h) => Database {
				host: Some(h.name.as_str()),
				region: Some(h.region.as_str()),
			},
			None => Database {
				host: None,
				region: self.facts.database_primary_region.as_deref(),
			},
		}
	}

	/// A spec's service, by environment and name.
	pub fn service(&self, environment: &str, name: &str) -> Option<&'a ServiceSpec> {
		self.spec
			.services
			.iter()
			.find(|s| s.environment == environment && s.name == name)
	}

	/// A host a rebalance may move to or from: heard, serving, not draining and not held.
	pub fn open(&self, h: &HostSpec) -> bool {
		h.serves
			&& !h.draining
			&& self.alive.contains(h.name.as_str())
			&& !self.report_only.contains(h.name.as_str())
	}

	/// `host` reports a copy of one of the environment's services that its agent has not given up on.
	pub fn still_runs(&self, host: &str, environment: &str) -> bool {
		self.spec
			.services
			.iter()
			.filter(|s| s.environment == environment)
			.any(|s| {
				self.copies
					.on(host, s)
					.iter()
					.any(|c| c.state != CopyState::GaveUp)
			})
	}

	/// The service's copies running on `host`, by version.
	pub fn running_versions(&self, host: &str, service: &ServiceSpec) -> BTreeMap<&'a str, u32> {
		let mut out = BTreeMap::new();
		for c in self.copies.on(host, service) {
			if c.state == CopyState::Running {
				*out.entry(c.version.as_str()).or_default() += 1;
			}
		}
		out
	}

	/// The service's copies of `version` on `host` its agent has not given up on.
	pub fn present(&self, host: &str, service: &ServiceSpec, version: &str) -> u32 {
		self.copies
			.on(host, service)
			.iter()
			.filter(|c| c.version == version && c.state != CopyState::GaveUp)
			.count() as u32
	}

	/// The service's copies of `version` running on `host`.
	pub fn running(&self, host: &str, service: &ServiceSpec, version: &str) -> u32 {
		self.copies
			.on(host, service)
			.iter()
			.filter(|c| c.version == version && c.state == CopyState::Running)
			.count() as u32
	}

	/// The service's copies of `version` on `host` that have run and answered for [`STEADY_MS`].
	pub fn steady(&self, host: &str, service: &ServiceSpec, version: &str) -> u32 {
		self.steady_named(host, &service.environment, &service.name, version)
	}

	pub fn steady_named(&self, host: &str, environment: &str, name: &str, version: &str) -> u32 {
		self.copies
			.on_named(host, environment, name)
			.iter()
			.filter(|c| c.version == version && self.is_steady(c))
			.count() as u32
	}

	fn is_steady(&self, copy: &CopyFacts) -> bool {
		copy.state == CopyState::Running
			&& copy
				.running_since_ms
				.is_some_and(|since| self.facts.now_ms.saturating_sub(since) >= STEADY_MS)
	}

	/// Of the copies chosen for a service, how many already run steady where they are chosen.
	pub fn steady_chosen(&self, service: &ServiceSpec, chosen: &Chosen) -> u32 {
		chosen
			.iter()
			.map(|(host, version, n)| self.steady(host, service, version).min(n))
			.sum()
	}
}

/// Where a service's database is (`World::database`).
#[derive(Clone, Copy)]
pub(super) struct Database<'a> {
	pub host: Option<&'a str>,
	pub region: Option<&'a str>,
}

/// The reported copies by host and service, built once per decision: at fleet size a decision
/// holds hundreds of services on tens of hosts, and scanning every host's report for each of them
/// is what made it slow.
pub(super) struct CopyIndex<'a> {
	/// environment -> service -> (host, copy), every reported host, silent ones too. Nested rather
	/// than keyed by a tuple, so a lookup borrows a plain `str`.
	by_service: Nested<'a, Vec<(&'a str, &'a CopyFacts)>>,
	/// host -> environment -> service -> copies.
	by_host: BTreeMap<&'a str, Nested<'a, Vec<&'a CopyFacts>>>,
}

type Nested<'a, T> = BTreeMap<&'a str, BTreeMap<&'a str, T>>;

impl<'a> CopyIndex<'a> {
	fn new(facts: &'a Facts) -> Self {
		let mut by_service: Nested<'a, Vec<_>> = BTreeMap::new();
		let mut by_host: BTreeMap<&'a str, Nested<'a, Vec<_>>> = BTreeMap::new();
		for host in &facts.hosts {
			for copy in &host.copies {
				by_service
					.entry(copy.environment.as_str())
					.or_default()
					.entry(copy.service.as_str())
					.or_default()
					.push((host.name.as_str(), copy));
				by_host
					.entry(host.name.as_str())
					.or_default()
					.entry(copy.environment.as_str())
					.or_default()
					.entry(copy.service.as_str())
					.or_default()
					.push(copy);
			}
		}
		CopyIndex {
			by_service,
			by_host,
		}
	}

	pub fn hosts_of(&self, service: &ServiceSpec) -> &[(&'a str, &'a CopyFacts)] {
		self.by_service
			.get(service.environment.as_str())
			.and_then(|s| s.get(service.name.as_str()))
			.map_or(&[], Vec::as_slice)
	}

	pub fn on(&self, host: &str, service: &ServiceSpec) -> &[&'a CopyFacts] {
		self.on_named(host, &service.environment, &service.name)
	}

	pub fn on_named(&self, host: &str, environment: &str, name: &str) -> &[&'a CopyFacts] {
		self.by_host
			.get(host)
			.and_then(|e| e.get(environment))
			.and_then(|s| s.get(name))
			.map_or(&[], Vec::as_slice)
	}
}
