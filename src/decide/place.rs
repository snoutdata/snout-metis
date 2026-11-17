//! The second pass, one service at a time in priority order, once every service has kept what runs:
//!
//!   0. a copy held where it runs (moving home, or where its room is taken) is chosen there first, so
//!      nothing new goes beside it;
//!   1. add what is missing, each copy on the best host the gate leaves open (`score.rs`), and say
//!      why when a version is short; packing, then move a copy kept off the home to it while the home
//!      has share and room;
//!   2. hand over the copies moving home, and the copies kept where their room is taken;
//!   3. pace a version change (`rollout.rs`);
//!   4. hand over a draining host's copies and the copies on a host the service moves off.
//!
//! Each running copy is held once at most, and a held copy is never its own replacement.
//!
//! The order is the behaviour: a roll counts what goes after the copies without room are held, so it
//! never lets one go early, and the last handovers count what the roll left.

use super::handover::{Handover, Hold, Why};
use super::keep::ServicePlan;
use super::note::Note;
use super::rollout::Roll;
use super::room::{Board, Chosen};
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
		mut holds,
		prefer,
		home,
	} = plan;
	let mut notes = said;
	// A copy moving home, or kept where its room is taken, is chosen where it runs from the start (with
	// its room, moving home): no other copy of the service goes beside it there while it is held.
	let mut moving = Chosen::default();
	for hold in holds
		.iter()
		.filter(|h| matches!(h.why, Why::ToHome(_) | Why::NoRoom))
	{
		if let Why::ToHome(_) = hold.why {
			board.room.take(hold.host, service.per_copy, hold.copies);
		}
		chosen.add(hold.host, hold.version, hold.copies);
		moving.add(hold.host, hold.version, hold.copies);
	}
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
	// 1b. Packing: a copy kept off the home moves there while the home has share and room, placed there
	// and held where it runs until the copies chosen run steady.
	if let Some(home) = home
		&& let Some(home_host) = eligible.iter().find(|h| h.name == home)
	{
		for (version, _) in &targets {
			let off: Vec<(&str, u32)> = chosen
				.by_host()
				.filter(|(host, _)| *host != home && !world.report_only.contains(host))
				.map(|(host, _)| {
					(
						host,
						chosen
							.count(host, version)
							.saturating_sub(moving.count(host, version)),
					)
				})
				.filter(|(_, n)| *n > 0)
				.collect();
			for (host, n) in off {
				for _ in 0..n {
					if gate.full(home_host, &chosen, &board.room).is_some() {
						break;
					}
					board.room.take(home, service.per_copy, 1);
					chosen.add(home, version, 1);
					notes.push(Note::Placed {
						id: id.clone(),
						version: version.to_string(),
						host: home.to_string(),
					});
					added.push((home, version));
					moving.add(host, version, 1);
					holds.push(Hold {
						host,
						version,
						copies: 1,
						why: Why::ToHome(home),
					});
				}
			}
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
		// Counted with the copies moving home aside: a held copy is no replacement for another (a copy
		// moving home was let go when the home got none, 2026-11-17).
		let mut replacements = handover.chosen.clone();
		for (host, version, n) in moving.iter() {
			replacements.remove(host, version, n);
		}
		let steady = world.steady_chosen(service, &replacements);
		for why in [
			|w: &Why| matches!(w, Why::ToHome(_)),
			|w: &Why| *w == Why::NoRoom,
		] {
			for &hold in holds.iter().filter(|h| why(&h.why)) {
				handover.settle(hold, steady >= wanted);
			}
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
			// Each running copy is held once at most: not when it is chosen there already (a finishing
			// host's copy kept), nor when it is held as moving off that host.
			let moving_off = holds
				.iter()
				.any(|h| h.why == Why::MovingOff && h.host == host.name && h.version == version);
			if chosen.count(host.name.as_str(), version) > 0 || moving_off {
				continue;
			}
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
