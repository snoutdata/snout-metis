//! TEST ONLY: `decide.rs` as it was at d9a1d3095, before it was split into its steps. The differential
//! test (`differential_tests.rs`) runs this and the new `decide` over generated and logged inputs and
//! requires the same outcome, byte for byte (34d65b1c9), or a difference one of the fixes made since
//! explains (`fixed`). Deleted once the new core has been live a while.
#![allow(dead_code, clippy::all)]

use crate::spec::{
	CopyFacts, CopyState, Facts, HostSpec, Move, PlaceMode, Qos, Rebalance, Resources, Rollback,
	ServiceSpec, Spec, VersionShare,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// `count` copies of one version of one service, on one host: what that host's agent is told to run.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Assignment {
	pub host: String,
	pub environment: String,
	pub service: String,
	pub version: String,
	pub count: u32,
}

/// One entry of the routing table: where a proxy may send a call for this service. Only copies
/// the agents report as running are routed to; `share` is the version's share of the calls.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Route {
	pub host: String,
	pub region: String,
	pub version: String,
	pub running: u32,
	/// Copies placed there that are not running yet. A proxy never sends a call to one; a door with no
	/// near copy running may hold a call a moment for one rather than send it far (`METIS_HOLD_MS`).
	#[serde(default, skip_serializing_if = "is_zero")]
	pub starting: u32,
	/// Copies placed there that wait for ANOTHER version of the service on the same host to stop (one
	/// copy per host: the agent drains the old one first). Not `starting`, since the old copy answers
	/// the same health and a hold would take it for this one; a door reads it only to keep
	/// this version's share near while the replacement happens.
	#[serde(default, skip_serializing_if = "is_zero")]
	pub replacing: u32,
	pub share: u32,
	/// The host is in the database primary's region: the proxy prefers it (MT4).
	pub near_database: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Outcome {
	pub placement: Vec<Assignment>,
	/// By service id (`environment/name`).
	pub table: BTreeMap<String, Vec<Route>>,
	/// What changed and why, for the log (MT6) and the alerts (MT11). Empty when nothing moved.
	pub notes: Vec<String>,
	/// The hosts this decision heard (reporting within the host timeout), by name. A host not in it
	/// was decided WITHOUT, so its share says nothing about what it should run: `/desired` tells it
	/// to keep its file (`server.rs`). Empty in a decision from a member before this field.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub heard: Vec<String>,
	/// The rebalancer's move in flight, which the next decision carries on (`Facts::moves`).
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub moves: Vec<Move>,
	/// Rollouts rolled back, by service id, which the next decision keeps (`Facts::rollbacks`).
	#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
	pub rollbacks: BTreeMap<String, Rollback>,
	/// Each version of a service placed short of its target, with every host the decision
	/// considered and why it took no more copies: what the admin console's hosts, the shortfall
	/// alert and the dashboard read, so none of them guesses why.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub shortfalls: Vec<Shortfall>,
}

/// One version of one service a decision could not give every copy it wants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shortfall {
	/// The service id (`environment/name`).
	pub service: String,
	pub version: String,
	pub wanted: u32,
	pub placed: u32,
	/// What one more copy needs (`per_copy`).
	pub needs: Resources,
	/// Every host of the spec, and why it took no more copies, in the spec's order.
	pub hosts: Vec<Refusal>,
}

/// One host's answer to one more copy of a service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
	pub host: String,
	pub reason: Refused,
	/// Whose host it is, when it is dedicated to a customer.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub dedicated_to: Option<String>,
	pub capacity: Resources,
	/// What is left on it after this decision's placements so far.
	pub free: Resources,
	/// The whole server, when the spec says (`HostSpec::server`).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub server: Option<Resources>,
}

/// Why a host took no more copies of a service. The first that applies, in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Refused {
	/// Dedicated to another customer.
	Dedicated,
	/// A shared host, and the service runs only on its customer's dedicated hosts.
	Shared,
	/// Not on the service's list of hosts or regions (`placement`). On the fleet, a pool's servers
	/// are kept off a shared project's list this way (sql/337).
	NotOnItsList,
	/// Not heard within the host timeout. After the three above: a host the service could never use
	/// is that, not silent (2026-11-17: bamboo's alert blamed a silent shared host it may not use).
	NotReporting,
	/// Being emptied to retire.
	Draining,
	/// Serves nothing (a witness host, or closed).
	NotServing,
	/// Its agent gave up on this service there.
	GaveUp,
	/// The service's copies are moving off it (pressure, the rebalancer).
	MovingOff,
	/// Full and held as it is: nothing new goes there.
	Frozen,
	/// Its agent only reports.
	OnlyReports,
	/// Its copies use more than `pressure_percent` of it.
	UnderPressure,
	/// Already holds as many copies of this service as one host may.
	HoldsItsShare,
	/// Not enough CPU or memory left for one more copy.
	Full,
}

impl Refused {
	/// A few words for the note, after a count.
	fn words(self) -> &'static str {
		match self {
			Refused::NotReporting => "not reporting",
			Refused::Dedicated => "dedicated to another customer",
			Refused::Shared => "shared, and it runs only on its customer's own hosts",
			Refused::NotOnItsList => "not on its list of hosts",
			Refused::Draining => "draining",
			Refused::NotServing => "serving nothing",
			Refused::GaveUp => "where its agent gave up on it",
			Refused::MovingOff => "it is moving off",
			Refused::Frozen => "frozen",
			Refused::OnlyReports => "only reporting",
			Refused::UnderPressure => "under pressure",
			Refused::HoldsItsShare => "already holding its copy",
			Refused::Full => "full",
		}
	}
}

/// How long a copy must have run and answered its health before a draining host's copy of the same
/// service is let go (packing).
pub const STEADY_MS: u64 = 30_000;

