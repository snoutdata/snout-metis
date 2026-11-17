//! A version change at the pace the service allows (Kubernetes' Deployment: `max_unavailable`,
//! `max_surge`), after its copies are chosen. A version change never takes a region's copies down in
//! one decision (swapping both copies in a region at once leaves the far copy alone answering until
//! they run). A running copy a decision would retire, on a host that stays, goes only when nothing
//! placed is still on its way up, and `max_unavailable` hosts at a time (one, without a rollout); the
//! rest keep their version, routed at the share of the version replacing it, until a later decision.
//! A copy added on such a host is taken back, and with a rollout, the copy that would have replaced
//! it goes beside the old ones instead, while the service stays within `max_surge` of its copies (the
//! surge).

use super::gate::Gate;
use super::note::Note;
use super::room::{Chosen, Room};
use super::score::best_host;
use super::world::World;
use crate::spec::{HostSpec, ServiceSpec, VersionShare};
use std::collections::{BTreeMap, BTreeSet};

pub(super) struct Roll<'a, 'b> {
	pub world: &'b World<'a>,
	pub service: &'a ServiceSpec,
	pub id: &'b str,
	pub gate: &'b Gate<'a, 'b>,
	pub pack: bool,
	pub eligible: &'b [&'a HostSpec],
	/// Hosts a new copy may go to (`Gate::closed` passes).
	pub addable: &'b [&'a HostSpec],
	/// Its versions as this decision places them (rollbacks applied).
	pub shares: &'b [VersionShare],
	/// Its copies wanted, all versions.
	pub wanted: u32,
	pub with: Option<&'b BTreeSet<&'a str>>,
}

impl<'a> Roll<'a, '_> {
	/// Holds back what would go too fast. `added` is (host, version) of each copy this decision
	/// added, `held` the old versions kept through the roll with their share, for the table.
	pub fn pace(
		&self,
		chosen: &mut Chosen<'a>,
		added: &mut Vec<(&'a str, &'a str)>,
		room: &mut Room<'a>,
		held: &mut BTreeMap<(String, String, String), u32>,
		notes: &mut Vec<Note>,
	) {
		let service = self.service;
		let rollout = service.rollout;
		let max_unavailable = rollout.map_or(1, |r| r.max_unavailable) as usize;
		let max_surge = rollout.map_or(0, |r| r.max_surge);
		let mut retiring: Vec<(&str, &str, u32)> = Vec::new();
		if !added.is_empty() || rollout.is_some() {
			let mut hosts: Vec<&str> = self
				.eligible
				.iter()
				.map(|h| h.name.as_str())
				.filter(|h| !self.world.report_only.contains(h))
				.collect();
			hosts.sort();
			for host in hosts {
				for (version, running) in self.world.running_versions(host, service) {
					let gone = running.saturating_sub(chosen.count(host, version));
					if gone > 0 {
						retiring.push((host, version, gone));
					}
				}
			}
		}
		let changing: BTreeSet<&str> = retiring.iter().map(|(h, _, _)| *h).collect();
		let on_the_way = chosen
			.by_host()
			.filter(|(host, versions)| {
				!changing.contains(host)
					&& versions
						.iter()
						.any(|(version, n)| self.world.running(host, service, version) < *n)
			})
			.count();
		let budget = max_unavailable.saturating_sub(on_the_way);
		let mut let_go: BTreeSet<&str> = BTreeSet::new();
		// Copies of the new version moved to a host holding none of the service (the surge).
		let mut surged: Vec<(&str, &str)> = Vec::new();
		for (host, old, gone) in retiring {
			if let_go.contains(host) || let_go.len() < budget {
				let_go.insert(host);
				continue;
			}
			// The copies added on this host are taken back, with their notes.
			let mut replacing = None;
			added.retain(|(h, version)| {
				if *h != host {
					return true;
				}
				chosen.remove_one(host, version);
				room.give(host, service.per_copy, 1);
				let placed = Note::Placed {
					id: self.id.to_string(),
					version: version.to_string(),
					host: host.to_string(),
				};
				if let Some(i) = notes.iter().position(|n| *n == placed) {
					notes.remove(i);
				}
				replacing.get_or_insert(*version);
				false
			});
			room.take(host, service.per_copy, gone);
			chosen.add(host, old, gone);
			let share_of = |version: &str| {
				self.shares
					.iter()
					.find(|v| v.version == version)
					.map_or(0, |v| v.share)
			};
			held.insert(
				(self.id.to_string(), host.to_string(), old.to_string()),
				share_of(old).max(replacing.map_or(0, share_of)),
			);
			notes.push(Note::KeptForRoll {
				id: self.id.to_string(),
				host: host.to_string(),
				old: old.to_string(),
			});
			// The surge: the copy that would have replaced it here goes to a host holding none of the
			// service, beside it.
			if let Some(version) = replacing
				&& chosen.total() < self.wanted + max_surge
			{
				let fresh: Vec<&HostSpec> = self
					.addable
					.iter()
					.copied()
					.filter(|h| chosen.on_host(h.name.as_str()) == 0)
					.collect();
				if let Some(to) =
					best_host(&fresh, self.gate, chosen, room, self.pack, self.with, None)
				{
					room.take(to, service.per_copy, 1);
					chosen.add(to, version, 1);
					surged.push((to, version));
				}
			}
		}
		for (to, version) in surged {
			notes.push(Note::Surge {
				id: self.id.to_string(),
				version: version.to_string(),
				host: to.to_string(),
			});
		}
	}
}
