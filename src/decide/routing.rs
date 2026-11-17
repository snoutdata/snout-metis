//! The routing table every proxy balances calls from: the copies placed AND reported running on a
//! live host, at their version's share, the ones placed and not running yet named but never routed.

use super::world::World;
use super::{Assignment, Route};
use crate::spec::{CopyState, HostSpec, VersionShare};
use std::collections::BTreeMap;

/// Routes to the copies that are placed AND reported running on a live host, and names the ones
/// placed and not running yet (`starting`), which are never routed to. A service that is not
/// `routed` is in the table with no route, so its copies run and no call reaches them.
pub(super) fn routing_table(
	world: &World,
	placement: &[Assignment],
	held: &BTreeMap<(String, String, String), u32>,
	shares: &BTreeMap<String, Vec<VersionShare>>,
) -> BTreeMap<String, Vec<Route>> {
	let (spec, alive, copies) = (world.spec, &world.alive, &world.copies);
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
		let database = world.database(service);
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
				let running = world.running(host.name.as_str(), service, version);
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
						near_database: database.region == Some(host.region.as_str()),
					});
				}
			}
		}
		routes.sort();
		table.insert(id, routes);
	}
	table
}