/// Decides from a spec that passed [`Spec::check`].
pub fn decide(spec: &Spec, facts: &Facts) -> Outcome {
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

	let mut notes = Vec::new();
	for host in &spec.hosts {
		if !alive.contains(host.name.as_str()) {
			let copies = reported
				.get(host.name.as_str())
				.map_or(0, |f| f.copies.len());
			if copies > 0 || reported.contains_key(host.name.as_str()) {
				notes.push(format!(
					"host {} is silent past {}s; its copies are placed elsewhere",
					host.name, spec.host_timeout_secs
				));
			}
		}
	}

	let copies = CopyIndex::new(facts);
	// A host whose agent only reports keeps what runs there and is never given a copy: a copy
	// placed there would never run, and the table would route nowhere for it. A frozen host
	// (full, and must not change) is held the same way: what runs there stays, nothing new
	// goes there and nothing is moved off it, while it is heard; a frozen host lost is a host lost.
	let report_only: BTreeSet<&str> = facts
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
	// A host held because its drain was withdrawn: held like a frozen one, except that a
	// move already in flight off it finishes: moves in flight finish, nothing moves back.
	let finishing: BTreeSet<&str> = spec
		.hosts
		.iter()
		.filter(|h| h.frozen && h.finishing)
		.map(|h| h.name.as_str())
		.collect();
	let mut free: BTreeMap<&str, Resources> = spec
		.hosts
		.iter()
		.map(|h| (h.name.as_str(), h.capacity))
		.collect();
	let mut placement = Vec::new();
	// (service id, host, version) -> share: an old version a host keeps through a roll (below).
	let mut held: BTreeMap<(String, String, String), u32> = BTreeMap::new();
	// The highest priority first, so it keeps and takes room before the lower ones; then by id. With
	// every priority 0 this is the order by id it always was.
	let mut services: Vec<&ServiceSpec> = spec.services.iter().collect();
	services.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.id().cmp(&b.id())));

	// Rollbacks: the ones the last decision made, while the spec still names the version that gave
	// up, and the new ones this decision finds. Each service's versions as this decision places them.
	let rollbacks = rollbacks(spec, facts, &alive, &copies, &mut notes);
	let shares: BTreeMap<String, Vec<VersionShare>> = spec
		.services
		.iter()
		.map(|s| {
			let id = s.id();
			let versions = effective_versions(s, rollbacks.get(&id));
			(id, versions)
		})
		.collect();
	// Hosts whose copies use more than the spec lets them (QoS), and the services moved off each.
	let pressure = pressure(spec, facts, &alive, &report_only, &mut notes);
	// The rebalancer's move in flight, if it can still be made.
	let moves = live_moves(spec, &alive, &report_only, facts, &mut notes);

	// First what runs, for every service, then what is missing. At fleet size one decision holds
	// many environments on the same hosts, and a copy that runs is never moved to make room for a
	// copy that does not yet: a new project's copies take what the running ones leave.
	let mut plans: VecDeque<ServicePlan> = VecDeque::with_capacity(services.len());
	for service in services {
		let id = service.id();
		let mut said = Vec::new();
		let wanted = wanted_copies(service, facts, &alive, &mut said);
		let targets = version_targets(&shares[&id], wanted);
		let gave_up: BTreeSet<&str> = copies
			.hosts_of(service)
			.iter()
			.filter(|(_, c)| c.state == CopyState::GaveUp)
			.map(|(h, _)| *h)
			.collect();
		for host in &gave_up {
			said.push(format!(
				"{id}: the agent on {host} gave up on it; placed on another host"
			));
		}
		// Hosts its copies move off, make before break: one under pressure that sheds this service,
		// or the rebalancer's `from` for its environment (with the host it goes to first).
		let mut off: BTreeMap<&str, Option<&str>> = BTreeMap::new();
		for (host, ids) in &pressure.shed {
			if ids.contains(&id) {
				off.insert(host, None);
			}
		}
		for m in &moves {
			if m.environment == service.environment
				&& copies
					.on(&m.from, service)
					.iter()
					.any(|c| c.state != CopyState::GaveUp)
			{
				off.insert(m.from.as_str(), Some(m.to.as_str()));
			}
		}
		let prefer = off.values().find_map(|p| *p);
		// Where its database is: a closed home (`serves_home`) takes this service's copies
		// alone, and packing starts there (below).
		let database = database_of(spec, facts, service);
		let eligible: Vec<&HostSpec> = spec
			.hosts
			.iter()
			.filter(|h| {
				(h.serves
					|| h.frozen || (h.serves_home
					&& !h.draining && database.host == Some(h.name.as_str())))
					&& alive.contains(h.name.as_str())
					&& h.dedicated_to == service.dedicated_to
					&& service.placement.allows(h)
					&& !gave_up.contains(h.name.as_str())
					&& !off.contains_key(h.name.as_str())
			})
			.collect();
		// The copies on a host they move off keep their room there until they go.
		let mut holding: Vec<(&str, &str, u32)> = Vec::new();
		for host in off.keys() {
			let mut running: BTreeMap<&str, u32> = BTreeMap::new();
			for c in copies.on(host, service) {
				if c.state == CopyState::Running {
					*running.entry(c.version.as_str()).or_default() += 1;
				}
			}
			for (version, n) in running {
				for _ in 0..n {
					free.insert(host, free[host].minus(service.per_copy));
				}
				holding.push((host, version, n));
			}
		}
		// How many copies one host may hold. One, for a service the agents run on one fixed host port
		// (`compile.rs`: a lost host leaves its copy unplaced and noted, never doubled up on a
		// survivor) and for one whose policy spreads (a lost host's copy waits for a host of its
		// own, which an operator adds). When it packs, up to `max_per_host` on its
		// home alone, which the control plane runs as one bigger services pod there, and one on any other host, past the home's room.
		let pack = service.policy.mode == PlaceMode::Pack && service.run.is_none();
		let cap = if pack { service.policy.max_per_host } else { 1 };
		// Packing starts at the database's own host (the project's home) while it has room.
		let cap_at = |host: &str| if database.host == Some(host) { cap } else { 1 };
		let home = database
			.host
			.filter(|h| pack && !report_only.contains(h) && eligible.iter().any(|e| e.name == *h));
		// (host, version, copies) kept off the home only until the home's copies run steady.
		let mut moving_home: Vec<(&str, &str, u32)> = Vec::new();
		// (host, version, copies) running where there is no room for them: placed elsewhere, and held
		// where they run, their room never counted, until the copies elsewhere run steady.
		let mut crowded: Vec<(&str, &str, u32)> = Vec::new();
		// host -> version -> copies, for this service.
		let mut chosen: BTreeMap<&str, BTreeMap<&str, u32>> = BTreeMap::new();
		let mut kept_of = Vec::with_capacity(targets.len());
		for (version, target) in &targets {
			// Keep what runs, on hosts still eligible, up to the target.
			let mut existing: Vec<(&str, u32)> = eligible
				.iter()
				.map(|h| {
					let n = copies
						.on(h.name.as_str(), service)
						.iter()
						.filter(|c| c.version == *version && c.state != CopyState::GaveUp)
						.count() as u32;
					(h.name.as_str(), n)
				})
				.filter(|(_, n)| *n > 0)
				.collect();
			// What runs on a report-only host first: it cannot be moved, so it is the one kept. Then,
			// packing, what runs on the home.
			// A finishing host's copy is kept last: a replacement already elsewhere is kept first, and
			// the finishing copy is then held only by the make-before-break below, until it runs steady.
			let held = |h: &str| report_only.contains(h) && !finishing.contains(h);
			existing.sort_by(|a, b| {
				held(b.0)
					.cmp(&held(a.0))
					.then(finishing.contains(a.0).cmp(&finishing.contains(b.0)))
					.then((home == Some(b.0)).cmp(&(home == Some(a.0))))
					.then(b.1.cmp(&a.1))
					.then(a.0.cmp(b.0))
			});
			let mut kept = 0;
			// Copies the home is holding room for, from hosts they move off.
			let mut reserved = 0;
			let ran: Vec<(&str, u32)> = existing.clone();
			for (host, n) in existing {
				let room = cap_at(host).saturating_sub(on_host(&chosen, host));
				let wanted_here = n.min(target - kept).min(room);
				// Each copy kept takes its own room: a host holds no more running copies than fit,
				// and the rest are placed elsewhere and held here until those run steady (`crowded`).
				let mut left = free[host];
				let mut take = 0;
				while take < wanted_here && left.fits(service.per_copy) {
					left = left.minus(service.per_copy);
					take += 1;
				}
				if take < wanted_here {
					crowded.push((host, *version, wanted_here - take));
				}
				if let Some(home) = home
					&& host != home && !report_only.contains(host)
				{
					// A copy off the home moves there while the home has room: placed there, and kept
					// here until that one runs steady (make before break, below).
					let mut at_home = free[home];
					for _ in 0..reserved {
						at_home = at_home.minus(service.per_copy);
					}
					let mut moving = 0;
					while moving < take
						&& on_host(&chosen, home) + reserved + moving < cap
						&& at_home.fits(service.per_copy)
					{
						at_home = at_home.minus(service.per_copy);
						moving += 1;
					}
					if moving > 0 {
						take -= moving;
						reserved += moving;
					}
				}
				for _ in 0..take {
					free.insert(host, free[host].minus(service.per_copy));
				}
				if take > 0 {
					*chosen.entry(host).or_default().entry(version).or_default() += take;
				}
				kept += take;
				if kept == *target {
					break;
				}
			}
			// Packing: what runs off the home and is not kept moves to it, and stays where it runs until
			// the home's copies run steady (below).
			if let Some(home) = home {
				for (host, n) in ran {
					let kept_there = chosen
						.get(host)
						.and_then(|v| v.get(*version))
						.copied()
						.unwrap_or(0);
					if host != home && !report_only.contains(host) && n > kept_there {
						moving_home.push((host, *version, n - kept_there));
					}
				}
			}
			kept_of.push(kept);
		}
		plans.push_back(ServicePlan {
			service,
			id,
			said,
			targets,
			kept: kept_of,
			eligible,
			cap,
			pack,
			moving_home,
			chosen,
			holding,
			crowded,
			prefer,
			off: off.keys().copied().collect(),
			gave_up,
		});
	}

	// environment -> the hosts its services so far hold: a project's later services go there first, so
	// its copies name one set of hosts (otherwise auth and its router on different
	// hosts would name more hosts than copies, which the control plane refuses).
	let mut env_hosts: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
	// Environments that still hold a copy on a host they move off (a move not done yet).
	let mut still_moving: BTreeSet<(&str, &str)> = BTreeSet::new();
	let mut shortfalls: Vec<Shortfall> = Vec::new();
	while let Some(plan) = plans.pop_front() {
		let ServicePlan {
			service,
			id,
			said,
			targets,
			kept,
			eligible,
			cap,
			pack,
			moving_home,
			mut chosen,
			holding,
			crowded,
			prefer,
			gave_up,
			off,
		} = plan;
		notes.extend(said);
		// Never a new copy on a host under pressure.
		let addable: Vec<&HostSpec> = eligible
			.iter()
			.copied()
			.filter(|h| {
				!report_only.contains(h.name.as_str()) && !pressure.hosts.contains(h.name.as_str())
			})
			.collect();
		let database = database_of(spec, facts, service);
		// (host, version) of each copy this decision adds.
		let mut added: Vec<(&str, &str)> = Vec::new();
		for ((version, target), kept) in targets.iter().zip(kept) {
			// Add what is missing.
			let mut placed = kept;
			while placed < *target {
				let with = env_hosts.get(service.environment.as_str());
				let host = best_host(
					&addable, &chosen, &free, service, database, cap, pack, with, prefer,
				);
				let Some(host) = host else {
					break;
				};
				free.insert(host, free[host].minus(service.per_copy));
				*chosen.entry(host).or_default().entry(version).or_default() += 1;
				notes.push(format!("{id}: a copy of {version} placed on {host}"));
				added.push((host, version));
				placed += 1;
			}
			if placed < *target {
				let refused = refusals(
					spec,
					service,
					&Refusing {
						alive: &alive,
						report_only: &report_only,
						pressured: &pressure.hosts,
						gave_up: &gave_up,
						off: &off,
						database: database.host,
						cap,
					},
					&chosen,
					&free,
				);
				let why = if eligible.is_empty() {
					"no eligible host is reporting"
				} else if addable.is_empty() {
					"every eligible host only reports"
				} else {
					"no eligible host has room"
				};
				notes.push(format!(
					"{id}: wanted {target} copies of {version}, placed {placed}: {why}{}",
					refusal_summary(&refused, service.per_copy)
				));
				shortfalls.push(Shortfall {
					service: id.clone(),
					version: version.to_string(),
					wanted: *target,
					placed,
					needs: service.per_copy,
					hosts: refused,
				});
			}
		}
		env_hosts
			.entry(service.environment.as_str())
			.or_default()
			.extend(chosen.keys().copied());
		// A copy moving to the home stays placed, and so routed, where it runs until the home's copies
		// of its version all run steady (make before break, as a draining host's).
		if let Some(home) = database.host {
			for (host, version, n) in moving_home {
				let at_home = chosen
					.get(home)
					.and_then(|v| v.get(version))
					.copied()
					.unwrap_or(0);
				let steady_at_home = copies
					.on(home, service)
					.iter()
					.filter(|c| {
						c.version == version
							&& c.state == CopyState::Running
							&& c.running_since_ms.is_some_and(|since| {
								facts.now_ms.saturating_sub(since) >= STEADY_MS
							})
					})
					.count() as u32;
				if steady_at_home >= at_home {
					notes.push(format!(
						"{id}: {host}'s copy of {version} moved to its home, {home}"
					));
					continue;
				}
				for _ in 0..n {
					free.insert(host, free[host].minus(service.per_copy));
				}
				*chosen.entry(host).or_default().entry(version).or_default() += n;
				notes.push(format!(
					"{id}: {host} keeps its copy of {version} until the one on its home, {home}, runs steady"
				));
			}
		}
		// A version change never takes a region's copies down in one decision (swapping both
		// copies in a region at once leaves the far copy alone answering until they run). A running
		// copy that a decision adding copies would retire, on a host that stays, goes only when
		// nothing placed is still on its way up, and `max_unavailable` hosts at a time (one, without a
		// rollout); the rest keep their version, routed at the share of the version replacing it,
		// until a later decision. With a rollout, a copy that is only surplus (its replacement
		// already elsewhere, the surge) goes the same way.
		let rollout = service.rollout;
		let max_unavailable = rollout.map_or(1, |r| r.max_unavailable) as usize;
		let max_surge = rollout.map_or(0, |r| r.max_surge);
		let running_of = |host: &str, version: &str| -> u32 {
			copies
				.on(host, service)
				.iter()
				.filter(|c| c.version == version && c.state == CopyState::Running)
				.count() as u32
		};
		// A copy running where its room is taken stays placed, and so routed, until the copies placed
		// for the service run steady: make before break, so no copy is stopped for want of room. Held
		// here, before a roll counts what goes, so a roll never lets it go early either.
		{
			let wanted: u32 = targets.iter().map(|(_, t)| *t).sum();
			let steady: u32 = chosen
				.iter()
				.map(|(host, versions)| {
					versions
						.iter()
						.map(|(version, n)| {
							(copies
								.on(host, service)
								.iter()
								.filter(|c| {
									c.version == *version
										&& c.state == CopyState::Running
										&& c.running_since_ms.is_some_and(|since| {
											facts.now_ms.saturating_sub(since) >= STEADY_MS
										})
								})
								.count() as u32)
								.min(*n)
						})
						.sum::<u32>()
				})
				.sum();
			for (host, version, n) in crowded {
				if steady >= wanted {
					notes.push(format!(
						"{id}: {host}'s copy of {version} moved off it, since its room there is taken"
					));
					continue;
				}
				*chosen.entry(host).or_default().entry(version).or_default() += n;
				still_moving.insert((service.environment.as_str(), host));
				notes.push(format!(
					"{id}: {host} has no room for its copy of {version}; it stays until the copies elsewhere run steady"
				));
			}
		}
		let mut retiring: Vec<(&str, &str, u32)> = Vec::new();
		if !added.is_empty() || rollout.is_some() {
			let mut hosts: Vec<&str> = eligible
				.iter()
				.map(|h| h.name.as_str())
				.filter(|h| !report_only.contains(h))
				.collect();
			hosts.sort();
			for host in hosts {
				let versions: BTreeSet<&str> = copies
					.on(host, service)
					.iter()
					.filter(|c| c.state == CopyState::Running)
					.map(|c| c.version.as_str())
					.collect();
				for version in versions {
					let kept = chosen
						.get(host)
						.and_then(|v| v.get(version))
						.copied()
						.unwrap_or(0);
					let gone = running_of(host, version).saturating_sub(kept);
					if gone > 0 {
						retiring.push((host, version, gone));
					}
				}
			}
		}
		let changing: BTreeSet<&str> = retiring.iter().map(|(h, _, _)| *h).collect();
		let on_the_way = chosen
			.iter()
			.filter(|(host, versions)| {
				!changing.contains(*host)
					&& versions
						.iter()
						.any(|(version, n)| running_of(host, version) < *n)
			})
			.count();
		let budget = if on_the_way > 0 {
			max_unavailable.saturating_sub(on_the_way)
		} else {
			max_unavailable
		};
		let wanted: u32 = targets.iter().map(|(_, t)| *t).sum();
		let mut let_go: BTreeSet<&str> = BTreeSet::new();
		// Copies of the new version moved to a host holding none of the service (the surge).
		let mut surged: Vec<(&str, &str)> = Vec::new();
		for (host, old, gone) in retiring {
			if let_go.contains(host) || let_go.len() < budget {
				let_go.insert(host);
				continue;
			}
			let mut replacing = None;
			added.retain(|(h, version)| {
				if *h != host {
					return true;
				}
				let versions = chosen.get_mut(host).expect("added there");
				let n = versions.get_mut(version).expect("added there");
				*n -= 1;
				if *n == 0 {
					versions.remove(version);
				}
				free.insert(host, free[host].plus(service.per_copy));
				if let Some(i) = notes
					.iter()
					.position(|n| *n == format!("{id}: a copy of {version} placed on {host}"))
				{
					notes.remove(i);
				}
				replacing.get_or_insert(*version);
				false
			});
			for _ in 0..gone {
				free.insert(host, free[host].minus(service.per_copy));
			}
			*chosen.entry(host).or_default().entry(old).or_default() += gone;
			let effective = &shares[&id];
			let share_of = |version: &str| {
				effective
					.iter()
					.find(|v| v.version == version)
					.map_or(0, |v| v.share)
			};
			held.insert(
				(id.clone(), host.to_string(), old.to_string()),
				share_of(old).max(replacing.map_or(0, share_of)),
			);
			notes.push(format!(
				"{id}: {host} keeps {old} until the copy changing before it runs"
			));
			// The surge: the copy that would have replaced it here goes to a host that holds none of
			// the service, beside it, while the service stays within `max_surge` of its copies.
			if let Some(version) = replacing {
				let total: u32 = chosen.values().flat_map(|v| v.values()).sum();
				if total < wanted + max_surge {
					let fresh: Vec<&HostSpec> = addable
						.iter()
						.copied()
						.filter(|h| on_host(&chosen, h.name.as_str()) == 0)
						.collect();
					let with = env_hosts.get(service.environment.as_str());
					if let Some(to) = best_host(
						&fresh, &chosen, &free, service, database, cap, pack, with, None,
					) {
						free.insert(to, free[to].minus(service.per_copy));
						*chosen.entry(to).or_default().entry(version).or_default() += 1;
						surged.push((to, version));
					}
				}
			}
		}
		for (to, version) in surged {
			notes.push(format!(
				"{id}: a copy of {version} placed on {to}, beside the old ones (surge)"
			));
		}
		// A draining host (packing) takes no copy, since it does not serve, and the copy it
		// runs stays placed, and so routed, until the copies placed for the service elsewhere all run
		// steady: make before break, so a move never takes a service below its copies. A host a copy
		// moves off for pressure or the rebalancer holds it the same way.
		let steady_elsewhere: u32 = chosen
			.iter()
			.map(|(host, versions)| {
				versions
					.iter()
					.map(|(version, n)| {
						let steady = copies
							.on(host, service)
							.iter()
							.filter(|c| {
								c.version == *version
									&& c.state == CopyState::Running
									&& c.running_since_ms.is_some_and(|since| {
										facts.now_ms.saturating_sub(since) >= STEADY_MS
									})
							})
							.count() as u32;
						steady.min(*n)
					})
					.sum::<u32>()
			})
			.sum();
		if steady_elsewhere < wanted {
			for host in spec.hosts.iter().filter(|h| {
				(h.draining || finishing.contains(h.name.as_str()))
					&& alive.contains(h.name.as_str())
			}) {
				let versions: BTreeSet<&str> = copies
					.on(host.name.as_str(), service)
					.iter()
					.filter(|c| c.state == CopyState::Running)
					.map(|c| c.version.as_str())
					.collect();
				for version in versions {
					*chosen
						.entry(host.name.as_str())
						.or_default()
						.entry(version)
						.or_default() += 1;
					notes.push(format!(
						"{id}: {} keeps its copy of {version} while it drains, until the copies elsewhere run steady",
						host.name
					));
				}
			}
		}
		for (host, version, n) in holding {
			if steady_elsewhere >= wanted {
				for _ in 0..n {
					free.insert(host, free[host].plus(service.per_copy));
				}
				notes.push(format!("{id}: {host}'s copy of {version} moved off it"));
				continue;
			}
			*chosen.entry(host).or_default().entry(version).or_default() += n;
			still_moving.insert((service.environment.as_str(), host));
			notes.push(format!(
				"{id}: {host} keeps its copy of {version} until the copies elsewhere run steady"
			));
		}
		for (host, versions) in chosen {
			for (version, count) in versions {
				placement.push(Assignment {
					host: host.to_string(),
					environment: service.environment.clone(),
					service: service.name.clone(),
					version: version.to_string(),
					count,
				});
			}
		}
	}
	placement.sort();

	// A move in flight stays until no copy of its environment is held on `from`; then a new one may
	// start, from this decision's placement, once everything runs steady.
	let mut next_moves: Vec<Move> = moves
		.iter()
		.filter(|m| still_moving.contains(&(m.environment.as_str(), m.from.as_str())))
		.cloned()
		.collect();
	if next_moves.is_empty()
		&& let Some(band) = spec.rebalance
		&& let Some((m, why)) = pick_move(
			spec,
			facts,
			&alive,
			&report_only,
			&free,
			&placement,
			&copies,
			&rollbacks,
			band,
		) {
		notes.push(why);
		next_moves.push(m);
	}

	let table = routing_table(spec, facts, &alive, &placement, &held, &copies, &shares);
	Outcome {
		placement,
		table,
		notes,
		heard: alive.iter().map(|h| h.to_string()).collect(),
		moves: next_moves,
		rollbacks,
		shortfalls,
	}
}

