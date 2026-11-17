//! The decision: spec and facts in, placement, routing table and notes out (MT7). Pure, and the same
//! input always gives the same output: every choice breaks ties by name.
//!
//! It is laid out the way Kubernetes splits the same work, each step in its own file:
//!
//! | step                                   | here           | Kubernetes                         |
//! |----------------------------------------|----------------|------------------------------------|
//! | what is heard, what each service wants | `world.rs`, `wanted.rs` | node status, ReplicaSet / HPA |
//! | may a host take a copy, and why not    | `gate.rs`      | scheduler filter plugins           |
//! | which host is best                     | `score.rs`     | scheduler score plugins            |
//! | what runs stays where it fits          | `keep.rs`      | running pods are not rescheduled   |
//! | add what is missing                    | `place.rs`     | scheduling a pending pod           |
//! | an old copy goes once replaced         | `handover.rs`  | Deployment, PodDisruptionBudget    |
//! | a version change at its pace           | `rollout.rs`   | Deployment rolling update          |
//! | a host whose copies use too much       | `pressure.rs`  | kubelet eviction                   |
//! | evening out the hosts                  | `rebalance.rs` | descheduler                        |
//! | where calls go                         | `routing.rs`   | EndpointSlices                     |
//!
//! Room is reservations only (`per_copy`, `room.rs`): measured use never decides a placement here,
//! as the Kubernetes scheduler never reads it. What the facts say is read once (`World`); everything
//! that changes while deciding is in `room.rs`: each host's room (`Room`), each service's chosen
//! copies (`Chosen`), and what the decision carries from service to service (`Board`). Notes are
//! values while it decides (`note.rs`) and words only in the outcome.

mod gate;
mod handover;
mod keep;
mod note;
mod place;
mod pressure;
mod rebalance;
mod rollout;
mod room;
mod routing;
mod score;
mod wanted;
mod world;

use crate::spec::{Facts, Move, Resources, Rollback, ServiceSpec, Spec, VersionShare};
use room::{Board, Room};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use world::World;

/// `count` copies of one version of one service, on one host: what that host's agent is told to run.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Assignment {
	pub host: String,
	pub environment: String,
	pub service: String,
	pub version: String,
	pub count: u32,
}

/// One entry of the routing table: where a proxy may send a call for this service. Only copies
/// the agents report as running are routed to; `share` is the version's share of the calls.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Route {
	pub host: String,
	pub region: String,
	pub version: String,
	pub running: u32,
	/// Copies placed there that are not running yet. A proxy never sends a call to one; a door with no
	/// near copy running may hold a call a moment for one rather than send it far (`METIS_HOLD_MS`).
	#[serde(default, skip_serializing_if = "is_zero")]
	pub starting: u32,
	/// Copies placed there that wait for ANOTHER version of the service on the same host to stop (one
	/// copy per host: the agent drains the old one first). Not `starting`, since the old copy answers
	/// the same health and a hold would take it for this one; a door reads it only to keep
	/// this version's share near while the replacement happens.
	#[serde(default, skip_serializing_if = "is_zero")]
	pub replacing: u32,
	pub share: u32,
	/// The host is in the database primary's region: the proxy prefers it (MT4).
	pub near_database: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Outcome {
	pub placement: Vec<Assignment>,
	/// By service id (`environment/name`).
	pub table: BTreeMap<String, Vec<Route>>,
	/// What changed and why, for the log (MT6) and the alerts (MT11). Empty when nothing moved.
	pub notes: Vec<String>,
	/// The hosts this decision heard (reporting within the host timeout), by name. A host not in it
	/// was decided WITHOUT, so its share says nothing about what it should run: `/desired` tells it
	/// to keep its file (`server.rs`). Empty in a decision from a member before this field.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub heard: Vec<String>,
	/// The rebalancer's move in flight, which the next decision carries on (`Facts::moves`).
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub moves: Vec<Move>,
	/// Rollouts rolled back, by service id, which the next decision keeps (`Facts::rollbacks`).
	#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
	pub rollbacks: BTreeMap<String, Rollback>,
	/// Each version of a service placed short of its target, with every host the decision
	/// considered and why it took no more copies: what the admin console's hosts, the shortfall
	/// alert and the dashboard read, so none of them guesses why.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub shortfalls: Vec<Shortfall>,
}

/// One version of one service a decision could not give every copy it wants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shortfall {
	/// The service id (`environment/name`).
	pub service: String,
	pub version: String,
	pub wanted: u32,
	pub placed: u32,
	/// What one more copy needs (`per_copy`).
	pub needs: Resources,
	/// Every host of the spec, and why it took no more copies, in the spec's order.
	pub hosts: Vec<Refusal>,
}

/// One host's answer to one more copy of a service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
	pub host: String,
	pub reason: Refused,
	/// Whose host it is: the customer it is dedicated to, else its owner (a pool's server) when the
	/// spec says.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub dedicated_to: Option<String>,
	pub capacity: Resources,
	/// What is left on it after this decision's placements so far.
	pub free: Resources,
	/// The whole server, when the spec says (`HostSpec::server`).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub server: Option<Resources>,
	/// Which measure its capacity came from when not the reservations (`HostSpec::room`).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub room: Option<String>,
}

/// Why a host took no more copies of a service. The first that applies, in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Refused {
	/// Dedicated to another customer.
	Dedicated,
	/// A shared host, and the service runs only on its customer's dedicated hosts.
	Shared,
	/// Not on the service's list of hosts or regions (`placement`). On the fleet, a pool's servers
	/// are kept off a shared project's list this way (sql/337).
	NotOnItsList,
	/// Not heard within the host timeout. After the three above: a host the service could never use
	/// is that, not silent (2026-11-17: bamboo's alert blamed a silent shared host it may not use).
	NotReporting,
	/// Being emptied to retire.
	Draining,
	/// Serves nothing (a witness host, or closed).
	NotServing,
	/// Its agent gave up on this service there.
	GaveUp,
	/// The service's copies are moving off it (pressure, the rebalancer).
	MovingOff,
	/// Full and held as it is: nothing new goes there.
	Frozen,
	/// Its agent only reports.
	OnlyReports,
	/// Its copies use more than `pressure_percent` of it.
	UnderPressure,
	/// Already holds as many copies of this service as one host may.
	HoldsItsShare,
	/// Not enough CPU or memory left for one more copy.
	Full,
}

impl Refused {
	/// A few words for the note, after a count.
	fn words(self) -> &'static str {
		match self {
			Refused::NotReporting => "not reporting",
			Refused::Dedicated => "dedicated to another customer",
			Refused::Shared => "shared, and it runs only on its customer's own hosts",
			Refused::NotOnItsList => "not on its list of hosts",
			Refused::Draining => "draining",
			Refused::NotServing => "serving nothing",
			Refused::GaveUp => "where its agent gave up on it",
			Refused::MovingOff => "it is moving off",
			Refused::Frozen => "frozen",
			Refused::OnlyReports => "only reporting",
			Refused::UnderPressure => "under pressure",
			Refused::HoldsItsShare => "already holding its copy",
			Refused::Full => "full",
		}
	}
}

/// How long a copy must have run and answered its health before a draining host's copy of the same
/// service is let go (packing).
pub const STEADY_MS: u64 = 30_000;

fn is_zero(n: &u32) -> bool {
	*n == 0
}

/// Decides from a spec that passed [`Spec::check`].
pub fn decide(spec: &Spec, facts: &Facts) -> Outcome {
	let mut notes = Vec::new();
	let world = World::new(spec, facts, &mut notes);
	// Rollbacks: the ones the last decision made, while the spec still names the version that gave
	// up, and the new ones this decision finds. Each service's versions as this decision places them.
	let rollbacks = wanted::rollbacks(&world, &mut notes);
	let shares: BTreeMap<String, Vec<VersionShare>> = spec
		.services
		.iter()
		.map(|s| {
			let id = s.id();
			let versions = wanted::effective_versions(s, rollbacks.get(&id));
			(id, versions)
		})
		.collect();
	let pressure = pressure::pressure(&world, &mut notes);
	let moves = rebalance::live_moves(&world, &mut notes);

	// The highest priority first, so it keeps and takes room before the lower ones; then by id.
	let mut services: Vec<&ServiceSpec> = spec.services.iter().collect();
	services.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.id().cmp(&b.id())));
	let mut board = Board {
		room: Room::new(spec),
		env_hosts: BTreeMap::new(),
		still_moving: BTreeSet::new(),
		held: BTreeMap::new(),
		placement: Vec::new(),
		shortfalls: Vec::new(),
		notes,
	};
	// First what runs, for every service, then what is missing.
	let plans: Vec<_> = services
		.into_iter()
		.map(|service| keep::keep(&world, service, &shares, &pressure, &moves, &mut board.room))
		.collect();
	for plan in plans {
		place::settle(plan, &world, &shares, &mut board);
	}
	board.placement.sort();

	// A move in flight stays while a copy of its environment is held on `from`, and after that while
	// `from` still runs one: let go is not yet gone, and with the move over the next decision would
	// see the copy on both hosts and could keep the one on `from` (2026-11-17, the orchestration
	// drill's flake). Then a new one may start, from this decision's placement, once everything runs
	// steady.
	let mut next_moves: Vec<Move> = moves
		.iter()
		.filter(|m| {
			board
				.still_moving
				.contains(&(m.environment.as_str(), m.from.as_str()))
				|| world.still_runs(&m.from, &m.environment)
		})
		.map(|m| (*m).clone())
		.collect();
	if next_moves.is_empty()
		&& let Some(band) = spec.rebalance
		&& let Some((m, why)) =
			rebalance::pick_move(&world, &board.room, &board.placement, &rollbacks, band)
	{
		board.notes.push(why);
		next_moves.push(m);
	}

	let table = routing::routing_table(&world, &board.placement, &board.held, &shares);
	Outcome {
		placement: board.placement,
		table,
		notes: board.notes.iter().map(ToString::to_string).collect(),
		heard: world.alive.iter().map(|h| h.to_string()).collect(),
		moves: next_moves,
		rollbacks,
		shortfalls: board.shortfalls,
	}
}

