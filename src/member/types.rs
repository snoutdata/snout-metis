//! What the members agree on.

use crate::database::{DatabasePrimary, ProjectPrimary};
use crate::decide::Outcome;
use crate::spec::Spec;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Cursor;

openraft::declare_raft_types!(
	/// The members' Raft types: [`Command`] in, [`Applied`] out, nodes by `u64` id and HTTP address.
	pub TypeConfig:
		D = Command,
		R = Applied,
);

pub type NodeId = u64;
pub type Node = openraft::BasicNode;
pub type Raft = openraft::Raft<TypeConfig>;

/// One change to the replicated state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
	/// An operator applied a spec (`metis apply`).
	SetSpec(Spec),
	/// The leader decided: what each agent runs and the table, with what moved and why (MT6).
	Decided(Decision),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
	/// The leader's clock when it decided, and who it was.
	pub at_ms: u64,
	pub by: NodeId,
	pub outcome: Outcome,
	/// The database primary the decision took its region from, when a report gave one
	/// (`database.rs`): replicated, so a new leader keeps it through a failover in which no copy
	/// says `primary` yet. A member on an older build writes none and ignores it.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub database: Option<DatabasePrimary>,
	/// Each project's primary copy by ref (`database.rs`, `project_primaries`), from the agents'
	/// project reports: what `/table`'s `projects` serves, so a standby's door sends a call to the
	/// primary's. Replicated like `database`, so a new leader keeps it; an older member ignores it.
	#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
	pub projects: BTreeMap<String, ProjectPrimary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Applied;

/// The replicated state, as every member holds it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct State {
	pub spec: Option<Spec>,
	pub decision: Option<Decision>,
	/// The last decisions that moved something, newest last, for `metis log`: their notes and heard
	/// hosts (`apply`); the whole of each is in the leader's decision log (MT6).
	pub history: Vec<Decision>,
}

/// How many decisions [`State::history`] keeps.
pub const HISTORY: usize = 200;

impl State {
	pub fn apply(&mut self, command: Command) {
		match command {
			Command::SetSpec(spec) => self.spec = Some(spec),
			Command::Decided(decision) => {
				if !decision.outcome.notes.is_empty() {
					// What moved and why, without the placement, table and project primaries: at
					// fleet size those are the bulk of a decision, and 200 of them in every
					// snapshot is what the state cannot carry. The decision log has them whole,
					// with the facts (`record.rs`). A member on an older build keeps them here.
					let mut kept = decision.clone();
					kept.outcome.placement.clear();
					kept.outcome.table.clear();
					kept.projects.clear();
					self.history.push(kept);
					if self.history.len() > HISTORY {
						let excess = self.history.len() - HISTORY;
						self.history.drain(..excess);
					}
				}
				self.decision = Some(decision);
			}
		}
	}
}
