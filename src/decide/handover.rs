//! Make before break, once (Kubernetes: an old pod goes only when its replacement is Ready). A copy
//! that runs where it should no longer be stays placed, and so routed, until what replaces it runs
//! steady; then it is let go. Four reasons a copy is in that position, one rule for all of them:
//!
//! | why          | replaced once                                | its room while held              |
//! |--------------|----------------------------------------------|----------------------------------|
//! | `ToHome`     | the home's copies of its version run steady  | counted (taken now)              |
//! | `NoRoom`     | the service's chosen copies run steady       | not counted: there is none       |
//! | `Draining`   | the service's chosen copies run steady       | not counted: the host serves none|
//! | `MovingOff`  | the service's chosen copies run steady       | counted (taken when kept), given back when let go |
//!
//! A held `NoRoom` or `MovingOff` copy also holds the rebalancer's move in flight (`still_moving`).

use super::note::Note;
use super::room::{Chosen, Room};
use crate::spec::Resources;
use std::collections::BTreeSet;

/// (host, version, copies) of one service, held where they run until replaced.
#[derive(Debug, Clone, Copy)]
pub(super) struct Hold<'a> {
	pub host: &'a str,
	pub version: &'a str,
	pub copies: u32,
	pub why: Why<'a>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Why<'a> {
	/// A packing project's copy off its home, while the home has room for it.
	ToHome(&'a str),
	/// Kept where its room is taken (a host shrank, or more copies run there than fit).
	NoRoom,
	/// On a draining (or finishing) host.
	Draining,
	/// On a host the service moves off: pressure, or the rebalancer's `from`.
	MovingOff,
}

/// One service's handover, with what it changes.
pub(super) struct Handover<'a, 'b> {
	pub id: &'b str,
	pub environment: &'a str,
	pub per_copy: Resources,
	pub chosen: &'b mut Chosen<'a>,
	pub room: &'b mut Room<'a>,
	pub still_moving: &'b mut BTreeSet<(&'a str, &'a str)>,
	pub notes: &'b mut Vec<Note>,
}

impl<'a> Handover<'a, '_> {
	/// Lets `hold` go when it is `replaced`, else keeps it placed where it runs.
	pub fn settle(&mut self, hold: Hold<'a>, replaced: bool) {
		let Hold {
			host,
			version,
			copies,
			why,
		} = hold;
		let (id, h, v) = (self.id.to_string(), host.to_string(), version.to_string());
		if replaced {
			match why {
				Why::ToHome(home) => self.notes.push(Note::MovedHome {
					id,
					host: h,
					version: v,
					home: home.to_string(),
				}),
				Why::NoRoom => self.notes.push(Note::MovedNoRoom {
					id,
					host: h,
					version: v,
				}),
				Why::Draining => {}
				Why::MovingOff => {
					self.room.give(host, self.per_copy, copies);
					self.notes.push(Note::MovedOff {
						id,
						host: h,
						version: v,
					});
				}
			}
			return;
		}
		if let Why::ToHome(_) = why {
			self.room.take(host, self.per_copy, copies);
		}
		self.chosen.add(host, version, copies);
		if matches!(why, Why::NoRoom | Why::MovingOff) {
			self.still_moving.insert((self.environment, host));
		}
		self.notes.push(match why {
			Why::ToHome(home) => Note::KeptForHome {
				id,
				host: h,
				version: v,
				home: home.to_string(),
			},
			Why::NoRoom => Note::KeptNoRoom {
				id,
				host: h,
				version: v,
			},
			Why::Draining => Note::KeptDraining {
				id,
				host: h,
				version: v,
			},
			Why::MovingOff => Note::KeptMovingOff {
				id,
				host: h,
				version: v,
			},
		});
	}
}