#[cfg(test)]
mod tests {
	use super::wanted::version_targets;
	use super::*;
	use crate::spec::{CopyFacts, HostFacts, VersionShare};
	use crate::spec::{CopyState, HostSpec, PlaceMode};

	const NOW: u64 = 1_000_000;

	fn host(name: &str, region: &str, domain: &str) -> HostSpec {
		HostSpec {
			name: name.into(),
			region: region.into(),
			fault_domain: domain.into(),
			serves: true,
			draining: false,
			serves_home: false,
			frozen: false,
			finishing: false,
			dedicated_to: None,
			server: None,
			owner: None,
			room: None,
			capacity: Resources {
				millicpu: 2000,
				memory_mb: 4096,
			},
		}
	}

	fn service(name: &str, copies: u32, versions: &[(&str, u32)]) -> ServiceSpec {
		ServiceSpec {
			name: name.into(),
			environment: "control-plane".into(),
			copies,
			versions: versions
				.iter()
				.map(|(v, s)| VersionShare {
					version: (*v).into(),
					share: *s,
				})
				.collect(),
			per_copy: Resources {
				millicpu: 100,
				memory_mb: 256,
			},
			one_clock: false,
			dedicated_to: None,
			run: None,
			placement: Default::default(),
			routed: true,
			scale: None,
			policy: Default::default(),
			limit: None,
			priority: 0,
			rollout: None,
		}
	}

	fn copy(service: &str, version: &str, state: CopyState) -> CopyFacts {
		CopyFacts {
			environment: "control-plane".into(),
			service: service.into(),
			version: version.into(),
			state,
			load: None,
			running_since_ms: None,
		}
	}

	fn alive(name: &str, copies: Vec<CopyFacts>) -> HostFacts {
		HostFacts {
			name: name.into(),
			last_heartbeat_ms: NOW - 1000,
			copies,
			report_only: false,
		}
	}

	fn silent(name: &str, copies: Vec<CopyFacts>) -> HostFacts {
		HostFacts {
			name: name.into(),
			last_heartbeat_ms: NOW - 120_000,
			copies,
			report_only: false,
		}
	}

	/// A control plane's shape: two hosts in the home region, one far away, and a member-only host.
	fn control_plane(services: Vec<ServiceSpec>) -> Spec {
		let mut witness = host("member-1", "region-b", "region-c-1");
		witness.serves = false;
		Spec {
			host_timeout_secs: 30,
			hosts: vec![
				host("home-1", "region-a", "FAULT-DOMAIN-3"),
				host("home-2", "region-a", "FAULT-DOMAIN-1"),
				host("remote-1", "region-c", "region-b-1"),
				witness,
			],
			services,
			database_cluster: None,
			agents: Vec::new(),
			pressure_percent: None,
			rebalance: None,
		}
	}

