//! The second pass, one service at a time in priority order, once every service has kept what runs:
//!
//!   1. add what is missing, each copy on the best host the gate leaves open (`score.rs`), and say
//!      why when a version is short;
//!   2. hand over a packing project's copies moving home, and the copies kept where their room is taken;
//!   3. pace a version change (`rollout.rs`);
//!   4. hand over a draining host's copies and the copies on a host the service moves off.
//!
//! The order is the behaviour: a roll counts what goes after the copies without room are held, so it
//! never lets one go early, and the last handovers count what the roll left.

use super::handover::{Handover, Hold, Why};
use super::keep::ServicePlan;
use super::note::Note;
use super::rollout::Roll;
use super::room::Board;
use super::score::best_host;
use super::world::World;
use super::{Assignment, Shortfall};
use crate::spec::{HostSpec, VersionShare};
use std::collections::BTreeMap;

pub(super) fn settle<'a>(
	plan: ServicePlan<'a, '_>,
	world: &World<'a>,
	shares: &'a BTreeMap<String, Vec<VersionShare>>,
	board: &mut Board<'a>,
) {
	let ServicePlan {
		service,
		id,
		said,
		targets,
		kept,
		gate,
		eligible,
		pack,
		mut chosen,
		holds,
		prefer,
	} = plan;
	let mut notes = said;
	// 1. Add what is missing.
	let addable: Vec<&HostSpec> = eligible
		.iter()
		.copied()
		.filter(|h| gate.closed(h).is_none())
		.collect();
	// (host, version) of each copy this decision adds.
	let mut added: Vec<(&str, &str)> = Vec::new();
	for ((version, target), kept) in targets.iter().zip(kept) {
		let mut placed = kept;
		while placed < *target {
			let with = board.env_hosts.get(service.environment.as_str());
			let Some(host) = best_host(&addable, &gate, &chosen, &board.room, pack, with, prefer)
			else {
				break;
			};
			board.room.take(host, service.per_copy, 1);
			chosen.add(host, version, 1);
			notes.push(Note::Placed {
				id: id.clone(),
				version: version.to_string(),
				host: host.to_string(),
			});
			added.push((host, version));
			placed += 1;
		}
		if placed < *target {
			let refused = gate.refusals(&chosen, &board.room);
			let why = if eligible.is_empty() {
				"no eligible host is reporting"
			} else if addable.is_empty() {
				"every eligible host only reports"
			} else {
				"no eligible host has room"
			};
			notes.push(Note::Short {
				id: id.clone(),
				version: version.to_string(),
				target: *target,
				placed,
				why,
				refused: refused.clone(),
				needs: service.per_copy,
			});
			board.shortfalls.push(Shortfall {
				service: id.clone(),
				version: version.to_string(),
				wanted: *target,
				placed,
				needs: service.per_copy,
				hosts: refused,
			});
		}
	}
	board
		.env_hosts
		.entry(service.environment.as_str())
		.or_default()
		.extend(chosen.hosts());
	let wanted: u32 = targets.iter().map(|(_, t)| *t).sum();

	// 2. Copies moving home, then copies kept where their room is taken.
	{
		let mut handover = Handover {
			id: &id,
			environment: service.environment.as_str(),
			per_copy: service.per_copy,
			chosen: &mut chosen,
			room: &mut board.room,
			still_moving: &mut board.still_moving,
			notes: &mut notes,
		};
		for &hold in &holds {
			if let Why::ToHome(home) = hold.why {
				let replaced = world.steady(home, service, hold.version)
					>= handover.chosen.count(home, hold.version);
				handover.settle(hold, replaced);
			}
		}
		let steady = world.steady_chosen(service, &*handover.chosen);
		for &hold in holds.iter().filter(|h| h.why == Why::NoRoom) {
			handover.settle(hold, steady >= wanted);
		}
	}

	// 3. A version change, at its pace.
	Roll {
		world,
		service,
		id: &id,
		gate: &gate,
		pack,
		eligible: &eligible,
		addable: &addable,
		shares: &shares[&id],
		wanted,
		with: board.env_hosts.get(service.environment.as_str()),
	}
	.pace(
		&mut chosen,
		&mut added,
		&mut board.room,
		&mut board.held,
		&mut notes,
	);

	// 4. A draining host's copies, then the copies on a host it moves off: both go once the copies
	// chosen elsewhere run steady.
	let steady = world.steady_chosen(service, &chosen);
	let mut draining: Vec<Hold> = Vec::new();
	for host in world.spec.hosts.iter().filter(|h| {
		(h.draining || world.finishing.contains(h.name.as_str()))
			&& world.alive.contains(h.name.as_str())
	}) {
		for version in world
			.running_versions(host.name.as_str(), service)
			.into_keys()
		{
			draining.push(Hold {
				host: host.name.as_str(),
				version,
				copies: 1,
				why: Why::Draining,
			});
		}
	}
	let mut handover = Handover {
		id: &id,
		environment: service.environment.as_str(),
		per_copy: service.per_copy,
		chosen: &mut chosen,
		room: &mut board.room,
		still_moving: &mut board.still_moving,
		notes: &mut notes,
	};
	for hold in draining
		.into_iter()
		.chain(holds.iter().copied().filter(|h| h.why == Why::MovingOff))
	{
		handover.settle(hold, steady >= wanted);
	}

	board.notes.extend(notes);
	for (host, version, count) in chosen.iter() {
		board.placement.push(Assignment {
			host: host.to_string(),
			environment: service.environment.clone(),
			service: service.name.clone(),
			version: version.to_string(),
			count,
		});
	}
}