/// What a decision knows about one service when it says why each host refused a copy.
struct Refusing<'a, 'b> {
	alive: &'b BTreeSet<&'a str>,
	report_only: &'b BTreeSet<&'a str>,
	pressured: &'b BTreeSet<&'a str>,
	gave_up: &'b BTreeSet<&'a str>,
	off: &'b BTreeSet<&'a str>,
	database: Option<&'a str>,
	cap: u32,
}

/// Every host of the spec and why it took no more copies of `service`, in the order of
/// [`Refused`]: the same tests, in the same order, as the two passes of [`decide`]. A host that
/// would still take one (none, when `best_host` found nothing) is left out.
fn refusals(
	spec: &Spec,
	service: &ServiceSpec,
	why: &Refusing,
	chosen: &BTreeMap<&str, BTreeMap<&str, u32>>,
	free: &BTreeMap<&str, Resources>,
) -> Vec<Refusal> {
	spec.hosts
		.iter()
		.filter_map(|h| {
			let name = h.name.as_str();
			let on_database = why.database == Some(name);
			let cap = if on_database { why.cap } else { 1 };
			let left = free[name];
			let reason = if h.dedicated_to != service.dedicated_to {
				if h.dedicated_to.is_some() {
					Refused::Dedicated
				} else {
					Refused::Shared
				}
			} else if !service.placement.allows(h) {
				Refused::NotOnItsList
			} else if !why.alive.contains(name) {
				Refused::NotReporting
			} else if !(h.serves || h.frozen || (h.serves_home && !h.draining && on_database)) {
				if h.draining {
					Refused::Draining
				} else {
					Refused::NotServing
				}
			} else if why.gave_up.contains(name) {
				Refused::GaveUp
			} else if why.off.contains(name) {
				Refused::MovingOff
			} else if h.frozen {
				Refused::Frozen
			} else if why.report_only.contains(name) {
				Refused::OnlyReports
			} else if why.pressured.contains(name) {
				Refused::UnderPressure
			} else if on_host(chosen, name) >= cap {
				Refused::HoldsItsShare
			} else if !left.fits(service.per_copy) {
				Refused::Full
			} else {
				return None;
			};
			Some(Refusal {
				host: h.name.clone(),
				reason,
				dedicated_to: h.dedicated_to.clone().or_else(|| h.owner.clone()),
				capacity: h.capacity,
				free: left,
				server: h.server,
			})
		})
		.collect()
}