	/// The routes a proxy sends calls to: running copies, not the ones starting.
	fn routed_to<'a>(outcome: &'a Outcome, id: &str) -> Vec<&'a Route> {
		outcome.table[id].iter().filter(|r| r.running > 0).collect()
	}

	fn on(outcome: &Outcome, host: &str, service: &str) -> u32 {
		outcome
			.placement
			.iter()
			.filter(|a| a.host == host && a.service == service)
			.map(|a| a.count)
			.sum()
	}

	fn all_hosts_alive(copies: &[(&str, Vec<CopyFacts>)]) -> Facts {
		let names = ["home-1", "home-2", "remote-1", "member-1"];
		Facts {
			now_ms: NOW,
			hosts: names
				.iter()
				.map(|n| {
					alive(
						n,
						copies
							.iter()
							.find(|(h, _)| h == n)
							.map(|(_, c)| c.clone())
							.unwrap_or_default(),
					)
				})
				.collect(),
			database_primary_region: Some("region-c".into()),
			database_hosts: Default::default(),
			moves: Vec::new(),
			rollbacks: Default::default(),
		}
	}

	#[test]
	fn a_decision_names_the_hosts_it_heard() {
		let spec = control_plane(vec![service("auth", 2, &[("0.1.11", 1)])]);
		let running = vec![copy("auth", "0.1.11", CopyState::Running)];
		let facts = Facts {
			now_ms: NOW,
			hosts: vec![
				alive("home-1", running.clone()),
				silent("home-2", running),
				alive("remote-1", vec![]),
			],
			database_primary_region: None,
			database_hosts: Default::default(),
			moves: Vec::new(),
			rollbacks: Default::default(),
		};
		let outcome = decide(&spec, &facts);
		assert_eq!(outcome.heard, ["home-1", "remote-1"]);
		assert_eq!(on(&outcome, "home-2", "auth"), 0);
	}

	#[test]
	fn steady_state_moves_nothing() {
		let spec = control_plane(vec![service("auth", 3, &[("0.1.11", 1)])]);
		let running = vec![copy("auth", "0.1.11", CopyState::Running)];
		let facts = all_hosts_alive(&[
			("home-1", running.clone()),
			("home-2", running.clone()),
			("remote-1", running),
		]);
		let outcome = decide(&spec, &facts);
		assert!(outcome.notes.is_empty(), "{:?}", outcome.notes);
		for h in ["home-1", "home-2", "remote-1"] {
			assert_eq!(on(&outcome, h, "auth"), 1);
		}
		assert_eq!(outcome.table["control-plane/auth"].len(), 3);
	}

	#[test]
	fn first_placement_spreads_and_never_uses_a_member_only_host() {
		let spec = control_plane(vec![service("auth", 3, &[("0.1.11", 1)])]);
		let outcome = decide(&spec, &all_hosts_alive(&[]));
		for h in ["home-1", "home-2", "remote-1"] {
			assert_eq!(on(&outcome, h, "auth"), 1, "{h}");
		}
		assert_eq!(on(&outcome, "member-1", "auth"), 0);
		// Nothing runs yet, so nothing is routed to; each is named as starting.
		assert!(routed_to(&outcome, "control-plane/auth").is_empty());
		assert_eq!(outcome.table["control-plane/auth"].len(), 3);
	}

	#[test]
	fn a_silent_host_has_its_copies_placed_elsewhere() {
		let spec = control_plane(vec![service("functions", 3, &[("0.2.1", 1)])]);
		let running = vec![copy("functions", "0.2.1", CopyState::Running)];
		let mut facts = all_hosts_alive(&[
			("home-1", running.clone()),
			("home-2", running.clone()),
			("remote-1", running.clone()),
		]);
		facts.hosts[2] = silent("remote-1", running);
		let outcome = decide(&spec, &facts);
		assert_eq!(on(&outcome, "remote-1", "functions"), 0);
		// One per host (spread): the third waits for a host of its own, never doubled up.
		assert_eq!(on(&outcome, "home-1", "functions"), 1);
		assert_eq!(on(&outcome, "home-2", "functions"), 1);
		assert!(
			outcome
				.notes
				.iter()
				.any(|n| n.contains("wanted 3 copies of 0.2.1, placed 2"))
		);
		assert!(
			outcome
				.notes
				.iter()
				.any(|n| n.contains("remote-1 is silent"))
		);
		assert!(
			outcome.table["control-plane/functions"]
				.iter()
				.all(|r| r.host != "remote-1")
		);
	}

	#[test]
	fn a_copy_the_agent_gave_up_on_goes_to_another_host_and_not_back() {
		let spec = control_plane(vec![service("auth", 1, &[("0.1.11", 1)])]);
		let facts = all_hosts_alive(&[("home-1", vec![copy("auth", "0.1.11", CopyState::GaveUp)])]);
		let outcome = decide(&spec, &facts);
		assert_eq!(on(&outcome, "home-1", "auth"), 0);
		assert_eq!(outcome.placement.len(), 1);
		assert!(outcome.notes.iter().any(|n| n.contains("gave up")));
	}

	#[test]
	fn a_starting_copy_is_kept_where_it_is_but_not_routed_to() {
		let spec = control_plane(vec![service("auth", 1, &[("0.1.11", 1)])]);
		let facts =
			all_hosts_alive(&[("home-2", vec![copy("auth", "0.1.11", CopyState::Starting)])]);
		let outcome = decide(&spec, &facts);
		assert_eq!(on(&outcome, "home-2", "auth"), 1);
		assert!(outcome.notes.is_empty(), "{:?}", outcome.notes);
		assert!(routed_to(&outcome, "control-plane/auth").is_empty());
		assert_eq!(outcome.table["control-plane/auth"][0].starting, 1);
	}

	#[test]
	fn one_clock_stays_put_and_moves_only_when_its_host_is_lost() {
		let mut sweeps = service("sweeps", 1, &[("a1", 1)]);
		sweeps.one_clock = true;
		let spec = control_plane(vec![sweeps]);
		let facts = all_hosts_alive(&[("home-2", vec![copy("sweeps", "a1", CopyState::Running)])]);
		assert_eq!(on(&decide(&spec, &facts), "home-2", "sweeps"), 1);
		let mut lost = facts.clone();
		lost.hosts[1] = silent("home-2", vec![copy("sweeps", "a1", CopyState::Running)]);
		let outcome = decide(&spec, &lost);
		assert_eq!(on(&outcome, "home-2", "sweeps"), 0);
		assert_eq!(outcome.placement.iter().map(|a| a.count).sum::<u32>(), 1);
	}

	/// `rest` scaled between 2 and 4 copies (80% up, 40% down), each reserving 100m and 256 MB.
	fn scaled() -> ServiceSpec {
		let mut s = service("rest", 2, &[("v14", 1)]);
		s.scale = Some(crate::spec::Scale {
			max_copies: 4,
			up_percent: 80,
			down_percent: 40,
		});
		s
	}

	fn loaded(millicpu: u32, memory_mb: u32) -> CopyFacts {
		CopyFacts {
			load: Some(Resources {
				millicpu,
				memory_mb,
			}),
			..copy("rest", "v14", CopyState::Running)
		}
	}

	fn total(outcome: &Outcome) -> u32 {
		outcome
			.placement
			.iter()
			.filter(|a| a.service == "rest")
			.map(|a| a.count)
			.sum()
	}

	#[test]
	fn load_above_the_band_adds_one_copy_and_inside_it_moves_nothing() {
		let spec = control_plane(vec![scaled()]);
		// 90m of 100m on each: 90%, above 80.
		let busy = all_hosts_alive(&[
			("home-1", vec![loaded(90, 100)]),
			("home-2", vec![loaded(90, 100)]),
		]);
		let outcome = decide(&spec, &busy);
		assert_eq!(total(&outcome), 3, "{:?}", outcome.notes);
		assert!(
			outcome
				.notes
				.iter()
				.any(|n| n.contains("90%") && n.contains("3 wanted")),
			"{:?}",
			outcome.notes
		);

		// Memory counts too: 230 of 256 MB is 89%, with CPU idle.
		let memory = all_hosts_alive(&[
			("home-1", vec![loaded(5, 230)]),
			("home-2", vec![loaded(5, 230)]),
		]);
		assert_eq!(total(&decide(&spec, &memory)), 3);

		// 60%: inside the band, nothing moves and nothing is noted.
		let steady = all_hosts_alive(&[
			("home-1", vec![loaded(60, 100)]),
			("home-2", vec![loaded(60, 100)]),
		]);
		let outcome = decide(&spec, &steady);
		assert_eq!(total(&outcome), 2);
		assert!(outcome.notes.is_empty(), "{:?}", outcome.notes);
	}

	#[test]
	fn a_step_up_lands_inside_the_band_and_so_does_a_step_down() {
		let spec = control_plane(vec![scaled()]);
		// Three copies at 60% (what two at 90% become): kept, neither up nor down.
		let three = all_hosts_alive(&[
			("home-1", vec![loaded(60, 100)]),
			("home-2", vec![loaded(60, 100)]),
			("remote-1", vec![loaded(60, 100)]),
		]);
		let outcome = decide(&spec, &three);
		assert_eq!(total(&outcome), 3, "{:?}", outcome.notes);
		assert!(outcome.notes.is_empty(), "{:?}", outcome.notes);

		// Three at 20%: 30% on two, below 40, so one goes.
		let quiet = all_hosts_alive(&[
			("home-1", vec![loaded(20, 50)]),
			("home-2", vec![loaded(20, 50)]),
			("remote-1", vec![loaded(20, 50)]),
		]);
		let outcome = decide(&spec, &quiet);
		assert_eq!(total(&outcome), 2, "{:?}", outcome.notes);
		assert!(
			outcome.notes.iter().any(|n| n.contains("2 wanted")),
			"{:?}",
			outcome.notes
		);

		// Three at 30%: 45% on two, inside the band, so all three stay.
		let between = all_hosts_alive(&[
			("home-1", vec![loaded(30, 50)]),
			("home-2", vec![loaded(30, 50)]),
			("remote-1", vec![loaded(30, 50)]),
		]);
		assert_eq!(total(&decide(&spec, &between)), 3);
	}

	#[test]
	fn never_below_copies_nor_above_max_copies() {
		let spec = control_plane(vec![scaled()]);
		let idle = all_hosts_alive(&[
			("home-1", vec![loaded(0, 0)]),
			("home-2", vec![loaded(0, 0)]),
		]);
		assert_eq!(total(&decide(&spec, &idle)), 2);
		let mut at_max = scaled();
		at_max.scale = Some(crate::spec::Scale {
			max_copies: 2,
			up_percent: 80,
			down_percent: 40,
		});
		let spec = control_plane(vec![at_max]);
		let busy = all_hosts_alive(&[
			("home-1", vec![loaded(100, 256)]),
			("home-2", vec![loaded(100, 256)]),
		]);
		let outcome = decide(&spec, &busy);
		assert_eq!(total(&outcome), 2);
		assert!(outcome.notes.is_empty(), "{:?}", outcome.notes);
	}

	#[test]
	fn without_every_load_or_with_a_copy_starting_it_holds_what_runs() {
		let spec = control_plane(vec![scaled()]);
		// One running copy does not say its load: three are kept, none added or taken.
		let partial = all_hosts_alive(&[
			("home-1", vec![loaded(100, 256)]),
			("home-2", vec![copy("rest", "v14", CopyState::Running)]),
			("remote-1", vec![loaded(0, 0)]),
		]);
		let outcome = decide(&spec, &partial);
		assert_eq!(total(&outcome), 3, "{:?}", outcome.notes);
		assert!(outcome.notes.is_empty(), "{:?}", outcome.notes);

		// The copy a step up added is still starting: nothing more until it takes its share.
		let starting = all_hosts_alive(&[
			("home-1", vec![loaded(95, 100)]),
			("home-2", vec![loaded(95, 100)]),
			("remote-1", vec![copy("rest", "v14", CopyState::Starting)]),
		]);
		assert_eq!(total(&decide(&spec, &starting)), 3);

		// No load reported anywhere, as from today's agents: exactly `copies`.
		let none = all_hosts_alive(&[
			("home-1", vec![copy("rest", "v14", CopyState::Running)]),
			("home-2", vec![copy("rest", "v14", CopyState::Running)]),
		]);
		let outcome = decide(&spec, &none);
		assert_eq!(total(&outcome), 2);
		assert!(outcome.notes.is_empty(), "{:?}", outcome.notes);
	}

	#[test]
	fn a_scale_that_cannot_mean_one_thing_is_refused() {
		let refused = |change: fn(&mut ServiceSpec)| {
			let mut s = scaled();
			change(&mut s);
			control_plane(vec![s]).check().expect_err("refused")
		};
		assert!(refused(|s| s.scale.as_mut().unwrap().max_copies = 1).contains("below copies"));
		assert!(
			refused(|s| s.scale.as_mut().unwrap().down_percent = 80).contains("back and forth")
		);
		assert!(refused(|s| s.scale.as_mut().unwrap().up_percent = 0).contains("1 to 100"));
		assert!(
			refused(|s| {
				s.copies = 1;
				s.one_clock = true;
			})
			.contains("never scaled")
		);
		assert!(refused(|s| s.per_copy = Resources::default()).contains("reserves nothing"));
		control_plane(vec![scaled()])
			.check()
			.expect("a sound scale checks");
		// Read from JSON with the band's defaults.
		let read: crate::spec::Scale =
			serde_json::from_str(r#"{"max_copies": 3}"#).expect("parses");
		assert_eq!((read.up_percent, read.down_percent), (80, 40));
	}

	#[test]
	fn versions_split_by_share() {
		let s = service("functions", 10, &[("0.2.1", 9), ("0.2.2", 1)]);
		assert_eq!(
			version_targets(&s.versions, s.copies),
			vec![("0.2.1", 9), ("0.2.2", 1)]
		);
		let small = service("functions", 2, &[("0.2.1", 99), ("0.2.2", 1)]);
		assert_eq!(
			version_targets(&small.versions, small.copies),
			vec![("0.2.1", 1), ("0.2.2", 1)]
		);
		let one = service("functions", 1, &[("0.2.1", 9), ("0.2.2", 1)]);
		assert_eq!(
			version_targets(&one.versions, one.copies),
			vec![("0.2.1", 1)]
		);
		let off = service("functions", 3, &[("0.2.1", 1), ("0.2.0", 0)]);
		assert_eq!(
			version_targets(&off.versions, off.copies),
			vec![("0.2.1", 3)]
		);
	}

	#[test]
	fn the_table_carries_shares_and_prefers_the_database_region() {
		let spec = control_plane(vec![service("functions", 2, &[("0.2.1", 9), ("0.2.2", 1)])]);
		let facts = all_hosts_alive(&[
			(
				"remote-1",
				vec![copy("functions", "0.2.1", CopyState::Running)],
			),
			(
				"home-1",
				vec![copy("functions", "0.2.2", CopyState::Running)],
			),
		]);
		let outcome = decide(&spec, &facts);
		let routes = &outcome.table["control-plane/functions"];
		assert_eq!(routes.len(), 2);
		let far = routes
			.iter()
			.find(|r| r.host == "remote-1")
			.map(|r| (r.share, r.near_database));
		assert_eq!(far, Some((9, true)));
		let home = routes
			.iter()
			.find(|r| r.host == "home-1")
			.map(|r| (r.share, r.near_database));
		assert_eq!(home, Some((1, false)));
	}

	#[test]
	fn dedicated_services_and_hosts_never_mix_with_shared_ones() {
		let mut big = host("pod-big1", "region-c", "a");
		big.dedicated_to = Some("acme".into());
		let mut theirs = service("auth", 2, &[("0.1.11", 1)]);
		theirs.environment = "acme-project".into();
		theirs.dedicated_to = Some("acme".into());
		let ours = service("rest", 2, &[("14.12", 1)]);
		let spec = Spec {
			host_timeout_secs: 30,
			hosts: vec![host("pod-shared", "region-c", "b"), big],
			services: vec![theirs, ours],
			database_cluster: None,
			agents: Vec::new(),
			pressure_percent: None,
			rebalance: None,
		};
		let facts = Facts {
			now_ms: NOW,
			hosts: vec![alive("pod-shared", vec![]), alive("pod-big1", vec![])],
			database_primary_region: None,
			database_hosts: Default::default(),
			moves: Vec::new(),
			rollbacks: Default::default(),
		};
		let outcome = decide(&spec, &facts);
		// One per host (spread): each side places what its one host may hold, never on the other's.
		assert_eq!(on(&outcome, "pod-big1", "auth"), 1);
		assert_eq!(on(&outcome, "pod-shared", "auth"), 0);
		assert_eq!(on(&outcome, "pod-shared", "rest"), 1);
		assert_eq!(on(&outcome, "pod-big1", "rest"), 0);
	}

	#[test]
	fn no_room_is_a_note_not_a_guess() {
		let mut huge = service("psyche", 4, &[("x", 1)]);
		huge.per_copy = Resources {
			millicpu: 1500,
			memory_mb: 3000,
		};
		let spec = control_plane(vec![huge]);
		let outcome = decide(&spec, &all_hosts_alive(&[]));
		assert_eq!(outcome.placement.iter().map(|a| a.count).sum::<u32>(), 3);
		assert!(
			outcome
				.notes
				.iter()
				.any(|n| n.contains("wanted 4") && n.contains("placed 3"))
		);
	}

	#[test]
	fn the_same_input_gives_the_same_output() {
		let spec = control_plane(vec![
			service("auth", 2, &[("0.1.11", 1)]),
			service("functions", 5, &[("0.2.1", 4), ("0.2.2", 1)]),
		]);
		let facts = all_hosts_alive(&[]);
		assert_eq!(decide(&spec, &facts), decide(&spec, &facts));
	}

	#[test]
	fn a_spec_that_cannot_mean_one_thing_is_refused() {
		let mut twice = control_plane(vec![
			service("auth", 1, &[("a", 1)]),
			service("auth", 2, &[("a", 1)]),
		]);
		assert!(twice.check().unwrap_err().contains("twice"));
		twice.services.pop();
		assert!(twice.check().is_ok());
		let mut clock = service("sweeps", 2, &[("a", 1)]);
		clock.one_clock = true;
		assert!(
			control_plane(vec![clock])
				.check()
				.unwrap_err()
				.contains("exactly one")
		);
		assert!(
			control_plane(vec![service("auth", 1, &[("a", 0)])])
				.check()
				.unwrap_err()
				.contains("no version")
		);
		let mut orphan = service("auth", 1, &[("a", 1)]);
		orphan.dedicated_to = Some("nobody".into());
		assert!(
			control_plane(vec![orphan])
				.check()
				.unwrap_err()
				.contains("no host is")
		);
	}

	#[test]
	fn a_spec_reads_from_json() {
		let spec: Spec = serde_json::from_str(
			r#"{"host_timeout_secs":30,
			"hosts":[{"name":"h","region":"r","fault_domain":"d","capacity":{"millicpu":1000,"memory_mb":1024}}],
			"services":[{"name":"auth","environment":"control-plane","copies":1,
				"versions":[{"version":"0.1.11","share":1}],"per_copy":{"millicpu":100,"memory_mb":128}}]}"#,
		)
		.expect("parses");
		assert!(spec.hosts[0].serves);
		assert!(spec.check().is_ok());
	}

	#[test]
	fn a_report_only_host_keeps_what_runs_there_and_is_never_given_a_copy() {
		// functions x3, one per host, rolled 2:1 to a new version, with remote-1's agent only
		// reporting: the old copy there is the one kept, and the new one goes to the home region.
		let mut functions = service("functions", 3, &[("0.2.1", 2), ("0.2.1-d5", 1)]);
		functions.run = Some(crate::spec::RunSpec {
			image_repo: "r/functions".into(),
			port: 9000,
			host_port: 9000,
			memory_mb: 256,
			..Default::default()
		});
		let spec = control_plane(vec![functions]);
		spec.check().expect("checks");
		let old = vec![copy("functions", "0.2.1", CopyState::Running)];
		let mut facts = all_hosts_alive(&[
			("home-1", old.clone()),
			("home-2", old.clone()),
			("remote-1", old.clone()),
		]);
		let without = decide(&spec, &facts);
		let version = |outcome: &Outcome, host: &str| {
			outcome
				.placement
				.iter()
				.find(|a| a.host == host && a.service == "functions")
				.map(|a| a.version.clone())
		};
		assert_eq!(
			version(&without, "remote-1").as_deref(),
			Some("0.2.1-d5"),
			"by name order, without the fact"
		);

		for host in facts.hosts.iter_mut().filter(|h| h.name == "remote-1") {
			host.report_only = true;
		}
		let with = decide(&spec, &facts);
		assert_eq!(
			version(&with, "remote-1").as_deref(),
			Some("0.2.1"),
			"what runs there is kept"
		);
		assert_eq!(version(&with, "home-1").as_deref(), Some("0.2.1"));
		assert_eq!(
			version(&with, "home-2").as_deref(),
			Some("0.2.1-d5"),
			"the new copy goes where it will run"
		);

		// Nothing is ever ADDED there: with no copy running on it, a missing copy goes unplaced, noted.
		let mut auth = service("auth", 3, &[("0.1.11", 1)]);
		auth.run = Some(crate::spec::RunSpec {
			image_repo: "r/auth".into(),
			port: 9999,
			host_port: 9102,
			memory_mb: 128,
			..Default::default()
		});
		let spec = control_plane(vec![auth]);
		let mut facts = all_hosts_alive(&[]);
		for host in facts.hosts.iter_mut().filter(|h| h.name == "remote-1") {
			host.report_only = true;
		}
		let outcome = decide(&spec, &facts);
		assert_eq!(on(&outcome, "remote-1", "auth"), 0);
		assert_eq!(outcome.placement.iter().map(|a| a.count).sum::<u32>(), 2);
		assert!(
			outcome
				.notes
				.iter()
				.any(|n| n.contains("placed 2: no eligible host has room")),
			"{:?}",
			outcome.notes
		);
	}

	#[test]
	fn a_version_change_takes_one_host_at_a_time_and_the_old_copies_stay_routed() {
		// functions x3 back from 0.2.1-d5 to 0.2.1, both home-region copies on d5, remote-1's agent
		// only reporting with 0.2.1. One decision must not swap both home-region copies, leaving
		// the table naming remote-1 alone.
		let mut functions = service("functions", 3, &[("0.2.1", 1)]);
		functions.run = Some(crate::spec::RunSpec {
			image_repo: "r/functions".into(),
			port: 9000,
			host_port: 9000,
			memory_mb: 256,
			..Default::default()
		});
		let spec = control_plane(vec![functions]);
		spec.check().expect("checks");
		let d5 = || vec![copy("functions", "0.2.1-d5", CopyState::Running)];
		let mut facts = all_hosts_alive(&[
			("home-1", d5()),
			("home-2", d5()),
			(
				"remote-1",
				vec![copy("functions", "0.2.1", CopyState::Running)],
			),
		]);
		for host in facts.hosts.iter_mut().filter(|h| h.name == "remote-1") {
			host.report_only = true;
		}
		facts.database_primary_region = Some("region-a".into());
		let version = |outcome: &Outcome, host: &str| {
			outcome
				.placement
				.iter()
				.find(|a| a.host == host && a.service == "functions")
				.map(|a| a.version.clone())
		};
		let routed = |outcome: &Outcome| {
			outcome.table["control-plane/functions"]
				.iter()
				.filter(|r| r.running > 0)
				.map(|r| format!("{}:{}:{}", r.host, r.version, r.share))
				.collect::<Vec<_>>()
		};

		let first = decide(&spec, &facts);
		assert_eq!(version(&first, "home-1").as_deref(), Some("0.2.1"));
		assert_eq!(
			version(&first, "home-2").as_deref(),
			Some("0.2.1-d5"),
			"{:?}",
			first.notes
		);
		assert_eq!(
			routed(&first),
			["home-2:0.2.1-d5:1", "remote-1:0.2.1:1"],
			"a home-region copy is still routed while the other changes"
		);
		assert!(
			!first.table["control-plane/functions"]
				.iter()
				.any(|r| r.host == "home-1"
					&& r.version == "0.2.1"
					&& (r.running > 0 || r.starting > 0)),
			"not starting while the old copy still runs there (it answers the same health): {:?}",
			first.table
		);
		assert!(
			first.table["control-plane/functions"]
				.iter()
				.any(|r| r.host == "home-1" && r.version == "0.2.1" && r.replacing == 1),
			"named replacing instead, for a door to keep that version's share near: {:?}",
			first.table
		);
		assert!(first
			.notes
			.contains(&"control-plane/functions: home-2 keeps 0.2.1-d5 until the copy changing before it runs".into()));
		assert_eq!(
			decide(&spec, &facts),
			first,
			"the same host changes first until it has"
		);

		// home-1's copy is coming up on 0.2.1: home-2 still waits, and still answers.
		for state in [None, Some(CopyState::Starting)] {
			for host in facts.hosts.iter_mut().filter(|h| h.name == "home-1") {
				host.copies = state
					.into_iter()
					.map(|s| copy("functions", "0.2.1", s))
					.collect();
			}
			let waiting = decide(&spec, &facts);
			assert!(
				waiting.table["control-plane/functions"]
					.iter()
					.any(|r| r.host == "home-1"
						&& r.running == 0 && r.starting == 1
						&& r.replacing == 0),
				"once the old copy is gone, the new one is named starting (no longer replacing), for a door to wait on: {:?}",
				waiting.table
			);
			assert_eq!(version(&waiting, "home-2").as_deref(), Some("0.2.1-d5"));
			assert_eq!(routed(&waiting), ["home-2:0.2.1-d5:1", "remote-1:0.2.1:1"]);
		}

		// It runs: now home-2 changes, with home-1 answering in the home region.
		for host in facts.hosts.iter_mut().filter(|h| h.name == "home-1") {
			host.copies = vec![copy("functions", "0.2.1", CopyState::Running)];
		}
		let second = decide(&spec, &facts);
		assert_eq!(version(&second, "home-2").as_deref(), Some("0.2.1"));
		assert_eq!(routed(&second), ["home-1:0.2.1:1", "remote-1:0.2.1:1"]);
		assert!(
			second
				.notes
				.contains(&"control-plane/functions: a copy of 0.2.1 placed on home-2".into())
		);

		// A copy added on another host and not yet running holds every change: the old ones stay.
		let spec = control_plane(vec![service("rest", 2, &[("v14.13", 1)])]);
		let facts = all_hosts_alive(&[
			("home-1", vec![copy("rest", "v14.12", CopyState::Running)]),
			("home-2", vec![copy("rest", "v14.12", CopyState::Running)]),
		]);
		let surge = decide(&spec, &facts);
		assert_eq!(
			on(&surge, "home-1", "rest") + on(&surge, "home-2", "rest"),
			2
		);
		assert_eq!(
			routed_to(&surge, "control-plane/rest").len(),
			2,
			"{:?}",
			surge.table
		);
	}

	#[test]
	fn placement_keeps_a_service_on_its_hosts_and_one_copy_per_host_when_the_agents_run_it() {
		let mut auth = service("auth", 2, &[("0.1.11", 1)]);
		auth.placement.hosts = vec!["home-1".into(), "home-2".into()];
		auth.run = Some(crate::spec::RunSpec {
			image_repo: "r/auth".into(),
			port: 9999,
			host_port: 9102,
			memory_mb: 128,
			..Default::default()
		});
		let spec = control_plane(vec![auth.clone()]);
		spec.check().expect("checks");
		let outcome = decide(&spec, &all_hosts_alive(&[]));
		assert_eq!(on(&outcome, "home-1", "auth"), 1);
		assert_eq!(on(&outcome, "home-2", "auth"), 1);
		assert_eq!(on(&outcome, "remote-1", "auth"), 0);

		// home-2 silent: its copy is not doubled onto home-1, nor sent to remote-1.
		let mut facts = all_hosts_alive(&[]);
		facts.hosts.retain(|h| h.name != "home-2");
		facts.hosts.push(silent("home-2", vec![]));
		let outcome = decide(&spec, &facts);
		assert_eq!(on(&outcome, "home-1", "auth"), 1);
		assert_eq!(on(&outcome, "remote-1", "auth"), 0);
		assert!(
			outcome
				.notes
				.iter()
				.any(|n| n.contains("wanted 2 copies of 0.1.11, placed 1")),
			"{:?}",
			outcome.notes
		);

		// By region instead of by name: the same two.
		auth.placement.hosts.clear();
		auth.placement.regions = vec!["region-a".into()];
		let outcome = decide(&control_plane(vec![auth]), &all_hosts_alive(&[]));
		assert_eq!(on(&outcome, "remote-1", "auth"), 0);
		assert_eq!(
			on(&outcome, "home-1", "auth") + on(&outcome, "home-2", "auth"),
			2
		);
	}

	#[test]
	fn a_service_not_routed_runs_and_is_left_out_of_the_table() {
		let mut rest = service("rest", 1, &[("v14.12", 1)]);
		rest.routed = false;
		let spec = control_plane(vec![rest]);
		let placed = decide(&spec, &all_hosts_alive(&[]));
		let host = placed.placement[0].host.clone();
		let facts = all_hosts_alive(&[(
			host.as_str(),
			vec![copy("rest", "v14.12", CopyState::Running)],
		)]);
		let outcome = decide(&spec, &facts);
		assert_eq!(on(&outcome, &host, "rest"), 1);
		assert!(outcome.table["control-plane/rest"].is_empty());
	}

	#[test]
	fn a_placement_or_a_shared_namespace_that_cannot_hold_is_refused() {
		let run = || crate::spec::RunSpec {
			image_repo: "r/x".into(),
			port: 1,
			host_port: 9101,
			memory_mb: 64,
			..Default::default()
		};
		let mut nowhere = service("auth", 1, &[("a", 1)]);
		nowhere.placement.hosts = vec!["gone-1".into()];
		assert!(
			control_plane(vec![nowhere])
				.check()
				.unwrap_err()
				.contains("not a host")
		);
		let mut member_only = service("auth", 1, &[("a", 1)]);
		member_only.placement.hosts = vec!["member-1".into()];
		assert!(
			control_plane(vec![member_only])
				.check()
				.unwrap_err()
				.contains("serves nothing")
		);
		let mut disjoint = service("auth", 1, &[("a", 1)]);
		disjoint.placement.hosts = vec!["home-1".into()];
		disjoint.placement.regions = vec!["region-c".into()];
		assert!(
			control_plane(vec![disjoint])
				.check()
				.unwrap_err()
				.contains("both")
		);
		let mut crowded = service("auth", 3, &[("a", 1)]);
		crowded.placement.regions = vec!["region-a".into()];
		crowded.run = Some(run());
		assert!(
			control_plane(vec![crowded])
				.check()
				.unwrap_err()
				.contains("only 2 hosts")
		);

		let mut router = service("lepis-router", 2, &[("0.1.13", 1)]);
		router.placement.regions = vec!["region-a".into()];
		router.run = Some(crate::spec::RunSpec {
			host_port: 0,
			..run()
		});
		let mut auth = router.clone();
		auth.name = "auth".into();
		auth.run = Some(crate::spec::RunSpec {
			joins: Some("lepis-router".into()),
			..run()
		});
		assert!(
			control_plane(vec![router.clone(), auth.clone()])
				.check()
				.is_ok()
		);
		let mut elsewhere = auth.clone();
		elsewhere.placement.regions = vec!["region-c".into()];
		elsewhere.copies = 1;
		assert!(
			control_plane(vec![router.clone(), elsewhere])
				.check()
				.unwrap_err()
				.contains("same copies and placement")
		);
		let mut chained = auth.clone();
		chained.name = "rest".into();
		chained.run = Some(crate::spec::RunSpec {
			joins: Some("auth".into()),
			..run()
		});
		assert!(
			control_plane(vec![router.clone(), auth.clone(), chained])
				.check()
				.unwrap_err()
				.contains("joins another itself")
		);
		let mut no_owner = auth.clone();
		no_owner.run = Some(crate::spec::RunSpec {
			joins: Some("nobody".into()),
			..run()
		});
		assert!(
			control_plane(vec![no_owner])
				.check()
				.unwrap_err()
				.contains("not a service")
		);

		let mut bad = router.clone();
		bad.run = Some(crate::spec::RunSpec {
			host_port: 0,
			health_path: Some("/".into()),
			..run()
		});
		assert!(
			control_plane(vec![bad])
				.check()
				.unwrap_err()
				.contains("published port")
		);
		let mut path = router.clone();
		path.run
			.as_mut()
			.expect("run")
			.secret_env
			.insert("PASSWORD".into(), "../etc/shadow".into());
		assert!(
			control_plane(vec![path])
				.check()
				.unwrap_err()
				.contains("not a host secret file name")
		);
		let mut twice = router.clone();
		let r = twice.run.as_mut().expect("run");
		r.env.insert("PASSWORD".into(), "visible".into());
		r.secret_env.insert("PASSWORD".into(), "file".into());
		assert!(
			control_plane(vec![twice])
				.check()
				.unwrap_err()
				.contains("set twice")
		);
		let mut mount = router;
		mount
			.run
			.as_mut()
			.expect("run")
			.files
			.insert("relative/ca.pem".into(), "ca.pem".into());
		assert!(
			control_plane(vec![mount])
				.check()
				.unwrap_err()
				.contains("path to mount")
		);
	}

	/// A fleet project's service: the environment is its ref.
	fn of(environment: &str, name: &str, copies: u32) -> ServiceSpec {
		let mut s = service(name, copies, &[("1.0.0", 1)]);
		s.environment = environment.into();
		s
	}

	fn running_of(environment: &str, name: &str) -> CopyFacts {
		let mut c = copy(name, "1.0.0", CopyState::Running);
		c.environment = environment.into();
		c
	}

	#[test]
	fn a_frozen_host_keeps_what_runs_there_and_is_given_nothing_new() {
		// H2 is full and must not change. Its copy stays placed and routed; a service wanting
		// one more copy places it elsewhere, never on h2; the copies wanted going down keeps h2's.
		let mut frozen = host("h2", "r", "z2");
		frozen.serves = false;
		frozen.frozen = true;
		// Its real capacity, as the spec carries it: what runs there is kept while it fits.
		let spec = |copies: u32| Spec {
			host_timeout_secs: 30,
			hosts: vec![host("h1", "r", "z1"), frozen.clone(), host("h3", "r", "z3")],
			services: vec![service("auth", copies, &[("0.1.11", 1)])],
			database_cluster: None,
			agents: Vec::new(),
			pressure_percent: None,
			rebalance: None,
		};
		let running = || copy("auth", "0.1.11", CopyState::Running);
		let facts = Facts {
			now_ms: NOW,
			hosts: vec![
				alive("h1", vec![running()]),
				alive("h2", vec![running()]),
				alive("h3", Vec::new()),
			],
			database_primary_region: None,
			database_hosts: Default::default(),
			moves: Vec::new(),
			rollbacks: Default::default(),
		};
		let kept = decide(&spec(2), &facts);
		assert_eq!(
			(
				on(&kept, "h1", "auth"),
				on(&kept, "h2", "auth"),
				on(&kept, "h3", "auth")
			),
			(1, 1, 0)
		);
		let more = decide(&spec(3), &facts);
		assert_eq!(
			(
				on(&more, "h1", "auth"),
				on(&more, "h2", "auth"),
				on(&more, "h3", "auth")
			),
			(1, 1, 1)
		);
		let fewer = decide(&spec(1), &facts);
		assert_eq!(
			on(&fewer, "h2", "auth"),
			1,
			"the frozen host's copy is the one kept"
		);
		assert_eq!(on(&fewer, "h1", "auth") + on(&fewer, "h3", "auth"), 0);
		// Nothing new on it: a service with no copy there never lands there.
		let empty = Facts {
			hosts: vec![
				alive("h1", Vec::new()),
				alive("h2", Vec::new()),
				alive("h3", Vec::new()),
			],
			..facts.clone()
		};
		assert_eq!(on(&decide(&spec(3), &empty), "h2", "auth"), 0);
		// Lost, it is a host lost: its copy placed elsewhere.
		let lost = Facts {
			hosts: vec![alive("h1", vec![running()]), alive("h3", Vec::new())],
			..facts.clone()
		};
		let after = decide(&spec(2), &lost);
		assert_eq!(
			(
				on(&after, "h1", "auth"),
				on(&after, "h2", "auth"),
				on(&after, "h3", "auth")
			),
			(1, 0, 1)
		);
	}

	#[test]
	fn a_finishing_host_lets_go_only_of_a_copy_whose_replacement_runs_steady() {
		// H2's drain was withdrawn after h3's copy began. Nothing moves back and nothing more
		// moves off: with no replacement h2 keeps its copy; with one starting or running, that one is
		// kept and h2's is held only until it runs steady, then let go.
		let mut finishing = host("h2", "r", "z2");
		finishing.serves = false;
		finishing.frozen = true;
		finishing.finishing = true;
		let spec = Spec {
			host_timeout_secs: 30,
			hosts: vec![host("h1", "r", "z1"), finishing, host("h3", "r", "z3")],
			services: vec![service("auth", 2, &[("0.1.11", 1)])],
			database_cluster: None,
			agents: Vec::new(),
			pressure_percent: None,
			rebalance: None,
		};
		let since = |state: CopyState, ms: Option<u64>| CopyFacts {
			running_since_ms: ms,
			..copy("auth", "0.1.11", state)
		};
		let old = || since(CopyState::Running, Some(NOW - 600_000));
		let facts = |h3: Vec<CopyFacts>| Facts {
			now_ms: NOW,
			hosts: vec![
				alive("h1", vec![old()]),
				alive("h2", vec![old()]),
				alive("h3", h3),
			],
			database_primary_region: None,
			database_hosts: Default::default(),
			moves: Vec::new(),
			rollbacks: Default::default(),
		};
		let at = |o: &Outcome| {
			(
				on(o, "h1", "auth"),
				on(o, "h2", "auth"),
				on(o, "h3", "auth"),
			)
		};

		assert_eq!(
			at(&decide(&spec, &facts(Vec::new()))),
			(1, 1, 0),
			"no replacement: nothing moves off"
		);
		assert_eq!(
			at(&decide(
				&spec,
				&facts(vec![since(CopyState::Starting, None)])
			)),
			(1, 1, 1),
			"a replacement starting is kept, and h2 holds until it is steady"
		);
		assert_eq!(
			at(&decide(
				&spec,
				&facts(vec![since(CopyState::Running, Some(NOW - 1_000))])
			)),
			(1, 1, 1),
			"running, not steady yet: h2 still holds"
		);
		let done = decide(&spec, &facts(vec![old()]));
		assert_eq!(
			at(&done),
			(1, 0, 1),
			"steady: h2's copy is let go, never the new one"
		);
		let routed: Vec<&str> = routed_to(&done, "control-plane/auth")
			.iter()
			.map(|r| r.host.as_str())
			.collect();
		assert_eq!(routed, ["h1", "h3"]);
	}

	#[test]
	fn a_draining_host_keeps_its_copy_until_the_one_replacing_it_runs_steady() {
		// Packing: h2 is being emptied. Its copy is placed elsewhere first and let go only
		// once that copy has run STEADY_MS, so the service never has fewer than its copies.
		let mut draining = host("h2", "r", "z2");
		draining.serves = false;
		draining.draining = true;
		let spec = Spec {
			host_timeout_secs: 30,
			hosts: vec![host("h1", "r", "z1"), draining, host("h3", "r", "z3")],
			services: vec![service("auth", 2, &[("0.1.11", 1)])],
			database_cluster: None,
			agents: Vec::new(),
			pressure_percent: None,
			rebalance: None,
		};
		let since = |state: CopyState, ms: Option<u64>| CopyFacts {
			running_since_ms: ms,
			..copy("auth", "0.1.11", state)
		};
		let facts = |h3: Vec<CopyFacts>| Facts {
			now_ms: NOW,
			hosts: vec![
				alive("h1", vec![since(CopyState::Running, Some(NOW - 600_000))]),
				alive("h2", vec![since(CopyState::Running, Some(NOW - 600_000))]),
				alive("h3", h3),
			],
			database_primary_region: None,
			database_hosts: Default::default(),
			moves: Vec::new(),
			rollbacks: Default::default(),
		};
		let total = |o: &Outcome| o.placement.iter().map(|a| a.count).sum::<u32>();

		// The replacement placed on h3; h2 keeps its copy, still routed.
		let first = decide(&spec, &facts(Vec::new()));
		assert_eq!(
			(
				on(&first, "h1", "auth"),
				on(&first, "h2", "auth"),
				on(&first, "h3", "auth")
			),
			(1, 1, 1)
		);
		assert_eq!(total(&first), 3, "one more than its copies while it moves");
		let routed: Vec<&str> = routed_to(&first, "control-plane/auth")
			.iter()
			.map(|r| r.host.as_str())
			.collect();
		assert_eq!(
			routed,
			["h1", "h2"],
			"the draining copy answers until the new one is steady"
		);
		assert!(
			first
				.notes
				.iter()
				.any(|n| n.contains("h2 keeps its copy of 0.1.11 while it drains"))
		);

		// Running, but not for STEADY_MS yet: still kept.
		let young = decide(
			&spec,
			&facts(vec![since(
				CopyState::Running,
				Some(NOW - STEADY_MS + 1_000),
			)]),
		);
		assert_eq!(on(&young, "h2", "auth"), 1);
		// An agent that does not say since when: never steady, so still kept.
		let unsaid = decide(&spec, &facts(vec![since(CopyState::Running, None)]));
		assert_eq!(on(&unsaid, "h2", "auth"), 1);

		// Steady: h2 lets it go, and the service is back at its copies.
		let steady = decide(
			&spec,
			&facts(vec![since(CopyState::Running, Some(NOW - STEADY_MS))]),
		);
		assert_eq!(
			(
				on(&steady, "h1", "auth"),
				on(&steady, "h2", "auth"),
				on(&steady, "h3", "auth")
			),
			(1, 0, 1)
		);
		assert_eq!(total(&steady), 2);

		// Nothing is ever placed on a draining host that runs nothing there.
		let empty = Facts {
			now_ms: NOW,
			hosts: vec![
				alive("h1", Vec::new()),
				alive("h2", Vec::new()),
				alive("h3", Vec::new()),
			],
			database_primary_region: None,
			database_hosts: Default::default(),
			moves: Vec::new(),
			rollbacks: Default::default(),
		};
		assert_eq!(on(&decide(&spec, &empty), "h2", "auth"), 0);
	}

	#[test]
	fn a_running_copy_is_never_moved_to_make_room_for_a_new_one() {
		// Room for one copy on each host. `aaa`'s copy is new and sorts first; `bbb`'s runs on h2.
		let mut hosts = vec![host("h1", "r", "z1"), host("h2", "r", "z2")];
		for h in &mut hosts {
			h.capacity.memory_mb = 256;
		}
		hosts[0].capacity.memory_mb = 0;
		let spec = Spec {
			host_timeout_secs: 30,
			hosts,
			services: vec![of("aaa", "auth", 1), of("bbb", "auth", 1)],
			database_cluster: None,
			agents: Vec::new(),
			pressure_percent: None,
			rebalance: None,
		};
		let facts = Facts {
			now_ms: NOW,
			hosts: vec![
				alive("h1", vec![]),
				alive("h2", vec![running_of("bbb", "auth")]),
			],
			..Default::default()
		};
		let outcome = decide(&spec, &facts);
		assert_eq!(
			outcome.placement,
			[Assignment {
				host: "h2".into(),
				environment: "bbb".into(),
				service: "auth".into(),
				version: "1.0.0".into(),
				count: 1,
			}]
		);
		assert!(outcome.notes.iter().any(
			|n| n.starts_with("aaa/auth: wanted 1") && n.contains("no eligible host has room")
		));
	}

	#[test]
	fn copies_stay_off_their_own_database_host_where_another_has_room() {
		let spec = Spec {
			host_timeout_secs: 30,
			hosts: vec![
				host("h1", "region-c", "z1"),
				host("h2", "region-c", "z2"),
				host("h3", "region-c", "z3"),
			],
			services: vec![of("proja", "auth", 2), of("projb", "auth", 2)],
			database_cluster: None,
			agents: Vec::new(),
			pressure_percent: None,
			rebalance: None,
		};
		let mut facts = Facts {
			now_ms: NOW,
			hosts: vec![
				alive("h1", vec![]),
				alive("h2", vec![]),
				alive("h3", vec![]),
			],
			..Default::default()
		};
		facts.database_hosts.insert("proja".into(), "h1".into());
		facts.database_hosts.insert("projb".into(), "h3".into());
		let outcome = decide(&spec, &facts);
		let hosts_of = |env: &str| -> Vec<&str> {
			outcome
				.placement
				.iter()
				.filter(|a| a.environment == env)
				.map(|a| a.host.as_str())
				.collect()
		};
		assert_eq!(hosts_of("proja"), ["h2", "h3"]);
		assert_eq!(hosts_of("projb"), ["h1", "h2"]);

		// With only two hosts, one copy goes on the database's host: a copy beside it beats none.
		let mut two = spec.clone();
		two.hosts.pop();
		two.services.pop();
		let outcome = decide(&two, &facts);
		assert_eq!(on(&outcome, "h1", "auth") + on(&outcome, "h2", "auth"), 2);
	}

	#[test]
	fn a_projects_services_are_placed_on_one_set_of_hosts() {
		// auth takes the two roomiest hosts, which leaves another host the roomiest for the router;
		// the project's copies would name three hosts for two copies, and the control plane refuses
		// that placement ("more hosts than its copies").
		let sized = |name: &str, domain: &str, mb: u32| {
			let mut h = host(name, "region-c", domain);
			h.capacity.memory_mb = mb;
			h
		};
		let mut auth = of("proj", "auth", 2);
		auth.per_copy.memory_mb = 128;
		let mut lepis = of("proj", "lepis", 2);
		lepis.per_copy.memory_mb = 64;
		let spec = fleet(
			vec![
				sized("ha", "z1", 2941),
				sized("hb", "z1", 2941),
				sized("hc", "z2", 2371),
				sized("hd", "z1", 2371),
			],
			vec![auth, lepis],
		);
		let mut facts = Facts {
			now_ms: NOW,
			hosts: vec![
				alive("ha", vec![]),
				alive("hb", vec![]),
				alive("hc", vec![]),
				alive("hd", vec![]),
			],
			..Default::default()
		};
		facts.database_hosts.insert("proj".into(), "hd".into());
		let outcome = decide(&spec, &facts);
		let hosts_of = |service: &str| -> BTreeSet<&str> {
			outcome
				.placement
				.iter()
				.filter(|a| a.environment == "proj" && a.service == service)
				.map(|a| a.host.as_str())
				.collect()
		};
		assert_eq!(hosts_of("auth").len(), 2, "{:?}", outcome.placement);
		assert_eq!(
			hosts_of("auth"),
			hosts_of("lepis"),
			"{:?}",
			outcome.placement
		);
	}

	#[test]
	fn a_closed_home_takes_its_own_projects_copies_and_no_others() {
		// H1 is closed (serves false) and homes proja; h2 serves; projb's home is elsewhere.
		let mut closed = host("h1", "region-c", "z1");
		closed.serves = false;
		closed.serves_home = true;
		let spec = fleet(
			vec![closed, host("h2", "region-c", "z2")],
			vec![of("proja", "auth", 2), of("projb", "auth", 2)],
		);
		let mut facts = Facts {
			now_ms: NOW,
			hosts: vec![alive("h1", vec![]), alive("h2", vec![])],
			..Default::default()
		};
		facts.database_hosts.insert("proja".into(), "h1".into());
		facts.database_hosts.insert("projb".into(), "h9".into());
		spec.check().expect("a closed home's spec reads");
		let outcome = decide(&spec, &facts);
		let on_env = |env: &str, host: &str| -> u32 {
			outcome
				.placement
				.iter()
				.filter(|a| a.environment == env && a.host == host)
				.map(|a| a.count)
				.sum()
		};
		assert_eq!(
			(on_env("proja", "h1"), on_env("proja", "h2")),
			(1, 1),
			"its home and h2"
		);
		assert_eq!(
			on_env("projb", "h1"),
			0,
			"another project's copy on a closed home"
		);
		assert_eq!(on_env("projb", "h2"), 1);

		// Without the flag a closed home takes nothing, its own project's copy included.
		let mut plain = spec.clone();
		plain.hosts[0].serves_home = false;
		let outcome = decide(&plain, &facts);
		assert_eq!(
			outcome.placement.iter().filter(|a| a.host == "h1").count(),
			0
		);

		// Draining wins: a closed home that is draining takes nothing either.
		let mut draining = spec.clone();
		draining.hosts[0].draining = true;
		let outcome = decide(&draining, &facts);
		assert_eq!(
			outcome.placement.iter().filter(|a| a.host == "h1").count(),
			0
		);
	}

	fn packed(environment: &str, copies: u32, per_host: u32) -> ServiceSpec {
		let mut s = of(environment, "auth", copies);
		s.policy = crate::spec::Policy {
			mode: PlaceMode::Pack,
			max_per_host: per_host,
		};
		s
	}

	fn fleet(hosts: Vec<HostSpec>, services: Vec<ServiceSpec>) -> Spec {
		Spec {
			host_timeout_secs: 30,
			hosts,
			services,
			database_cluster: None,
			agents: Vec::new(),
			pressure_percent: None,
			rebalance: None,
		}
	}

	#[test]
	fn a_packing_project_fills_its_home_first_then_another_host() {
		// A packing plan packs, two to a host, and the home (its database's host) first.
		let spec = fleet(
			vec![host("h1", "region-c", "z1"), host("h2", "region-c", "z2")],
			vec![packed("proja", 2, 2)],
		);
		let mut facts = Facts {
			now_ms: NOW,
			hosts: vec![alive("h1", vec![]), alive("h2", vec![])],
			..Default::default()
		};
		facts.database_hosts.insert("proja".into(), "h2".into());
		let outcome = decide(&spec, &facts);
		assert_eq!(on(&outcome, "h2", "auth"), 2, "both on the home");
		assert_eq!(on(&outcome, "h1", "auth"), 0);

		// A home with room for one: the other goes to the next host.
		let mut tight = spec.clone();
		tight.hosts[1].capacity.memory_mb = 300;
		let outcome = decide(&tight, &facts);
		assert_eq!(on(&outcome, "h2", "auth"), 1);
		assert_eq!(on(&outcome, "h1", "auth"), 1);
		// Several on one host only at home: past the home's room, one per host, never two on h1.
		let three = fleet(tight.hosts.clone(), vec![packed("proja", 3, 3)]);
		let outcome = decide(&three, &facts);
		assert_eq!(
			(on(&outcome, "h2", "auth"), on(&outcome, "h1", "auth")),
			(1, 1)
		);

		// One to a host: a pack of one is the home, then the next host, never two on one.
		let one = fleet(spec.hosts.clone(), vec![packed("proja", 2, 1)]);
		let outcome = decide(&one, &facts);
		assert_eq!(
			(on(&outcome, "h2", "auth"), on(&outcome, "h1", "auth")),
			(1, 1)
		);

		// Spread, the same project: one per host, off the home where another has room.
		let spread = fleet(
			vec![
				host("h1", "region-c", "z1"),
				host("h2", "region-c", "z2"),
				host("h3", "region-c", "z3"),
			],
			vec![of("proja", "auth", 2)],
		);
		facts.hosts.push(alive("h3", vec![]));
		let outcome = decide(&spread, &facts);
		assert_eq!(
			(
				on(&outcome, "h1", "auth"),
				on(&outcome, "h2", "auth"),
				on(&outcome, "h3", "auth")
			),
			(1, 0, 1)
		);
	}

	#[test]
	fn a_packing_projects_copy_elsewhere_moves_home_only_once_the_home_runs_steady() {
		// A project scaled before packing has a copy on h1; its home h2 has room. The copy is
		// placed on the home and h1 keeps its own until the home's has run STEADY_MS.
		let spec = fleet(
			vec![host("h1", "region-c", "z1"), host("h2", "region-c", "z2")],
			vec![packed("proja", 2, 2)],
		);
		let running = |since: Option<u64>| {
			let mut c = running_of("proja", "auth");
			c.running_since_ms = since;
			c
		};
		let facts_with = |home: Vec<CopyFacts>| {
			let mut f = Facts {
				now_ms: NOW,
				hosts: vec![
					alive("h1", vec![running(Some(NOW - 600_000))]),
					alive("h2", home),
				],
				..Default::default()
			};
			f.database_hosts.insert("proja".into(), "h2".into());
			f
		};
		// The home runs one; a second is placed there, and h1 keeps its copy meanwhile.
		let outcome = decide(&spec, &facts_with(vec![running(Some(NOW - 600_000))]));
		assert_eq!(on(&outcome, "h2", "auth"), 2);
		assert_eq!(
			on(&outcome, "h1", "auth"),
			1,
			"kept until the home's run steady"
		);
		assert!(outcome.notes.iter().any(|n| {
			n.contains("h1 keeps its copy of 1.0.0 until the one on its home, h2, runs steady")
		}));
		// Both run at home, the second not steady yet: still kept.
		let young = decide(
			&spec,
			&facts_with(vec![
				running(Some(NOW - 600_000)),
				running(Some(NOW - STEADY_MS + 1_000)),
			]),
		);
		assert_eq!(on(&young, "h1", "auth"), 1);
		// Both steady at home: h1's is let go.
		let done = decide(
			&spec,
			&facts_with(vec![
				running(Some(NOW - 600_000)),
				running(Some(NOW - STEADY_MS)),
			]),
		);
		assert_eq!((on(&done, "h2", "auth"), on(&done, "h1", "auth")), (2, 0));
		assert!(
			done.notes
				.iter()
				.any(|n| n.contains("h1's copy of 1.0.0 moved to its home, h2"))
		);
	}

	#[test]
	fn each_copy_kept_on_a_host_needs_its_own_room_and_the_rest_wait_for_their_replacements() {
		// The home (h2) shrank to room for one copy while two of the project's run there. One is kept;
		// the other is placed on h1 and held on h2, never stopped, until h1's runs steady (Joel,
		// 2026-11-17: two copies were kept in room for one, the host read full and nothing short).
		let spec = fleet(
			vec![host("h1", "region-c", "z1"), host("h2", "region-c", "z2")],
			vec![packed("proja", 2, 2)],
		);
		let mut spec = spec;
		spec.hosts[1].capacity.memory_mb = 300;
		let running = |since: u64| {
			let mut c = running_of("proja", "auth");
			c.running_since_ms = Some(since);
			c
		};
		let facts_with = |h1: Vec<CopyFacts>| {
			let mut f = Facts {
				now_ms: NOW,
				hosts: vec![
					alive("h1", h1),
					alive("h2", vec![running(NOW - 600_000), running(NOW - 600_000)]),
				],
				..Default::default()
			};
			f.database_hosts.insert("proja".into(), "h2".into());
			f
		};
		let outcome = decide(&spec, &facts_with(vec![]));
		assert_eq!(on(&outcome, "h1", "auth"), 1, "{:?}", outcome.notes);
		assert_eq!(
			on(&outcome, "h2", "auth"),
			2,
			"the second held until h1's runs steady"
		);
		assert!(
			outcome.notes.iter().any(|n| n.contains(
				"h2 has no room for its copy of 1.0.0; it stays until the copies elsewhere run steady"
			)),
			"{:?}",
			outcome.notes
		);
		assert!(outcome.shortfalls.is_empty(), "{:?}", outcome.shortfalls);

		// h1's runs, not steady yet: still held.
		let young = decide(&spec, &facts_with(vec![running(NOW - STEADY_MS + 1_000)]));
		assert_eq!((on(&young, "h1", "auth"), on(&young, "h2", "auth")), (1, 2));
		// Steady: h2 keeps the one it has room for.
		let done = decide(&spec, &facts_with(vec![running(NOW - STEADY_MS)]));
		assert_eq!((on(&done, "h1", "auth"), on(&done, "h2", "auth")), (1, 1));
		assert!(
			done.notes
				.iter()
				.any(|n| n
					.contains("h2's copy of 1.0.0 moved off it, since its room there is taken"))
		);

		// Nowhere else has room: both stay where they run, and the shortfall says one is unplaced.
		let alone = fleet(vec![spec.hosts[1].clone()], vec![packed("proja", 2, 2)]);
		let mut f = facts_with(vec![]);
		f.hosts.remove(0);
		let outcome = decide(&alone, &f);
		assert_eq!(
			on(&outcome, "h2", "auth"),
			2,
			"never stopped for want of room"
		);
		assert_eq!(outcome.shortfalls.len(), 1);
		assert_eq!(
			(outcome.shortfalls[0].wanted, outcome.shortfalls[0].placed),
			(2, 1)
		);
	}

	#[test]
	fn the_table_prefers_each_projects_own_database_region() {
		let spec = Spec {
			host_timeout_secs: 30,
			hosts: vec![host("west", "region-c", "a"), host("east", "region-b", "b")],
			services: vec![of("proja", "auth", 2), of("projb", "auth", 2)],
			database_cluster: None,
			agents: Vec::new(),
			pressure_percent: None,
			rebalance: None,
		};
		let both = vec![running_of("proja", "auth"), running_of("projb", "auth")];
		let mut facts = Facts {
			now_ms: NOW,
			hosts: vec![alive("west", both.clone()), alive("east", both)],
			database_primary_region: Some("region-b".into()),
			..Default::default()
		};
		facts.database_hosts.insert("proja".into(), "west".into());
		let outcome = decide(&spec, &facts);
		let near = |id: &str| -> Vec<&str> {
			outcome.table[id]
				.iter()
				.filter(|r| r.near_database)
				.map(|r| r.host.as_str())
				.collect()
		};
		assert_eq!(near("proja/auth"), ["west"]);
		// No project primary known: the one primary region of the facts, as before.
		assert_eq!(near("projb/auth"), ["east"]);
		// A database host the spec does not name is no host: the facts' region again.
		facts.database_hosts.insert("projb".into(), "gone".into());
		assert_eq!(
			decide(&spec, &facts).table["projb/auth"]
				.iter()
				.filter(|r| r.near_database)
				.count(),
			1
		);
	}

	/// The fleet's shape at a size well past today's: 400 spread projects of four services, two
	/// copies each, on 30 hosts in three zones. Every copy placed on two hosts, none over its
	/// memory, none on its project's database host, and the decision after, with every copy
	/// running, moves nothing.
	#[test]
	fn a_fleet_decision_places_every_project_and_then_holds() {
		let hosts: Vec<HostSpec> = (0..30)
			.map(|i| {
				let mut h = host(
					&format!("pod-{i:02}"),
					"region-c",
					&format!("region-c{}", ["a", "b", "c"][i % 3]),
				);
				h.capacity = Resources {
					millicpu: 0,
					memory_mb: 32_000,
				};
				h
			})
			.collect();
		let mut services = Vec::new();
		let mut database_hosts = BTreeMap::new();
		for p in 0..400 {
			let env = format!("p{p:04}");
			for name in ["auth", "rest", "psyche", "lepis"] {
				// As `cloud_metis_fleet_spec()` renders them: memory, no CPU yet.
				let mut s = of(&env, name, 2);
				s.per_copy.millicpu = 0;
				services.push(s);
			}
			database_hosts.insert(env, hosts[p % 30].name.clone());
		}
		let spec = Spec {
			host_timeout_secs: 60,
			hosts: hosts.clone(),
			services,
			database_cluster: None,
			agents: Vec::new(),
			pressure_percent: None,
			rebalance: None,
		};
		spec.check().expect("the fleet spec is one");
		let empty = Facts {
			now_ms: NOW,
			hosts: hosts.iter().map(|h| alive(&h.name, vec![])).collect(),
			database_hosts: database_hosts.clone(),
			..Default::default()
		};
		let started = std::time::Instant::now();
		let first = decide(&spec, &empty);
		let took = started.elapsed();
		assert_eq!(
			first.placement.iter().map(|a| a.count).sum::<u32>(),
			400 * 4 * 2
		);
		for service in &spec.services {
			let on: Vec<&Assignment> = first
				.placement
				.iter()
				.filter(|a| a.environment == service.environment && a.service == service.name)
				.collect();
			assert_eq!(on.len(), 2, "{} on two hosts", service.id());
			assert!(
				on.iter()
					.all(|a| Some(&a.host) != database_hosts.get(&service.environment))
			);
		}
		for h in &hosts {
			let used: u32 = first
				.placement
				.iter()
				.filter(|a| a.host == h.name)
				.map(|a| a.count * 256)
				.sum();
			assert!(used <= h.capacity.memory_mb, "{} holds {used} MB", h.name);
		}
		let running = Facts {
			now_ms: NOW,
			hosts: hosts
				.iter()
				.map(|h| {
					alive(
						&h.name,
						first
							.placement
							.iter()
							.filter(|a| a.host == h.name)
							.flat_map(|a| {
								(0..a.count).map(|_| running_of(&a.environment, &a.service))
							})
							.collect(),
					)
				})
				.collect(),
			database_hosts,
			..Default::default()
		};
		let started = std::time::Instant::now();
		let then = decide(&spec, &running);
		let took_then = started.elapsed();
		assert_eq!(then.placement, first.placement);
		assert!(
			then.notes.is_empty(),
			"{:?}",
			&then.notes[..then.notes.len().min(3)]
		);
		assert!(
			then.table
				.values()
				.all(|routes| routes.iter().map(|r| r.running).sum::<u32>() == 2)
		);
		eprintln!("fleet decision: {took:?} placing, {took_then:?} holding");
	}
}
