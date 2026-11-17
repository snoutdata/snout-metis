//! What a decision says it did and why. Kept as values while it decides (a copy un-placed by a roll
//! takes its own note back, by value), and put into words once, for the outcome's notes: the log
//! (MT6), the alerts (MT11) and the admin console read those words, so the words below are the wire
//! format and never change by accident.

use super::Refusal;
use super::gate::refusal_summary;
use crate::spec::{Qos, Resources};
use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Note {
	/// A host not heard within the timeout.
	Silent { host: String, secs: u64 },
	/// An agent gave up on a version; its share goes back to an older one.
	RolledBack {
		id: String,
		from: String,
		to: String,
	},
	/// Pressure: a host's copies use more than they may, and this service moves off it.
	Shed {
		host: String,
		percent: u32,
		id: String,
		qos: Qos,
	},
	/// Pressure: what is left is guaranteed, so nothing more moves.
	StillPressed { host: String, percent: u32 },
	/// The rebalancer's move in flight can no longer be made.
	MoveGivenUp {
		environment: String,
		from: String,
		to: String,
	},
	/// Scaling on load, one up or one down.
	ScaleUp {
		id: String,
		n: u32,
		now: u64,
		up: u32,
		max: u32,
	},
	ScaleDown {
		id: String,
		n: u32,
		now: u64,
		fewer: u64,
		down: u32,
		least: u32,
	},
	/// An agent gave up on its copy; placed elsewhere.
	GaveUp { id: String, host: String },
	/// A new copy.
	Placed {
		id: String,
		version: String,
		host: String,
	},
	/// Fewer copies than wanted, and every host's reason.
	Short {
		id: String,
		version: String,
		target: u32,
		placed: u32,
		why: &'static str,
		refused: Vec<Refusal>,
		needs: Resources,
	},
	/// Handovers (`handover.rs`): moved, or kept until what replaces it runs steady.
	MovedHome {
		id: String,
		host: String,
		version: String,
		home: String,
	},
	KeptForHome {
		id: String,
		host: String,
		version: String,
		home: String,
	},
	MovedNoRoom {
		id: String,
		host: String,
		version: String,
	},
	KeptNoRoom {
		id: String,
		host: String,
		version: String,
	},
	KeptDraining {
		id: String,
		host: String,
		version: String,
	},
	MovedOff {
		id: String,
		host: String,
		version: String,
	},
	KeptMovingOff {
		id: String,
		host: String,
		version: String,
	},
	/// A roll keeps an old copy one more decision (`rollout.rs`).
	KeptForRoll {
		id: String,
		host: String,
		old: String,
	},
	/// A roll's surge copy, beside the old ones.
	Surge {
		id: String,
		version: String,
		host: String,
	},
	/// The rebalancer's next move.
	Rebalance {
		environment: String,
		from: String,
		from_percent: u64,
		to: String,
		to_percent: u64,
	},
}

impl fmt::Display for Note {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Note::Silent { host, secs } => write!(
				f,
				"host {host} is silent past {secs}s; its copies are placed elsewhere"
			),
			Note::RolledBack { id, from, to } => {
				write!(
					f,
					"{id}: an agent gave up on {from}; rolled back to {to} until the spec changes its version"
				)
			}
			Note::Shed {
				host,
				percent,
				id,
				qos,
			} => write!(
				f,
				"{host}: its copies use more than {percent}% of what it has; {id} ({}) moves off it",
				match qos {
					Qos::BestEffort => "best effort",
					Qos::Burstable => "burstable",
					Qos::Guaranteed => "guaranteed",
				}
			),
			Note::StillPressed { host, percent } => {
				write!(
					f,
					"{host}: still past {percent}% with only guaranteed copies left; nothing more moves"
				)
			}
			Note::MoveGivenUp {
				environment,
				from,
				to,
			} => {
				write!(
					f,
					"rebalance: {environment}'s move from {from} to {to} is no longer possible; given up"
				)
			}
			Note::ScaleUp {
				id,
				n,
				now,
				up,
				max,
			} => write!(
				f,
				"{id}: {n} copies at {now}% of what they reserve, above {up}%: {} wanted (at most {max})",
				n + 1
			),
			Note::ScaleDown {
				id,
				n,
				now,
				fewer,
				down,
				least,
			} => write!(
				f,
				"{id}: {n} copies at {now}%, {fewer}% on one fewer, below {down}%: {} wanted (at least {least})",
				n - 1
			),
			Note::GaveUp { id, host } => write!(
				f,
				"{id}: the agent on {host} gave up on it; placed on another host"
			),
			Note::Placed { id, version, host } => {
				write!(f, "{id}: a copy of {version} placed on {host}")
			}
			Note::Short {
				id,
				version,
				target,
				placed,
				why,
				refused,
				needs,
			} => write!(
				f,
				"{id}: wanted {target} copies of {version}, placed {placed}: {why}{}",
				refusal_summary(refused, *needs)
			),
			Note::MovedHome {
				id,
				host,
				version,
				home,
			} => write!(
				f,
				"{id}: {host}'s copy of {version} moved to its home, {home}"
			),
			Note::KeptForHome {
				id,
				host,
				version,
				home,
			} => {
				write!(
					f,
					"{id}: {host} keeps its copy of {version} until the one on its home, {home}, runs steady"
				)
			}
			Note::MovedNoRoom { id, host, version } => {
				write!(
					f,
					"{id}: {host}'s copy of {version} moved off it, since its room there is taken"
				)
			}
			Note::KeptNoRoom { id, host, version } => write!(
				f,
				"{id}: {host} has no room for its copy of {version}; it stays until the copies elsewhere run steady"
			),
			Note::KeptDraining { id, host, version } => write!(
				f,
				"{id}: {host} keeps its copy of {version} while it drains, until the copies elsewhere run steady"
			),
			Note::MovedOff { id, host, version } => {
				write!(f, "{id}: {host}'s copy of {version} moved off it")
			}
			Note::KeptMovingOff { id, host, version } => {
				write!(
					f,
					"{id}: {host} keeps its copy of {version} until the copies elsewhere run steady"
				)
			}
			Note::KeptForRoll { id, host, old } => write!(
				f,
				"{id}: {host} keeps {old} until the copy changing before it runs"
			),
			Note::Surge { id, version, host } => write!(
				f,
				"{id}: a copy of {version} placed on {host}, beside the old ones (surge)"
			),
			Note::Rebalance {
				environment,
				from,
				from_percent,
				to,
				to_percent,
			} => write!(
				f,
				"rebalance: {environment}'s copies on {from} ({from_percent}% reserved) move to {to} ({to_percent}%)"
			),
		}
	}
}