/// The refusals as a clause for the note, by reason: ` (2 full: h1 256 of 4096 MB free, h2 …; 1
/// not on its list of hosts: h3)`. Five hosts named a reason, then how many more.
fn refusal_summary(refused: &[Refusal], needs: Resources) -> String {
	if refused.is_empty() {
		return String::new();
	}
	let mut by: BTreeMap<Refused, Vec<&Refusal>> = BTreeMap::new();
	for r in refused {
		by.entry(r.reason).or_default().push(r);
	}
	let parts: Vec<String> = by
		.iter()
		.map(|(reason, hosts)| {
			let mut named: Vec<String> = hosts
				.iter()
				.take(5)
				.map(|r| match r.reason {
					Refused::Full if r.free.memory_mb < needs.memory_mb => match r.server {
						Some(server) if server.memory_mb > r.capacity.memory_mb => format!(
							"{} {} of {} MB free, of a {} MB server",
							r.host, r.free.memory_mb, r.capacity.memory_mb, server.memory_mb
						),
						_ => format!(
							"{} {} of {} MB free",
							r.host, r.free.memory_mb, r.capacity.memory_mb
						),
					},
					Refused::Full => format!(
						"{} {} of {} millicpu free",
						r.host, r.free.millicpu, r.capacity.millicpu
					),
					// Whose it is matters where that is the reason; a host off the service's list is
					// someone's pool or another region, and its owner would only be noise.
					Refused::Dedicated => match &r.dedicated_to {
						Some(owner) => format!("{} ({owner})", r.host),
						None => r.host.clone(),
					},
					_ => r.host.clone(),
				})
				.collect();
			if hosts.len() > 5 {
				named.push(format!("{} more", hosts.len() - 5));
			}
			format!("{} {}: {}", hosts.len(), reason.words(), named.join(", "))
		})
		.collect();
	format!(" ({})", parts.join("; "))
}

