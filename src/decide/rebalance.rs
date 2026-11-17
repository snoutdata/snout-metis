//! The rebalancer (Kubernetes' descheduler): once everything runs steady, one project's copies on
//! the most reserved host move to the least reserved one that takes them all. Off unless the spec
//! sets the band. Its move in flight is carried by the facts until no copy is held on `from`.

use super::Assignment;
use super::note::Note;
use super::room::Room;
use super::world::World;
use crate::spec::{
	CopyState, HostSpec, Move, PlaceMode, Rebalance, Resources, Rollback, ServiceSpec,
};
use std::collections::{BTreeMap, BTreeSet};

/// The last decision's move, while the spec still rebalances and both hosts can take part: `from`
/// heard, `to` heard, serving and not held.
pub(super) fn live_moves<'a>(world: &World<'a>, notes: &mut Vec<Note>) -> Vec<&'a Move> {
	let (spec, facts, alive, report_only) =
		(world.spec, world.facts, &world.alive, &world.report_only);
	if spec.rebalance.is_none() {
		return Vec::new();
	}
	let mut out = Vec::new();
	for m in &facts.moves {
		let to = spec.hosts.iter().find(|h| h.name == m.to);
		let ok = alive.contains(m.from.as_str())
			&& !report_only.contains(m.from.as_str())
			&& to.is_some_and(|h| world.open(h));
		if ok {
			out.push(m);
		} else {
			notes.push(Note::MoveGivenUp {
				environment: m.environment.clone(),
				from: m.from.clone(),
				to: m.to.clone(),
			});
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
pub(super) fn pick_move(
	world: &World,
	room: &Room,
	placement: &[Assignment],
	rollbacks: &BTreeMap<String, Rollback>,
	band: Rebalance,
) -> Option<(Move, Note)> {
	let (spec, facts, alive, copies) = (world.spec, world.facts, &world.alive, &world.copies);
	// Calm: every placed copy runs steady, and nothing runs that is not placed.
	for a in placement {
		if world.steady_named(&a.host, &a.environment, &a.service, &a.version) < a.count {
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
			let known = world.service(&c.environment, &c.service).is_some();
			if c.state == CopyState::Starting || (known && !placed) {
				return None;
			}
		}
	}
	if spec.hosts.iter().any(|h| {
		(h.draining || world.finishing.contains(h.name.as_str())) && alive.contains(h.name.as_str())
	}) || !rollbacks.is_empty()
	{
		return None;
	}
	let open: Vec<&HostSpec> = spec.hosts.iter().filter(|h| world.open(h)).collect();
	let percent = |h: &HostSpec| reserved_percent(h.capacity, room.left(h.name.as_str()));
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
	let service_of = |env: &str, name: &str| world.service(env, name);
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
					&& room.left(to.name.as_str()).fits(need)
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
					reserved_percent(from.capacity, room.left(from.name.as_str()).plus(need));
				let after_to =
					reserved_percent(to.capacity, room.left(to.name.as_str()).minus(need));
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
			let why = Note::Rebalance {
				environment: env.to_string(),
				from: from.name.clone(),
				from_percent: percent(from),
				to: to.name.clone(),
				to_percent: percent(to),
			};
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
