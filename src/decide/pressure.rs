//! Pressure (Kubernetes' kubelet eviction, by QoS class): a host whose running copies USE more than
//! `pressure_percent` of it sheds services until what stays fits, and no new copy goes there. Off
//! unless the spec sets the percent.

use super::note::Note;
use super::world::World;
use crate::spec::{CopyState, Qos, Resources, ServiceSpec};
use std::collections::{BTreeMap, BTreeSet};

/// What pressure sheds: the hosts under it, and by host the service ids moved off it.
pub(super) struct Pressure<'a> {
	pub hosts: BTreeSet<&'a str>,
	pub shed: BTreeMap<&'a str, BTreeSet<String>>,
}

/// Hosts whose running copies use more than `pressure_percent` of their capacity (CPU or memory, a
/// dimension with no capacity left out), and on each, the services moved off it until what stays fits:
/// best effort first, then burstable by how far past its request it runs, the lowest priority first
/// within a class, never a guaranteed or one-clock copy. A host that only reports, frozen or draining
/// is left as it is.
pub(super) fn pressure<'a>(world: &World<'a>, notes: &mut Vec<Note>) -> Pressure<'a> {
	let (spec, facts, alive, report_only) =
		(world.spec, world.facts, &world.alive, &world.report_only);
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
			if let Some(s) = world.service(&c.environment, &c.service) {
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
			notes.push(Note::Shed {
				host: name.to_string(),
				percent,
				id: id.clone(),
				qos: s.qos(),
			});
			shed.insert(id);
		}
		if over(left) {
			notes.push(Note::StillPressed {
				host: name.to_string(),
				percent,
			});
		}
		if !shed.is_empty() {
			out.shed.insert(name, shed);
		}
	}
	out
}