/// One service between the two passes of [`decide`]: what it wants, and the copies it keeps.
struct ServicePlan<'a> {
	service: &'a ServiceSpec,
	id: String,
	/// Its notes so far (load, gave up), said before the copies it adds.
	said: Vec<String>,
	targets: Vec<(&'a str, u32)>,
	/// Copies kept, by the index of `targets`.
	kept: Vec<u32>,
	eligible: Vec<&'a HostSpec>,
	cap: u32,
	/// Its policy packs: the home first, up to `cap` copies there, one on any other host.
	pack: bool,
	/// Copies kept off the home only until the home's copies run steady.
	moving_home: Vec<(&'a str, &'a str, u32)>,
	chosen: BTreeMap<&'a str, BTreeMap<&'a str, u32>>,
	/// (host, version, copies) running on a host it moves off (pressure, the rebalancer): kept there,
	/// their room held, until the copies elsewhere run steady.
	holding: Vec<(&'a str, &'a str, u32)>,
	/// (host, version, copies) running where no room is left for them: held there, their room not
	/// counted, until the copies elsewhere run steady.
	crowded: Vec<(&'a str, &'a str, u32)>,
	/// The host its new copies go to first: the rebalancer's `to`.
	prefer: Option<&'a str>,
	/// Hosts whose agent gave up on it, and hosts it moves off: kept to say why a host refused.
	gave_up: BTreeSet<&'a str>,
	off: BTreeSet<&'a str>,
}

/// The versions a service is placed and routed at: the spec's, with a rolled-back version's share
/// given to the version it rolled back to.
fn effective_versions(service: &ServiceSpec, rollback: Option<&Rollback>) -> Vec<VersionShare> {
	let Some(rollback) = rollback else {
		return service.versions.clone();
	};
	let moved: u32 = service
		.versions
		.iter()
		.filter(|v| v.version == rollback.from)
		.map(|v| v.share)
		.sum();
	let mut out: Vec<VersionShare> = service
		.versions
		.iter()
		.filter(|v| v.version != rollback.from)
		.cloned()
		.collect();
	match out.iter_mut().find(|v| v.version == rollback.to) {
		Some(v) => v.share += moved,
		None => out.push(VersionShare {
			version: rollback.to.clone(),
			share: moved,
		}),
	}
	out
}

/// The rollbacks in force: the last decision's (`Facts::rollbacks`) while the service still rolls
/// back and its spec still names the version that gave up with a share, and a new one when an agent
/// gave up on a copy of a version with a share while another version still runs (the most running
/// copies, then by name). A spec that changes the version ends it.
fn rollbacks(
	spec: &Spec,
	facts: &Facts,
	alive: &BTreeSet<&str>,
	copies: &CopyIndex,
	notes: &mut Vec<String>,
) -> BTreeMap<String, Rollback> {
	let mut out = BTreeMap::new();
	for service in &spec.services {
		if !service.rollout.is_some_and(|r| r.rollback) {
			continue;
		}
		let id = service.id();
		let named = |version: &str| {
			service
				.versions
				.iter()
				.any(|v| v.version == version && v.share > 0)
		};
		if let Some(last) = facts.rollbacks.get(&id)
			&& named(&last.from)
		{
			out.insert(id, last.clone());
			continue;
		}
		let on_alive: Vec<&CopyFacts> = copies
			.hosts_of(service)
			.iter()
			.filter(|(h, _)| alive.contains(h))
			.map(|(_, c)| *c)
			.collect();
		let failed = service.versions.iter().find(|v| {
			v.share > 0
				&& on_alive
					.iter()
					.any(|c| c.version == v.version && c.state == CopyState::GaveUp)
		});
		let Some(failed) = failed else {
			continue;
		};
		// Only back to an older version: one the spec no longer gives a share, or one it lists before
		// the one that gave up (a canary lists the stable version first). A stable copy giving up beside
		// a canary is a copy given up, never a reason to roll forward.
		let at = |version: &str| service.versions.iter().position(|v| v.version == version);
		let older = |version: &str| match (at(version), at(&failed.version)) {
			(Some(i), Some(f)) => !named(version) || i < f,
			_ => true,
		};
		let mut running: BTreeMap<&str, u32> = BTreeMap::new();
		for c in &on_alive {
			if c.state == CopyState::Running && c.version != failed.version && older(&c.version) {
				*running.entry(c.version.as_str()).or_default() += 1;
			}
		}
		let back = running
			.iter()
			.max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
			.map(|(v, _)| v.to_string());
		if let Some(to) = back {
			notes.push(format!(
				"{id}: an agent gave up on {}; rolled back to {to} until the spec changes its version",
				failed.version
			));
			out.insert(
				id,
				Rollback {
					from: failed.version.clone(),
					to,
				},
			);
		}
	}
	out
}

/// What pressure sheds: the hosts under it, and by host the service ids moved off it.
struct Pressure<'a> {
	hosts: BTreeSet<&'a str>,
	shed: BTreeMap<&'a str, BTreeSet<String>>,
}

