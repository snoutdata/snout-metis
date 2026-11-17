//! Which host a new copy goes to, among the hosts the gate leaves open (Kubernetes' score plugins,
//! as one fixed order of preferences instead of weights, so the same input always gives the same
//! host). In order:
//!
//!   1. packing: the project's home first;
//!   2. the rebalancer's `to`, while it moves a project;
//!   3. a host the project's earlier services hold (one set of hosts a project);
//!   4. the fewest copies of this service, then the fewest in its fault domain (spread);
//!   5. off the database's own host (a spread copy is there for when that host is lost, MT14);
//!   6. in the database primary's region;
//!   7. the most memory left;
//!   8. by name.

use super::gate::Gate;
use super::room::{Chosen, Room};
use crate::spec::HostSpec;
use std::collections::BTreeSet;

pub(super) fn best_host<'a>(
	candidates: &[&'a HostSpec],
	gate: &Gate,
	chosen: &Chosen,
	room: &Room,
	pack: bool,
	with: Option<&BTreeSet<&'a str>>,
	prefer: Option<&str>,
) -> Option<&'a str> {
	let database = gate.database;
	let shared = |host: &HostSpec| with.is_some_and(|w| w.contains(host.name.as_str()));
	let on = |host: &str| chosen.on_host(host);
	let in_domain = |domain: &str| {
		candidates
			.iter()
			.filter(|h| h.fault_domain == domain)
			.map(|h| on(&h.name))
			.sum::<u32>()
	};
	let on_database = |host: &HostSpec| database.host == Some(host.name.as_str());
	let near = |host: &HostSpec| database.region == Some(host.region.as_str());
	let preferred = |h: &HostSpec| prefer == Some(h.name.as_str());
	candidates
		.iter()
		.filter(|h| gate.full(h, chosen, room).is_none())
		.min_by(|a, b| {
			let packed = if pack {
				on_database(b).cmp(&on_database(a))
			} else {
				std::cmp::Ordering::Equal
			};
			packed
				.then(preferred(b).cmp(&preferred(a)))
				.then(shared(b).cmp(&shared(a)))
				.then(on(&a.name).cmp(&on(&b.name)))
				.then(in_domain(&a.fault_domain).cmp(&in_domain(&b.fault_domain)))
				.then(on_database(a).cmp(&on_database(b)))
				.then(near(b).cmp(&near(a)))
				.then(
					room.left(&b.name)
						.memory_mb
						.cmp(&room.left(&a.name).memory_mb),
				)
				.then(a.name.cmp(&b.name))
		})
		.map(|h| h.name.as_str())
}
