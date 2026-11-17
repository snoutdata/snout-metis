//! What each service wants: its versions as this decision places them (a rollback gives a version
//! that gave up's share back to an older one), how many copies (`copies`, or by its load with
//! `scale`), and how those copies split between its versions. Nothing here looks at hosts.

use super::note::Note;
use super::world::World;
use crate::spec::{CopyFacts, CopyState, Resources, Rollback, ServiceSpec, VersionShare};
use std::collections::BTreeMap;

/// The versions a service is placed and routed at: the spec's, with a rolled-back version's share
/// given to the version it rolled back to.
pub(super) fn effective_versions(
	service: &ServiceSpec,
	rollback: Option<&Rollback>,
) -> Vec<VersionShare> {
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
pub(super) fn rollbacks(world: &World, notes: &mut Vec<Note>) -> BTreeMap<String, Rollback> {
	let (facts, alive, copies) = (world.facts, &world.alive, &world.copies);
	let mut out = BTreeMap::new();
	for service in &world.spec.services {
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
			notes.push(Note::RolledBack {
				id: id.clone(),
				from: failed.version.clone(),
				to: to.clone(),
			});
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

/// How many copies a service wants now: `copies`, or with `scale` (`spec.rs`, [`crate::spec::Scale`])
/// what its load says, one step from what runs. Only from facts: every RUNNING copy on a live host
/// must report its load, and none may be starting (a copy just added has not taken its share yet),
/// or it holds what is there, within its limits. So a scale-up adds one copy and waits for it.
pub(super) fn wanted_copies(service: &ServiceSpec, world: &World, notes: &mut Vec<Note>) -> u32 {
	let alive = &world.alive;
	let Some(scale) = service.scale else {
		return service.copies;
	};
	let within = |n: u32| n.clamp(service.copies, scale.max_copies);
	let copies: Vec<&CopyFacts> = world
		.copies
		.hosts_of(service)
		.iter()
		.filter(|(h, c)| alive.contains(h) && c.state != CopyState::GaveUp)
		.map(|(_, c)| *c)
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
		notes.push(Note::ScaleUp {
			id,
			n,
			now,
			up: scale.up_percent,
			max: scale.max_copies,
		});
		return n + 1;
	}
	if n > service.copies {
		let fewer = busy_percent(used, service.per_copy, n - 1);
		if fewer < u64::from(scale.down_percent) {
			notes.push(Note::ScaleDown {
				id,
				n,
				now,
				fewer,
				down: scale.down_percent,
				least: service.copies,
			});
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
pub(super) fn version_targets(versions: &[VersionShare], copies: u32) -> Vec<(&str, u32)> {
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