/// Hosts whose running copies use more than `pressure_percent` of their capacity (CPU or memory, a
/// dimension with no capacity left out), and on each, the services moved off it until what stays fits:
/// best effort first, then burstable by how far past its request it runs, the lowest priority first
/// within a class, never a guaranteed or one-clock copy. A host that only reports, frozen or draining
/// is left as it is.
fn pressure<'a>(
	spec: &'a Spec,
	facts: &'a Facts,
	alive: &BTreeSet<&str>,
	report_only: &BTreeSet<&str>,
	notes: &mut Vec<String>,
) -> Pressure<'a> {
	let mut out = Pressure {
		hosts: BTreeSet::new(),
		shed: BTreeMap::new(),
	};
	let Some(percent) = spec.pressure_percent else {
		return out;
	};
	let of = |n: u32| (u64::from(n) * u64::from(percent) / 100) as u32;
	for host in &spec.hosts {
		let name = host.name.as_str();
		if !alive.contains(name) || report_only.contains(name) || host.draining {
			continue;
		}
		let Some(reported) = facts.hosts.iter().find(|h| h.name == host.name) else {
			continue;
		};
		let limit = Resources {
			millicpu: of(host.capacity.millicpu),
			memory_mb: of(host.capacity.memory_mb),
		};
		// (service, its copies' use here) for each spec service running here.
		let mut by_service: BTreeMap<String, (&ServiceSpec, Resources)> = BTreeMap::new();
		let mut used = Resources::default();
		for c in &reported.copies {
			let (Some(load), CopyState::Running) = (c.load, c.state) else {
				continue;
			};
			used = used.plus(load);
			if let Some(s) = spec
				.services
				.iter()
				.find(|s| s.environment == c.environment && s.name == c.service)
			{
				let entry = by_service
					.entry(s.id())
					.or_insert((s, Resources::default()));
				entry.1 = entry.1.plus(load);
			}
		}
		let over = |used: Resources| {
			(host.capacity.millicpu > 0 && used.millicpu > limit.millicpu)
				|| (host.capacity.memory_mb > 0 && used.memory_mb > limit.memory_mb)
		};
		if !over(used) {
			continue;
		}
		out.hosts.insert(name);
		let mut candidates: Vec<(String, &ServiceSpec, Resources)> = by_service
			.into_iter()
			.filter(|(_, (s, _))| s.qos() != Qos::Guaranteed && !s.one_clock)
			.map(|(id, (s, load))| (id, s, load))
			.collect();
		let past = |s: &ServiceSpec, load: Resources| {
			(
				load.memory_mb.saturating_sub(s.per_copy.memory_mb),
				load.millicpu.saturating_sub(s.per_copy.millicpu),
			)
		};
		candidates.sort_by(|a, b| {
			a.1.qos()
				.cmp(&b.1.qos())
				.then(a.1.priority.cmp(&b.1.priority))
				.then(past(b.1, b.2).cmp(&past(a.1, a.2)))
				.then(a.0.cmp(&b.0))
		});
		let mut left = used;
		let mut shed = BTreeSet::new();
		for (id, s, load) in candidates {
			if !over(left) {
				break;
			}
			left = left.minus(load);
			notes.push(format!(
				"{name}: its copies use more than {percent}% of what it has; {id} ({}) moves off it",
				match s.qos() {
					Qos::BestEffort => "best effort",
					Qos::Burstable => "burstable",
					Qos::Guaranteed => "guaranteed",
				}
			));
			shed.insert(id);
		}
		if over(left) {
			notes.push(format!(
				"{name}: still past {percent}% with only guaranteed copies left; nothing more moves"
			));
		}
		if !shed.is_empty() {
			out.shed.insert(name, shed);
		}
	}
	out
}

/// The last decision's move, while the spec still rebalances and both hosts can take part: `from`
/// heard, `to` heard, serving and not held.
fn live_moves<'a>(
	spec: &'a Spec,
	alive: &BTreeSet<&str>,
	report_only: &BTreeSet<&str>,
	facts: &'a Facts,
	notes: &mut Vec<String>,
) -> Vec<Move> {
	if spec.rebalance.is_none() {
		return Vec::new();
	}
	let mut out = Vec::new();
	for m in &facts.moves {
		let to = spec.hosts.iter().find(|h| h.name == m.to);
		let ok = alive.contains(m.from.as_str())
			&& alive.contains(m.to.as_str())
			&& !report_only.contains(m.from.as_str())
			&& to.is_some_and(|h| h.serves && !h.draining && !report_only.contains(m.to.as_str()));
		if ok {
			out.push(m.clone());
		} else {
			notes.push(format!(
				"rebalance: {}'s move from {} to {} is no longer possible; given up",
				m.environment, m.from, m.to
			));
		}
	}
	out
}

/// A host's share of its capacity that its placed copies reserve, in percent: the busier of CPU and
/// memory, a dimension it has none of left out.
fn reserved_percent(capacity: Resources, free: Resources) -> u64 {
	let of = |cap: u32, left: u32| {
		if cap == 0 {
			0
		} else {
			u64::from(cap.saturating_sub(left)) * 100 / u64::from(cap)
		}
	};
	of(capacity.millicpu, free.millicpu).max(of(capacity.memory_mb, free.memory_mb))
}

