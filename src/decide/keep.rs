//! The first pass, for every service before any is added to: what runs is kept where the gate
//! allows it and its room is there, up to the copies wanted. At fleet size one decision holds many
//! environments on the same hosts, and a copy that runs is never moved to make room for one that does
//! not yet: a new project's copies take what the running ones leave. What runs and is not kept is
//! held for its handover (`handover.rs`) when it is moving somewhere rather than simply surplus.

use super::gate::Gate;
use super::handover::{Hold, Why};
use super::note::Note;
use super::pressure::Pressure;
use super::room::{Chosen, Room, fitting};
use super::wanted::{version_targets, wanted_copies};
use super::world::World;
use crate::spec::{CopyState, HostSpec, Move, PlaceMode, ServiceSpec, VersionShare};
use std::collections::{BTreeMap, BTreeSet};

/// One service between the two passes: what it wants, and the copies it keeps.
pub(super) struct ServicePlan<'a, 'w> {
	pub service: &'a ServiceSpec,
	pub id: String,
	/// Its notes so far (load, gave up), said before the copies it adds.
	pub said: Vec<Note>,
	pub targets: Vec<(&'a str, u32)>,
	/// Copies kept, by the index of `targets`.
	pub kept: Vec<u32>,
	pub gate: Gate<'a, 'w>,
	/// Hosts the gate does not bar: what runs on them may be kept, and the new copies go among them.
	pub eligible: Vec<&'a HostSpec>,
	/// Its policy packs: the home first, up to `cap` copies there, one on any other host.
	pub pack: bool,
	pub chosen: Chosen<'a>,
	/// What runs and waits for its handover, in the order found.
	pub holds: Vec<Hold<'a>>,
	/// The host its new copies go to first: the rebalancer's `to`.
	pub prefer: Option<&'a str>,
}

pub(super) fn keep<'a, 'w>(
	world: &'w World<'a>,
	service: &'a ServiceSpec,
	shares: &'a BTreeMap<String, Vec<VersionShare>>,
	pressure: &'w Pressure<'a>,
	moves: &[&'a Move],
	room: &mut Room<'a>,
) -> ServicePlan<'a, 'w> {
	let id = service.id();
	let copies = &world.copies;
	let mut said = Vec::new();
	let wanted = wanted_copies(service, world, &mut said);
	let targets = version_targets(&shares[&id], wanted);
	let gave_up: BTreeSet<&str> = copies
		.hosts_of(service)
		.iter()
		.filter(|(_, c)| c.state == CopyState::GaveUp)
		.map(|(h, _)| *h)
		.collect();
	for host in &gave_up {
		said.push(Note::GaveUp {
			id: id.clone(),
			host: host.to_string(),
		});
	}
	// Hosts its copies move off, make before break: one under pressure that sheds this service, or the
	// rebalancer's `from` for its environment (with the host it goes to first).
	let mut off: BTreeMap<&str, Option<&str>> = BTreeMap::new();
	for (host, ids) in &pressure.shed {
		if ids.contains(&id) {
			off.insert(host, None);
		}
	}
	for m in moves {
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
	// How many copies one host may hold. One, for a service the agents run on one fixed host port
	// (`compile.rs`: a lost host leaves its copy unplaced and noted, never doubled up on a survivor)
	// and for one whose policy spreads (a lost host's copy waits for a host of its own, which an
	// operator adds). When it packs, up to `max_per_host` on its home alone, which the control plane
	// runs as one bigger services pod there, and one on any other host, past the home's room.
	let pack = service.policy.mode == PlaceMode::Pack && service.run.is_none();
	let cap = if pack { service.policy.max_per_host } else { 1 };
	let gate = Gate::new(
		world,
		service,
		&pressure.hosts,
		gave_up,
		off.keys().copied().collect(),
		cap,
	);
	let eligible: Vec<&HostSpec> = world
		.spec
		.hosts
		.iter()
		.filter(|h| gate.bars(h).is_none())
		.collect();

	let mut holds = Vec::new();
	// The copies on a host they move off keep their room there until they go.
	for host in &gate.off {
		for (version, n) in world.running_versions(host, service) {
			room.take(host, service.per_copy, n);
			holds.push(Hold {
				host,
				version,
				copies: n,
				why: Why::MovingOff,
			});
		}
	}
	// Packing starts at the database's own host (the project's home) while it has room.
	let home = gate.database.host.filter(|h| {
		pack && !world.report_only.contains(h) && eligible.iter().any(|e| e.name == *h)
	});
	let mut chosen = Chosen::default();
	let mut kept_of = Vec::with_capacity(targets.len());
	for (version, target) in &targets {
		// What runs, on hosts still eligible.
		let mut existing: Vec<(&str, u32)> = eligible
			.iter()
			.map(|h| {
				(
					h.name.as_str(),
					world.present(h.name.as_str(), service, version),
				)
			})
			.filter(|(_, n)| *n > 0)
			.collect();
		// What runs on a report-only host first: it cannot be moved, so it is the one kept. Then,
		// packing, what runs on the home. A finishing host's copy is kept last: a replacement already
		// elsewhere is kept first, and the finishing copy is then held only by its handover.
		let pinned = |h: &str| world.report_only.contains(h) && !world.finishing.contains(h);
		existing.sort_by(|a, b| {
			pinned(b.0)
				.cmp(&pinned(a.0))
				.then(
					world
						.finishing
						.contains(a.0)
						.cmp(&world.finishing.contains(b.0)),
				)
				.then((home == Some(b.0)).cmp(&(home == Some(a.0))))
				.then(b.1.cmp(&a.1))
				.then(a.0.cmp(b.0))
		});
		let mut kept = 0;
		// Copies the home is holding room for, from hosts they move off.
		let mut reserved = 0;
		let ran = existing.clone();
		for (host, n) in existing {
			let share = gate.share_left(host, &chosen);
			let wanted_here = n.min(target - kept).min(share);
			// Each copy kept takes its own room: a host holds no more running copies than fit, and the
			// rest are placed elsewhere and held here until those run steady.
			let mut take = fitting(room.left(host), service.per_copy, wanted_here);
			if take < wanted_here {
				holds.push(Hold {
					host,
					version,
					copies: wanted_here - take,
					why: Why::NoRoom,
				});
			}
			if let Some(home) = home
				&& host != home
				&& !world.report_only.contains(host)
			{
				// A copy off the home moves there while the home has room: placed there, and kept here
				// until that one runs steady.
				let at_home = room.left_after(home, service.per_copy, reserved);
				let open = gate.share_left(home, &chosen).saturating_sub(reserved);
				let moving = fitting(at_home, service.per_copy, take.min(open));
				take -= moving;
				reserved += moving;
			}
			room.take(host, service.per_copy, take);
			chosen.add(host, version, take);
			kept += take;
			if kept == *target {
				break;
			}
		}
		// Packing: what runs off the home and is not kept moves to it, and stays where it runs until the
		// home's copies run steady.
		if let Some(home) = home {
			for (host, n) in ran {
				let kept_there = chosen.count(host, version);
				if host != home && !world.report_only.contains(host) && n > kept_there {
					holds.push(Hold {
						host,
						version,
						copies: n - kept_there,
						why: Why::ToHome(home),
					});
				}
			}
		}
		kept_of.push(kept);
	}
	ServicePlan {
		service,
		id,
		said,
		targets,
		kept: kept_of,
		gate,
		eligible,
		pack,
		chosen,
		holds,
		prefer,
	}
}
