//! Whether a host may take a service's copy, and if not, why: every rule written once, in the order
//! a refusal names them (Kubernetes' filter plugins: one check gives both the yes and the reason).
//! Three questions, each the one before and more:
//!
//!   - `bars`: may the service run here at all? What runs on a host that passes is kept (`keep.rs`).
//!   - `closed`: may a NEW copy go here? Not on a host held as it is or under pressure.
//!   - `full`: is there room and share for one more copy now?
//!
//! The shortfall's reasons (`refusals`) are the same checks asked of every host, so the alert and
//! the admin console can never name a reason the placement did not use.

use super::room::{Chosen, Room};
use super::world::{Database, World};
use super::{Refusal, Refused};
use crate::spec::{HostSpec, Resources, ServiceSpec};
use std::collections::{BTreeMap, BTreeSet};

pub(super) struct Gate<'a, 'w> {
	world: &'w World<'a>,
	service: &'a ServiceSpec,
	/// Hosts under pressure (`pressure.rs`): never given a new copy.
	pressured: &'w BTreeSet<&'a str>,
	/// Hosts whose agent gave up on this service.
	pub gave_up: BTreeSet<&'a str>,
	/// Hosts its copies move off (pressure, the rebalancer).
	pub off: BTreeSet<&'a str>,
	pub database: Database<'a>,
	/// The most copies one host may hold: `cap` on its database's host (packing), one elsewhere.
	pub cap: u32,
}

impl<'a, 'w> Gate<'a, 'w> {
	pub fn new(
		world: &'w World<'a>,
		service: &'a ServiceSpec,
		pressured: &'w BTreeSet<&'a str>,
		gave_up: BTreeSet<&'a str>,
		off: BTreeSet<&'a str>,
		cap: u32,
	) -> Self {
		Gate {
			world,
			service,
			pressured,
			gave_up,
			off,
			database: world.database(service),
			cap,
		}
	}

	/// How many copies of the service `host` may hold.
	pub fn cap_at(&self, host: &str) -> u32 {
		if self.database.host == Some(host) {
			self.cap
		} else {
			1
		}
	}

	/// How many more copies of the service `host` may hold, past those chosen there.
	pub fn share_left(&self, host: &str, chosen: &Chosen) -> u32 {
		self.cap_at(host).saturating_sub(chosen.on_host(host))
	}

	/// Why the service may not run on `h` at all.
	pub fn bars(&self, h: &HostSpec) -> Option<Refused> {
		let name = h.name.as_str();
		let on_database = self.database.host == Some(name);
		if h.dedicated_to != self.service.dedicated_to {
			Some(if h.dedicated_to.is_some() {
				Refused::Dedicated
			} else {
				Refused::Shared
			})
		} else if !self.service.placement.allows(h) {
			Some(Refused::NotOnItsList)
		} else if !self.world.alive.contains(name) {
			Some(Refused::NotReporting)
		} else if !(h.serves || h.frozen || (h.serves_home && !h.draining && on_database)) {
			Some(if h.draining {
				Refused::Draining
			} else {
				Refused::NotServing
			})
		} else if self.gave_up.contains(name) {
			Some(Refused::GaveUp)
		} else if self.off.contains(name) {
			Some(Refused::MovingOff)
		} else {
			None
		}
	}

	/// Why no new copy goes on `h`: barred, or held as it is.
	pub fn closed(&self, h: &HostSpec) -> Option<Refused> {
		let name = h.name.as_str();
		self.bars(h).or(if h.frozen {
			Some(Refused::Frozen)
		} else if h.draining {
			// A draining host that still serves (no spec the control plane writes, but nothing refuses
			// one) keeps what runs there and takes nothing new.
			Some(Refused::Draining)
		} else if self.world.report_only.contains(name) {
			Some(Refused::OnlyReports)
		} else if self.pressured.contains(name) {
			Some(Refused::UnderPressure)
		} else {
			None
		})
	}

	/// Why one more copy does not fit on `h` now: its share held, or its room taken.
	pub fn full(&self, h: &HostSpec, chosen: &Chosen, room: &Room) -> Option<Refused> {
		let name = h.name.as_str();
		if self.share_left(name, chosen) == 0 {
			Some(Refused::HoldsItsShare)
		} else if !room.left(name).fits(self.service.per_copy) {
			Some(Refused::Full)
		} else {
			None
		}
	}

	/// Every host of the spec that takes no more copies, and why, in the spec's order.
	pub fn refusals(&self, chosen: &Chosen, room: &Room) -> Vec<Refusal> {
		self.world
			.spec
			.hosts
			.iter()
			.filter_map(|h| {
				let reason = self.closed(h).or_else(|| self.full(h, chosen, room))?;
				Some(Refusal {
					host: h.name.clone(),
					reason,
					dedicated_to: h.dedicated_to.clone().or_else(|| h.owner.clone()),
					capacity: h.capacity,
					free: room.left(h.name.as_str()),
					server: h.server,
					room: h.room.clone(),
				})
			})
			.collect()
	}
}

/// The refusals as a clause for the note, by reason: ` (2 full: h1 256 of 4096 MB free, h2 …; 1
/// not on its list of hosts: h3)`. Five hosts named a reason, then how many more.
pub(super) fn refusal_summary(refused: &[Refusal], needs: Resources) -> String {
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
					Refused::Full if r.free.memory_mb < needs.memory_mb => {
						let measure = match r.room.as_deref() {
							Some(room) => format!(", {room} room"),
							None => String::new(),
						};
						match r.server {
							Some(server) if server.memory_mb > r.capacity.memory_mb => format!(
								"{} {} of {} MB free, of a {} MB server{measure}",
								r.host, r.free.memory_mb, r.capacity.memory_mb, server.memory_mb
							),
							_ => format!(
								"{} {} of {} MB free{measure}",
								r.host, r.free.memory_mb, r.capacity.memory_mb
							),
						}
					}
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