/// The rebalancer's next move, once every placed copy runs steady and nothing else moves: an
/// environment's copies on the most reserved host past `high_percent` go to the least reserved one
/// under `low_percent` that can take all of them (every service's own rules, one copy of each per
/// host, a fault domain no fuller than the one they leave, never the environment's database host),
/// and only when it leaves both hosts below where `from` was. One environment per decision.
#[allow(clippy::too_many_arguments)]
fn pick_move(
	spec: &Spec,
	facts: &Facts,
	alive: &BTreeSet<&str>,
	report_only: &BTreeSet<&str>,
	free: &BTreeMap<&str, Resources>,
	placement: &[Assignment],
	copies: &CopyIndex,
	rollbacks: &BTreeMap<String, Rollback>,
	band: Rebalance,
) -> Option<(Move, String)> {
	// Calm: every placed copy runs steady, and nothing runs that is not placed.
	let steady = |host: &str, env: &str, name: &str, version: &str| -> u32 {
		copies
			.by_host
			.get(host)
			.and_then(|e| e.get(env))
			.and_then(|s| s.get(name))
			.map_or(0, |cs| {
				cs.iter()
					.filter(|c| {
						c.version == version
							&& c.state == CopyState::Running
							&& c.running_since_ms
								.is_some_and(|s| facts.now_ms.saturating_sub(s) >= STEADY_MS)
					})
					.count() as u32
			})
	};
	for a in placement {
		if steady(&a.host, &a.environment, &a.service, &a.version) < a.count {
			return None;
		}
	}
	for host in facts
		.hosts
		.iter()
		.filter(|h| alive.contains(h.name.as_str()))
	{
		for c in &host.copies {
			let placed = placement.iter().any(|a| {
				a.host == host.name
					&& a.environment == c.environment
					&& a.service == c.service
					&& a.version == c.version
			});
			let known = spec
				.services
				.iter()
				.any(|s| s.environment == c.environment && s.name == c.service);
			if c.state == CopyState::Starting || (known && !placed) {
				return None;
			}
		}
	}
	if spec
		.hosts
		.iter()
		.any(|h| (h.draining || h.finishing) && alive.contains(h.name.as_str()))
		|| !rollbacks.is_empty()
	{
		return None;
	}
	let open: Vec<&HostSpec> =
		spec.hosts
			.iter()
			.filter(|h| {
				h.serves
					&& !h.draining && alive.contains(h.name.as_str())
					&& !report_only.contains(h.name.as_str())
			})
			.collect();
	let percent = |h: &HostSpec| reserved_percent(h.capacity, free[h.name.as_str()]);
	let mut busy: Vec<&HostSpec> = open
		.iter()
		.copied()
		.filter(|h| percent(h) > u64::from(band.high_percent))
		.collect();
	busy.sort_by(|a, b| percent(b).cmp(&percent(a)).then(a.name.cmp(&b.name)));
	let mut quiet: Vec<&HostSpec> = open
		.iter()
		.copied()
		.filter(|h| percent(h) < u64::from(band.low_percent))
		.collect();
	quiet.sort_by(|a, b| percent(a).cmp(&percent(b)).then(a.name.cmp(&b.name)));
	let service_of = |env: &str, name: &str| {
		spec.services
			.iter()
			.find(|s| s.environment == env && s.name == name)
	};
	for from in &busy {
		let mut envs: BTreeMap<&str, Vec<&Assignment>> = BTreeMap::new();
		for a in placement.iter().filter(|a| a.host == from.name) {
			envs.entry(a.environment.as_str()).or_default().push(a);
		}
		// (resulting worst percent, `to` after, environment, to)
		let mut best: Option<(u64, u64, &str, &HostSpec)> = None;
		for (env, here) in &envs {
			let services: Option<Vec<&ServiceSpec>> =
				here.iter().map(|a| service_of(env, &a.service)).collect();
			let Some(services) = services else {
				continue;
			};
			if services.iter().any(|s| {
				s.one_clock
					|| s.policy.mode == PlaceMode::Pack
					|| shares_versions(s, placement)
					|| rollbacks.contains_key(&s.id())
			}) {
				continue;
			}
			let mut need = Resources::default();
			for (a, s) in here.iter().zip(&services) {
				for _ in 0..a.count {
					need = need.plus(s.per_copy);
				}
			}
			let env_hosts: BTreeSet<&str> = placement
				.iter()
				.filter(|a| a.environment == *env)
				.map(|a| a.host.as_str())
				.collect();
			let domain_count = |domain: &str| {
				env_hosts
					.iter()
					.filter(|h| **h != from.name)
					.filter_map(|h| spec.hosts.iter().find(|s| s.name == *h))
					.filter(|h| h.fault_domain == domain)
					.count()
			};
			let database = facts.database_hosts.get(*env).map(String::as_str);
			for to in &quiet {
				let ok = to.name != from.name
					&& !env_hosts.contains(to.name.as_str())
					&& database != Some(to.name.as_str())
					&& to.dedicated_to == from.dedicated_to
					&& domain_count(&to.fault_domain) <= domain_count(&from.fault_domain)
					&& free[to.name.as_str()].fits(need)
					&& services.iter().all(|s| {
						s.dedicated_to == to.dedicated_to
							&& s.placement.allows(to)
							&& !copies
								.on(to.name.as_str(), s)
								.iter()
								.any(|c| c.state == CopyState::GaveUp)
					});
				if !ok {
					continue;
				}
				let after_from =
					reserved_percent(from.capacity, free[from.name.as_str()].plus(need));
				let after_to = reserved_percent(to.capacity, free[to.name.as_str()].minus(need));
				let worst = after_from.max(after_to);
				if worst >= percent(from) {
					continue;
				}
				if best.is_none_or(|(w, a, e, t)| {
					(worst, after_to, *env, &to.name) < (w, a, e, &t.name)
				}) {
					best = Some((worst, after_to, env, to));
				}
			}
		}
		if let Some((_, _, env, to)) = best {
			let why = format!(
				"rebalance: {env}'s copies on {} ({}% reserved) move to {} ({}%)",
				from.name,
				percent(from),
				to.name,
				percent(to)
			);
			return Some((
				Move {
					environment: env.to_string(),
					from: from.name.clone(),
					to: to.name.clone(),
				},
				why,
			));
		}
	}
	None
}

/// The placement holds more than one version of the service (a roll in progress).
fn shares_versions(service: &ServiceSpec, placement: &[Assignment]) -> bool {
	let versions: BTreeSet<&str> = placement
		.iter()
		.filter(|a| a.environment == service.environment && a.service == service.name)
		.map(|a| a.version.as_str())
		.collect();
	versions.len() > 1
}

/// The reported copies by host and service, built once per decision: at fleet size a decision
/// holds hundreds of services on tens of hosts, and scanning every host's report for each of them
/// is what made it slow.
struct CopyIndex<'a> {
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

	fn hosts_of(&self, service: &ServiceSpec) -> &[(&'a str, &'a CopyFacts)] {
		self.by_service
			.get(service.environment.as_str())
			.and_then(|s| s.get(service.name.as_str()))
			.map_or(&[], Vec::as_slice)
	}

	fn on(&self, host: &str, service: &ServiceSpec) -> &[&'a CopyFacts] {
		self.by_host
			.get(host)
			.and_then(|e| e.get(service.environment.as_str()))
			.and_then(|s| s.get(service.name.as_str()))
			.map_or(&[], Vec::as_slice)
	}
}

/// Where a service's database is, as far as this decision knows: the host of its environment's
/// primary (`Facts::database_hosts`, a fleet project's) when the spec names that host, and the
/// region of it, or else the one primary region of the facts (the control plane's).
#[derive(Clone, Copy)]
struct Database<'a> {
	host: Option<&'a str>,
	region: Option<&'a str>,
}

fn database_of<'a>(spec: &'a Spec, facts: &'a Facts, service: &ServiceSpec) -> Database<'a> {
	let host = facts
		.database_hosts
		.get(&service.environment)
		.and_then(|name| spec.hosts.iter().find(|h| &h.name == name));
	match host {
		Some(h) => Database {
			host: Some(h.name.as_str()),
			region: Some(h.region.as_str()),
		},
		None => Database {
			host: None,
			region: facts.database_primary_region.as_deref(),
		},
	}
}

fn is_of(copy: &CopyFacts, service: &ServiceSpec) -> bool {
	copy.environment == service.environment && copy.service == service.name
}

/// How many copies a service wants now: `copies`, or with `scale` (`spec.rs`, [`crate::spec::Scale`])
/// what its load says, one step from what runs. Only from facts: every RUNNING copy on a live host
/// must report its load, and none may be starting (a copy just added has not taken its share yet),
/// or it holds what is there, within its limits. So a scale-up adds one copy and waits for it.
fn wanted_copies(
	service: &ServiceSpec,
	facts: &Facts,
	alive: &BTreeSet<&str>,
	notes: &mut Vec<String>,
) -> u32 {
	let Some(scale) = service.scale else {
		return service.copies;
	};
	let within = |n: u32| n.clamp(service.copies, scale.max_copies);
	let copies: Vec<&CopyFacts> = facts
		.hosts
		.iter()
		.filter(|h| alive.contains(h.name.as_str()))
		.flat_map(|h| h.copies.iter())
		.filter(|c| is_of(c, service) && c.state != CopyState::GaveUp)
		.collect();
	let running: Vec<&CopyFacts> = copies
		.iter()
		.copied()
		.filter(|c| c.state == CopyState::Running)
		.collect();
	let n = running.len() as u32;
	if n == 0 || running.len() < copies.len() {
		return within(copies.len() as u32);
	}
	let Some(used) = running.iter().try_fold(Resources::default(), |sum, c| {
		c.load.map(|l| Resources {
			millicpu: sum.millicpu.saturating_add(l.millicpu),
			memory_mb: sum.memory_mb.saturating_add(l.memory_mb),
		})
	}) else {
		return within(n);
	};
	let id = service.id();
	let now = busy_percent(used, service.per_copy, n);
	if now > u64::from(scale.up_percent) && n < scale.max_copies {
		notes.push(format!(
			"{id}: {n} copies at {now}% of what they reserve, above {}%: {} wanted (at most {})",
			scale.up_percent,
			n + 1,
			scale.max_copies
		));
		return n + 1;
	}
	if n > service.copies {
		let fewer = busy_percent(used, service.per_copy, n - 1);
		if fewer < u64::from(scale.down_percent) {
			notes.push(format!(
				"{id}: {n} copies at {now}%, {fewer}% on one fewer, below {}%: {} wanted (at least {})",
				scale.down_percent,
				n - 1,
				service.copies
			));
			return n - 1;
		}
	}
	within(n)
}

