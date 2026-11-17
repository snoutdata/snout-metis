//! Everything that changes while deciding. `Room` is what each host has left once the copies chosen
//! so far are counted: every take and every give goes through it, never a sum kept on the side.
//! `Chosen` is one service's copies by host and version. `Board` is what the decision carries from
//! one service to the next: the room, and what it has placed, held and said so far. All of it is only
//! bookkeeping: what to take and when is decided by the steps (`keep.rs`, `place.rs`,
//! `handover.rs`, `rollout.rs`).

use super::note::Note;
use super::{Assignment, Shortfall};
use crate::spec::{Resources, Spec};
use std::collections::{BTreeMap, BTreeSet};

/// What the decision has so far, across services.
pub(super) struct Board<'a> {
	pub room: Room<'a>,
	/// environment -> the hosts its services so far hold: a project's later services go there first,
	/// so its copies name one set of hosts (otherwise auth and its router on different hosts would
	/// name more hosts than copies, which the control plane refuses).
	pub env_hosts: BTreeMap<&'a str, BTreeSet<&'a str>>,
	/// Environments that still hold a copy on a host they move off (a move not done yet).
	pub still_moving: BTreeSet<(&'a str, &'a str)>,
	/// (service id, host, version) -> share: an old version a host keeps through a roll.
	pub held: BTreeMap<(String, String, String), u32>,
	pub placement: Vec<Assignment>,
	pub shortfalls: Vec<Shortfall>,
	pub notes: Vec<Note>,
}

/// What each host has left: its capacity, less the reservation (`per_copy`) of every copy counted on it.
pub(super) struct Room<'a> {
	free: BTreeMap<&'a str, Resources>,
}

impl<'a> Room<'a> {
	pub fn new(spec: &'a Spec) -> Self {
		Room {
			free: spec
				.hosts
				.iter()
				.map(|h| (h.name.as_str(), h.capacity))
				.collect(),
		}
	}

	pub fn left(&self, host: &str) -> Resources {
		self.free[host]
	}

	/// `n` copies' room taken on `host` (each at `per_copy`, never below nothing).
	pub fn take(&mut self, host: &str, per_copy: Resources, n: u32) {
		let free = self.free.get_mut(host).expect("a host of the spec");
		*free = after(*free, per_copy, n);
	}

	/// `n` copies' room given back on `host`.
	pub fn give(&mut self, host: &str, per_copy: Resources, n: u32) {
		let free = self.free.get_mut(host).expect("a host of the spec");
		for _ in 0..n {
			*free = free.plus(per_copy);
		}
	}

	/// What is left after `n` more copies on `host`, without taking it.
	pub fn left_after(&self, host: &str, per_copy: Resources, n: u32) -> Resources {
		after(self.free[host], per_copy, n)
	}
}

/// `left` less `n` copies, one at a time (each never below nothing).
fn after(left: Resources, per_copy: Resources, n: u32) -> Resources {
	(0..n).fold(left, |left, _| left.minus(per_copy))
}

/// How many copies of `per_copy` fit in `left`, at most `most`.
pub(super) fn fitting(left: Resources, per_copy: Resources, most: u32) -> u32 {
	let mut left = left;
	let mut n = 0;
	while n < most && left.fits(per_copy) {
		left = left.minus(per_copy);
		n += 1;
	}
	n
}

/// One service's copies chosen so far: host -> version -> copies.
#[derive(Default)]
pub(super) struct Chosen<'a> {
	by_host: BTreeMap<&'a str, BTreeMap<&'a str, u32>>,
}

impl<'a> Chosen<'a> {
	pub fn add(&mut self, host: &'a str, version: &'a str, n: u32) {
		if n > 0 {
			*self
				.by_host
				.entry(host)
				.or_default()
				.entry(version)
				.or_default() += n;
		}
	}

	/// One copy fewer of `version` on `host` (one that was added).
	pub fn remove_one(&mut self, host: &str, version: &str) {
		let versions = self.by_host.get_mut(host).expect("added there");
		let n = versions.get_mut(version).expect("added there");
		*n -= 1;
		if *n == 0 {
			versions.remove(version);
		}
	}

	pub fn count(&self, host: &str, version: &str) -> u32 {
		self.by_host
			.get(host)
			.and_then(|v| v.get(version))
			.copied()
			.unwrap_or(0)
	}

	/// Every version's copies on `host`.
	pub fn on_host(&self, host: &str) -> u32 {
		self.by_host.get(host).map_or(0, |v| v.values().sum())
	}

	pub fn total(&self) -> u32 {
		self.by_host.values().flat_map(|v| v.values()).sum()
	}

	pub fn hosts(&self) -> impl Iterator<Item = &'a str> + '_ {
		self.by_host.keys().copied()
	}

	/// (host, version, copies), by host then version.
	pub fn iter(&self) -> impl Iterator<Item = (&'a str, &'a str, u32)> + '_ {
		self.by_host
			.iter()
			.flat_map(|(host, versions)| versions.iter().map(|(version, n)| (*host, *version, *n)))
	}

	/// Each host's versions and copies.
	pub fn by_host(&self) -> impl Iterator<Item = (&'a str, &BTreeMap<&'a str, u32>)> + '_ {
		self.by_host
			.iter()
			.map(|(host, versions)| (*host, versions))
	}
}