/// What `n` copies' reservations would be at with `used` spread over them, in percent, the busier
/// of CPU and memory (a dimension a copy reserves nothing of is left out).
fn busy_percent(used: Resources, per_copy: Resources, n: u32) -> u64 {
	let of = |used: u32, each: u32| {
		if each == 0 {
			0
		} else {
			u64::from(used) * 100 / (u64::from(each) * u64::from(n.max(1)))
		}
	};
	of(used.millicpu, per_copy.millicpu).max(of(used.memory_mb, per_copy.memory_mb))
}

/// The copies split by share, largest remainder first, ties to the version listed first. A version
/// with a share gets at least one copy when there are copies enough for every such version.
fn version_targets(versions: &[VersionShare], copies: u32) -> Vec<(&str, u32)> {
	let total: u32 = versions.iter().map(|v| v.share).sum();
	let shared: Vec<_> = versions.iter().filter(|v| v.share > 0).collect();
	let mut out: Vec<(&str, u32, u64)> = shared
		.iter()
		.map(|v| {
			let exact = u64::from(copies) * u64::from(v.share);
			(
				v.version.as_str(),
				(exact / u64::from(total)) as u32,
				exact % u64::from(total),
			)
		})
		.collect();
	if copies as usize >= out.len() {
		for entry in out.iter_mut() {
			if entry.1 == 0 {
				entry.1 = 1;
				entry.2 = 0;
			}
		}
	}
	let mut assigned: u32 = out.iter().map(|e| e.1).sum();
	while assigned > copies {
		// The minimums overshot: take back from the version with the most copies.
		let i = (0..out.len())
			.max_by_key(|&i| (out[i].1, std::cmp::Reverse(i)))
			.unwrap_or(0);
		out[i].1 -= 1;
		assigned -= 1;
	}
	let mut order: Vec<usize> = (0..out.len()).collect();
	order.sort_by(|&a, &b| out[b].2.cmp(&out[a].2).then(a.cmp(&b)));
	let mut k = 0;
	while assigned < copies && !order.is_empty() {
		out[order[k % order.len()]].1 += 1;
		assigned += 1;
		k += 1;
	}
	out.into_iter()
		.filter(|e| e.1 > 0)
		.map(|e| (e.0, e.1))
		.collect()
}

fn on_host(chosen: &BTreeMap<&str, BTreeMap<&str, u32>>, host: &str) -> u32 {
	chosen.get(host).map_or(0, |v| v.values().sum::<u32>())
}

#[allow(clippy::too_many_arguments)]
fn best_host<'a>(
	eligible: &[&'a HostSpec],
	chosen: &BTreeMap<&'a str, BTreeMap<&'a str, u32>>,
	free: &BTreeMap<&'a str, Resources>,
	service: &ServiceSpec,
	database: Database,
	cap: u32,
	pack: bool,
	with: Option<&BTreeSet<&'a str>>,
	prefer: Option<&str>,
) -> Option<&'a str> {
	// A host the project's earlier services hold.
	let shared = |host: &HostSpec| with.is_some_and(|w| w.contains(host.name.as_str()));
	let on = |host: &str| on_host(chosen, host);
	let in_domain = |domain: &str| {
		eligible
			.iter()
			.filter(|h| h.fault_domain == domain)
			.map(|h| on(&h.name))
			.sum::<u32>()
	};
	// Off the database's own host where another has room: a spread project's copies are there to
	// answer when that host is lost (MT14).
	let on_database = |host: &HostSpec| database.host == Some(host.name.as_str());
	// Several on one host only on a packing project's home (above); one anywhere else.
	let cap_of = |host: &HostSpec| if on_database(host) { cap } else { 1 };
	eligible
		.iter()
		.filter(|h| free[h.name.as_str()].fits(service.per_copy) && on(&h.name) < cap_of(h))
		.min_by(|a, b| {
			// Packing: the home first.
			let packed = if pack {
				on_database(b).cmp(&on_database(a))
			} else {
				std::cmp::Ordering::Equal
			};
			let preferred = |h: &HostSpec| prefer == Some(h.name.as_str());
			packed
				.then(preferred(b).cmp(&preferred(a)))
				.then(shared(b).cmp(&shared(a)))
				.then(on(&a.name).cmp(&on(&b.name)))
				.then(in_domain(&a.fault_domain).cmp(&in_domain(&b.fault_domain)))
				.then(on_database(a).cmp(&on_database(b)))
				.then(near(b, database.region).cmp(&near(a, database.region)))
				.then(
					free[b.name.as_str()]
						.memory_mb
						.cmp(&free[a.name.as_str()].memory_mb),
				)
				.then(a.name.cmp(&b.name))
		})
		.map(|h| h.name.as_str())
}

fn near(host: &HostSpec, primary_region: Option<&str>) -> bool {
	primary_region == Some(host.region.as_str())
}

fn is_zero(n: &u32) -> bool {
	*n == 0
}

/// Routes to the copies that are placed AND reported running on a live host, and names the ones
/// placed and not running yet (`starting`), which are never routed to. A service that is not
/// `routed` is in the table with no route, so its copies run and no call reaches them.
fn routing_table(
	spec: &Spec,
	facts: &Facts,
	alive: &BTreeSet<&str>,
	placement: &[Assignment],
	held: &BTreeMap<(String, String, String), u32>,
	copies: &CopyIndex,
	shares: &BTreeMap<String, Vec<VersionShare>>,
) -> BTreeMap<String, Vec<Route>> {
	let placed_at: BTreeMap<(&str, &str, &str, &str), u32> = placement
		.iter()
		.map(|a| {
			(
				(
					a.host.as_str(),
					a.environment.as_str(),
					a.service.as_str(),
					a.version.as_str(),
				),
				a.count,
			)
		})
		.collect();
	let live: Vec<&HostSpec> = spec
		.hosts
		.iter()
		.filter(|h| alive.contains(h.name.as_str()))
		.collect();
	let mut table: BTreeMap<String, Vec<Route>> = BTreeMap::new();
	for service in &spec.services {
		let mut routes = Vec::new();
		if !service.routed {
			table.insert(service.id(), routes);
			continue;
		}
		let id = service.id();
		let database = database_of(spec, facts, service);
		for host in &live {
			// Every live host reported (that is what makes it live).
			let here = copies.on(host.name.as_str(), service);
			// The spec's versions, and an old one this host keeps through a roll at its held share.
			let mut versions: BTreeMap<&str, u32> = shares[&id]
				.iter()
				.map(|v| (v.version.as_str(), v.share))
				.collect();
			for ((of, on, version), share) in held {
				if *of == id && *on == host.name {
					versions.insert(version.as_str(), *share);
				}
			}
			for (version, share) in versions {
				let placed = placed_at
					.get(&(
						host.name.as_str(),
						service.environment.as_str(),
						service.name.as_str(),
						version,
					))
					.copied()
					.unwrap_or(0);
				let running = here
					.iter()
					.filter(|c| c.version == version && c.state == CopyState::Running)
					.count() as u32;
				let routed = placed.min(running);
				// A copy waiting for another version of the service on the same host to stop (one
				// copy per host: the agent drains the old one first) is not starting: that old copy
				// answers the health on the same port, so a door waiting on it would take it for the
				// new one.
				let other_running = here
					.iter()
					.any(|c| c.version != version && c.state == CopyState::Running);
				let waiting = placed.saturating_sub(running);
				let (starting, replacing) = if other_running {
					(0, waiting)
				} else {
					(waiting, 0)
				};
				if routed > 0 || starting > 0 || replacing > 0 {
					routes.push(Route {
						host: host.name.clone(),
						region: host.region.clone(),
						version: version.to_string(),
						running: routed,
						starting,
						replacing,
						share,
						near_database: near(host, database.region),
					});
				}
			}
		}
		routes.sort();
		table.insert(id, routes);
	}
	table
}
