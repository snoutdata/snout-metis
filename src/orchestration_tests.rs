//! Table-driven cases for what Metis borrows from Kubernetes (MT7): requests and limits (QoS) and
//! eviction under pressure, priority (never an eviction), the rebalancer, and rollouts with a surge, more
//! than one host at a time, and a rollback. Every one is opt-in: a spec without the fields decides as
//! before (the rest of `decide.rs`'s tests).

use crate::decide::{Outcome, STEADY_MS, decide};
use crate::spec::{
	CopyFacts, CopyState, Facts, HostFacts, HostSpec, Move, Qos, Rebalance, Resources, Rollback,
	Rollout, ServiceSpec, Spec, VersionShare,
};

pub(crate) const NOW: u64 = 10_000_000;

pub(crate) fn mb(memory_mb: u32) -> Resources {
	Resources {
		millicpu: 0,
		memory_mb,
	}
}

pub(crate) fn host(name: &str, domain: &str, memory_mb: u32) -> HostSpec {
	HostSpec {
		name: name.into(),
		region: "r".into(),
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
		capacity: mb(memory_mb),
	}
}

pub(crate) fn service(environment: &str, name: &str, copies: u32, per_copy_mb: u32) -> ServiceSpec {
	ServiceSpec {
		name: name.into(),
		environment: environment.into(),
		copies,
		versions: vec![VersionShare {
			version: "1.0".into(),
			share: 1,
		}],
		per_copy: mb(per_copy_mb),
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

pub(crate) fn spec(hosts: Vec<HostSpec>, services: Vec<ServiceSpec>) -> Spec {
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

/// A copy running steady (past [`STEADY_MS`]), using `load_mb` when given.
pub(crate) fn steady(environment: &str, name: &str, version: &str, load_mb: Option<u32>) -> CopyFacts {
	CopyFacts {
		environment: environment.into(),
		service: name.into(),
		version: version.into(),
		state: CopyState::Running,
		load: load_mb.map(mb),
		running_since_ms: Some(NOW - STEADY_MS - 1),
	}
}

pub(crate) fn with_state(mut copy: CopyFacts, state: CopyState) -> CopyFacts {
	copy.state = state;
	if state != CopyState::Running {
		copy.running_since_ms = None;
	}
	copy
}

pub(crate) fn facts(hosts: Vec<(&str, Vec<CopyFacts>)>) -> Facts {
	Facts {
		now_ms: NOW,
		hosts: hosts
			.into_iter()
			.map(|(name, copies)| HostFacts {
				name: name.into(),
				last_heartbeat_ms: NOW - 1000,
				copies,
				report_only: false,
			})
			.collect(),
		..Default::default()
	}
}

pub(crate) fn on(outcome: &Outcome, host: &str, environment: &str) -> u32 {
	outcome
		.placement
		.iter()
		.filter(|a| a.host == host && a.environment == environment)
		.map(|a| a.count)
		.sum()
}

fn version_on(outcome: &Outcome, host: &str, version: &str) -> u32 {
	outcome
		.placement
		.iter()
		.filter(|a| a.host == host && a.version == version)
		.map(|a| a.count)
		.sum()
}

pub(crate) fn said(outcome: &Outcome, part: &str) -> bool {
	outcome.notes.iter().any(|n| n.contains(part))
}

// ---- Requests, limits and QoS ----

#[test]
fn a_service_is_classed_from_what_it_reserves_and_its_limit() {
	let mut s = service("a", "x", 1, 0);
	assert_eq!(s.qos(), Qos::BestEffort, "reserves nothing");
	s.per_copy = mb(256);
	assert_eq!(s.qos(), Qos::Burstable, "no limit");
	s.limit = Some(mb(512));
	assert_eq!(s.qos(), Qos::Burstable, "a limit above what it reserves");
	s.limit = Some(mb(256));
	assert_eq!(s.qos(), Qos::Guaranteed, "its limit is what it reserves");
}

#[test]
fn the_new_fields_are_checked_and_never_written_when_absent() {
	let base = || spec(vec![host("h1", "a", 1024)], vec![service("a", "x", 1, 256)]);
	assert!(base().check().is_ok());
	let written = serde_json::to_value(base()).expect("json");
	for field in ["pressure_percent", "rebalance"] {
		assert!(written.get(field).is_none(), "{field}");
	}
	for field in ["limit", "priority", "rollout"] {
		assert!(written["services"][0].get(field).is_none(), "{field}");
	}

	let mut below = base();
	below.services[0].limit = Some(mb(128));
	assert!(
		below
			.check()
			.unwrap_err()
			.contains("below what a copy reserves")
	);

	let mut run = base();
	run.services[0].limit = Some(mb(512));
	run.services[0].run = Some(crate::spec::RunSpec {
		image_repo: "repo".into(),
		port: 9000,
		host_port: 9000,
		memory_mb: 256,
		..Default::default()
	});
	assert!(run.check().unwrap_err().contains("its agent runs it with"));

	let mut priority = base();
	priority.services[0].priority = crate::spec::MAX_PRIORITY + 1;
	assert!(priority.check().is_err());

	let mut stuck = base();
	stuck.services[0].rollout = Some(Rollout {
		max_surge: 0,
		max_unavailable: 0,
		rollback: false,
	});
	assert!(stuck.check().unwrap_err().contains("max_unavailable 0"));

	let mut band = base();
	band.rebalance = Some(Rebalance {
		high_percent: 50,
		low_percent: 60,
	});
	assert!(band.check().is_err());

	let mut pressure = base();
	pressure.pressure_percent = Some(0);
	assert!(pressure.check().is_err());

	// A rollout's defaults: one host at a time, no surge, no rollback.
	let read: Rollout = serde_json::from_value(serde_json::json!({})).expect("defaults");
	assert_eq!(
		read,
		Rollout {
			max_surge: 0,
			max_unavailable: 1,
			rollback: false
		}
	);
}

/// h1 (1000 MB) runs a best-effort, a burstable and a guaranteed copy using 1200 MB in all; h2 is
/// empty. Past 90%, the best-effort one moves first, and that is enough.
fn pressured() -> (Spec, Vec<CopyFacts>) {
	let best_effort = service("be", "x", 1, 0);
	let burstable = service("bu", "x", 1, 200);
	let mut guaranteed = service("gu", "x", 1, 300);
	guaranteed.limit = Some(mb(300));
	let mut s = spec(
		vec![host("h1", "a", 1000), host("h2", "b", 1000)],
		vec![best_effort, burstable, guaranteed],
	);
	s.pressure_percent = Some(90);
	let on_h1 = vec![
		steady("be", "x", "1.0", Some(400)),
		steady("bu", "x", "1.0", Some(500)),
		steady("gu", "x", "1.0", Some(300)),
	];
	(s, on_h1)
}

#[test]
fn a_host_under_pressure_sheds_best_effort_first_make_before_break() {
	let (s, on_h1) = pressured();
	let outcome = decide(&s, &facts(vec![("h1", on_h1.clone()), ("h2", vec![])]));
	assert!(
		said(&outcome, "be/x (best effort) moves off it"),
		"{:?}",
		outcome.notes
	);
	assert!(!said(&outcome, "bu/x (burstable)"), "one was enough");
	assert_eq!(on(&outcome, "h2", "be"), 1, "its replacement");
	assert_eq!(
		on(&outcome, "h1", "be"),
		1,
		"kept until the replacement runs steady"
	);
	assert_eq!(on(&outcome, "h1", "bu"), 1);
	assert_eq!(on(&outcome, "h1", "gu"), 1);

	// The replacement runs steady: the copy on h1 goes.
	let outcome = decide(
		&s,
		&facts(vec![
			("h1", on_h1),
			("h2", vec![steady("be", "x", "1.0", Some(400))]),
		]),
	);
	assert_eq!(on(&outcome, "h1", "be"), 0, "{:?}", outcome.notes);
	assert_eq!(on(&outcome, "h2", "be"), 1);
}

#[test]
fn a_host_under_pressure_takes_burstable_next_and_never_a_guaranteed_copy() {
	let (s, _) = pressured();
	// Without the best-effort copy, 1100 MB: the burstable one, 300 MB past its request, goes.
	let on_h1 = vec![
		steady("bu", "x", "1.0", Some(500)),
		steady("gu", "x", "1.0", Some(600)),
	];
	let outcome = decide(&s, &facts(vec![("h1", on_h1), ("h2", vec![])]));
	assert!(
		said(&outcome, "bu/x (burstable) moves off it"),
		"{:?}",
		outcome.notes
	);
	assert_eq!(on(&outcome, "h2", "bu"), 1);
	assert_eq!(on(&outcome, "h1", "gu"), 1);
	assert_eq!(on(&outcome, "h2", "gu"), 0);

	// Guaranteed copies alone past 90%: said, and nothing moves.
	let on_h1 = vec![steady("gu", "x", "1.0", Some(950))];
	let outcome = decide(&s, &facts(vec![("h1", on_h1), ("h2", vec![])]));
	assert!(
		said(&outcome, "only guaranteed copies left"),
		"{:?}",
		outcome.notes
	);
	assert_eq!(on(&outcome, "h1", "gu"), 1);
	assert_eq!(on(&outcome, "h2", "gu"), 0);
}

#[test]
fn pressure_with_nowhere_to_go_keeps_the_copy_where_it_runs() {
	let (mut s, on_h1) = pressured();
	s.hosts.truncate(1);
	let outcome = decide(&s, &facts(vec![("h1", on_h1)]));
	assert_eq!(on(&outcome, "h1", "be"), 1, "{:?}", outcome.notes);
	assert!(said(
		&outcome,
		"keeps its copy of 1.0 until the copies elsewhere run steady"
	));
}

#[test]
fn without_a_pressure_percent_load_moves_nothing() {
	let (mut s, on_h1) = pressured();
	s.pressure_percent = None;
	let outcome = decide(&s, &facts(vec![("h1", on_h1), ("h2", vec![])]));
	assert!(outcome.notes.is_empty(), "{:?}", outcome.notes);
	assert_eq!(on(&outcome, "h2", "be"), 0);
}

// ---- Priority: who keeps and takes room first; never an eviction ----

fn crowded(priority: u32) -> Spec {
	let low = service("low", "x", 1, 400);
	let mut high = service("high", "x", 1, 400);
	high.priority = priority;
	spec(vec![host("h1", "a", 512)], vec![low, high])
}

#[test]
fn a_higher_priority_never_takes_a_running_copys_room() {
	// Joel, 2026-11-17: nothing is evictable; a free project is lower, never dispensable.
	let outcome = decide(
		&crowded(400),
		&facts(vec![("h1", vec![steady("low", "x", "1.0", None)])]),
	);
	assert_eq!(on(&outcome, "h1", "low"), 1, "{:?}", outcome.notes);
	assert_eq!(on(&outcome, "h1", "high"), 0);
	assert!(said(&outcome, "high/x: wanted 1 copies of 1.0, placed 0"));
	assert!(!outcome.notes.iter().any(|n| n.contains("gives way")));
}

#[test]
fn a_higher_priority_takes_free_room_first_and_room_elsewhere_is_used() {
	// Neither runs and h1 has room for one: the higher priority takes it, whatever the ids say.
	let mut first = crowded(10);
	first.services[0].environment = "a".into();
	first.services[1].environment = "z".into();
	let outcome = decide(&first, &facts(vec![("h1", vec![])]));
	assert_eq!((on(&outcome, "h1", "z"), on(&outcome, "h1", "a")), (1, 0), "{:?}", outcome.notes);

	// With room on another host, both are placed and nothing moves.
	let mut roomy = crowded(10);
	roomy.hosts.push(host("h2", "b", 512));
	let outcome = decide(
		&roomy,
		&facts(vec![
			("h1", vec![steady("low", "x", "1.0", None)]),
			("h2", vec![]),
		]),
	);
	assert_eq!(on(&outcome, "h1", "low"), 1, "{:?}", outcome.notes);
	assert_eq!(on(&outcome, "h2", "high"), 1);
}

#[test]
fn the_higher_priority_keeps_first_when_a_host_shrinks_and_the_other_is_never_stopped() {
	// h1 shrank to 512 MB with both copies running there. The higher priority's is kept in the room;
	// the lower one has none, so it is placed on h2 and held on h1 until that one runs steady.
	let mut s = crowded(10);
	s.hosts.push(host("h2", "b", 512));
	let both = vec![
		steady("low", "x", "1.0", None),
		steady("high", "x", "1.0", None),
	];
	let outcome = decide(&s, &facts(vec![("h1", both.clone()), ("h2", vec![])]));
	assert_eq!(on(&outcome, "h1", "high"), 1, "{:?}", outcome.notes);
	assert_eq!(on(&outcome, "h2", "low"), 1);
	assert_eq!(on(&outcome, "h1", "low"), 1, "held until h2's runs steady");

	// No other host: it stays where it runs, and the shortfall says so.
	let outcome = decide(&crowded(10), &facts(vec![("h1", both)]));
	assert_eq!((on(&outcome, "h1", "high"), on(&outcome, "h1", "low")), (1, 1));
	assert!(said(&outcome, "low/x: wanted 1 copies of 1.0, placed 0"));
}

// ---- The rebalancer ----

/// h1 holds three environments (900 of 1000 MB), h2 one (300), h3 none.
fn lopsided() -> Spec {
	let mut s = spec(
		vec![
			host("h1", "a", 1000),
			host("h2", "b", 1000),
			host("h3", "c", 1000),
		],
		["e1", "e2", "e3", "e4"]
			.iter()
			.map(|e| service(e, "x", 1, 300))
			.collect(),
	);
	s.rebalance = Some(Rebalance {
		high_percent: 80,
		low_percent: 50,
	});
	s
}

fn lopsided_facts() -> Facts {
	facts(vec![
		(
			"h1",
			vec![
				steady("e1", "x", "1.0", None),
				steady("e2", "x", "1.0", None),
				steady("e3", "x", "1.0", None),
			],
		),
		("h2", vec![steady("e4", "x", "1.0", None)]),
		("h3", vec![]),
	])
}

#[test]
fn the_rebalancer_moves_one_environment_off_the_busiest_host_make_before_break() {
	let s = lopsided();
	// 1: nothing moves yet; the move is chosen, to the emptiest host.
	let first = decide(&s, &lopsided_facts());
	assert_eq!(
		first.moves,
		vec![Move {
			environment: "e1".into(),
			from: "h1".into(),
			to: "h3".into()
		}],
		"{:?}",
		first.notes
	);
	assert_eq!(on(&first, "h1", "e1"), 1);
	assert_eq!(on(&first, "h3", "e1"), 0);
	assert!(said(
		&first,
		"rebalance: e1's copies on h1 (90% reserved) move to h3 (0%)"
	));

	// 2: the members carry it on: placed on h3, kept on h1 until h3's runs steady.
	let mut carried = lopsided_facts();
	carried.moves = first.moves.clone();
	let second = decide(&s, &carried);
	assert_eq!(on(&second, "h3", "e1"), 1, "{:?}", second.notes);
	assert_eq!(on(&second, "h1", "e1"), 1);
	assert_eq!(second.moves, first.moves, "still in flight");

	// 3: h3's copy runs steady: h1's goes. h1 still runs it until its agent stops it, so the move stays
	// in flight and h1 stays off e1's hosts: over, the next decision would see e1 on both and keep h1's.
	let mut done = lopsided_facts();
	done.moves = first.moves.clone();
	done.hosts[2].copies.push(steady("e1", "x", "1.0", None));
	let third = decide(&s, &done);
	assert_eq!(on(&third, "h1", "e1"), 0, "{:?}", third.notes);
	assert_eq!(on(&third, "h3", "e1"), 1);
	assert_eq!(third.moves, first.moves, "in flight while h1 still runs it");
	let again = decide(&s, &done);
	assert_eq!((on(&again, "h1", "e1"), on(&again, "h3", "e1")), (0, 1), "never back to h1");

	// What carrying it prevents: the same facts with the move over keep h1's copy and drop h3's.
	let mut over = done.clone();
	over.moves.clear();
	let flipped = decide(&s, &over);
	assert_eq!((on(&flipped, "h1", "e1"), on(&flipped, "h3", "e1")), (1, 0));

	// 4: h1's agent stopped it: the move is done, e1 on h3 alone, and no other starts in the same decision.
	let mut gone = done.clone();
	gone.hosts[0].copies.retain(|c| c.environment != "e1");
	let fourth = decide(&s, &gone);
	assert_eq!(on(&fourth, "h1", "e1"), 0, "{:?}", fourth.notes);
	assert_eq!(on(&fourth, "h3", "e1"), 1);
	assert!(fourth.moves.is_empty());
}

#[test]
fn the_rebalancer_waits_while_anything_is_on_its_way_and_is_off_without_a_band() {
	let s = lopsided();
	let mut busy = lopsided_facts();
	busy.hosts[1].copies[0].state = CopyState::Starting;
	busy.hosts[1].copies[0].running_since_ms = None;
	assert!(decide(&s, &busy).moves.is_empty());

	let mut off = lopsided();
	off.rebalance = None;
	assert!(decide(&off, &lopsided_facts()).moves.is_empty());
	// And a move carried in facts is dropped once the spec stops rebalancing.
	let mut carried = lopsided_facts();
	carried.moves = vec![Move {
		environment: "e1".into(),
		from: "h1".into(),
		to: "h3".into(),
	}];
	let outcome = decide(&off, &carried);
	assert!(outcome.moves.is_empty());
	assert_eq!(on(&outcome, "h3", "e1"), 0);
}

#[test]
fn the_rebalancer_never_crowds_a_fault_domain_nor_lands_on_the_database_host() {
	let mut s = lopsided();
	// e1 spreads over h1 and h2; h3 shares h2's fault domain: moving e1's h1 copy there would leave
	// both of its copies in one domain.
	s.hosts[2].fault_domain = "b".into();
	s.services[0].copies = 2;
	let mut f = lopsided_facts();
	f.hosts[1].copies.push(steady("e1", "x", "1.0", None));
	let outcome = decide(&s, &f);
	assert!(
		outcome.moves.iter().all(|m| m.environment != "e1"),
		"{:?}",
		outcome.moves
	);

	// e2's database lives on h3: its copies never go there; another environment moves.
	let mut s = lopsided();
	s.services
		.retain(|x| x.environment != "e1" && x.environment != "e3");
	s.services.push(service("e1", "x", 1, 300));
	let mut f = lopsided_facts();
	f.database_hosts.insert("e1".into(), "h3".into());
	f.database_hosts.insert("e2".into(), "h3".into());
	f.database_hosts.insert("e3".into(), "h3".into());
	assert!(decide(&s, &f).moves.is_empty());
}

#[test]
fn a_move_whose_destination_is_lost_is_given_up() {
	let s = lopsided();
	let mut f = lopsided_facts();
	f.moves = vec![Move {
		environment: "e1".into(),
		from: "h1".into(),
		to: "h3".into(),
	}];
	f.hosts[2].last_heartbeat_ms = 0;
	let outcome = decide(&s, &f);
	assert!(
		said(&outcome, "no longer possible; given up"),
		"{:?}",
		outcome.notes
	);
	assert_eq!(on(&outcome, "h1", "e1"), 1);
}

// ---- Rollouts ----

fn rolling(copies: u32, hosts: usize, rollout: Option<Rollout>) -> Spec {
	let mut x = service("cp", "x", copies, 100);
	x.versions = vec![VersionShare {
		version: "2.0".into(),
		share: 1,
	}];
	x.rollout = rollout;
	spec(
		(1..=hosts)
			.map(|i| host(&format!("h{i}"), &format!("d{i}"), 1000))
			.collect(),
		vec![x],
	)
}

fn old_on(hosts: &[&str], all: usize) -> Facts {
	let mut f = facts(vec![]);
	for i in 1..=all {
		let name = format!("h{i}");
		let copies = if hosts.contains(&name.as_str()) {
			vec![steady("cp", "x", "1.0", None)]
		} else {
			vec![]
		};
		f.hosts.push(HostFacts {
			name,
			last_heartbeat_ms: NOW - 1000,
			copies,
			report_only: false,
		});
	}
	f
}

#[test]
fn without_a_rollout_a_version_change_takes_one_host_at_a_time() {
	let outcome = decide(&rolling(3, 3, None), &old_on(&["h1", "h2", "h3"], 3));
	let new: u32 = ["h1", "h2", "h3"]
		.iter()
		.map(|h| version_on(&outcome, h, "2.0"))
		.sum();
	assert_eq!(new, 1, "{:?}", outcome.notes);
}

#[test]
fn max_unavailable_lets_that_many_hosts_change_at_once() {
	let rollout = Rollout {
		max_surge: 0,
		max_unavailable: 2,
		rollback: false,
	};
	let outcome = decide(
		&rolling(3, 3, Some(rollout)),
		&old_on(&["h1", "h2", "h3"], 3),
	);
	assert_eq!(version_on(&outcome, "h1", "2.0"), 1, "{:?}", outcome.notes);
	assert_eq!(version_on(&outcome, "h2", "2.0"), 1);
	assert_eq!(version_on(&outcome, "h3", "1.0"), 1);
}

#[test]
fn a_surge_places_the_new_version_beside_the_old_on_a_free_host() {
	let rollout = Rollout {
		max_surge: 1,
		max_unavailable: 1,
		rollback: false,
	};
	let s = rolling(3, 4, Some(rollout));
	let outcome = decide(&s, &old_on(&["h1", "h2", "h3"], 4));
	assert_eq!(version_on(&outcome, "h1", "2.0"), 1, "{:?}", outcome.notes);
	assert_eq!(version_on(&outcome, "h2", "1.0"), 1);
	assert_eq!(version_on(&outcome, "h3", "1.0"), 1);
	assert_eq!(version_on(&outcome, "h4", "2.0"), 1, "the surge");
	assert!(said(&outcome, "beside the old ones (surge)"));
	let total: u32 = outcome.placement.iter().map(|a| a.count).sum();
	assert_eq!(total, 4, "copies plus max_surge, never more");
}

#[test]
fn with_a_rollout_a_surplus_old_copy_waits_for_the_new_ones() {
	let rollout = Rollout {
		max_surge: 1,
		max_unavailable: 1,
		rollback: false,
	};
	let s = rolling(3, 4, Some(rollout));
	// New on h1, h2 (starting) and h4; old still on h3: h3 is surplus, and h2 is on its way up.
	let mut f = old_on(&["h3"], 4);
	f.hosts[0].copies.push(steady("cp", "x", "2.0", None));
	f.hosts[1].copies.push(with_state(
		steady("cp", "x", "2.0", None),
		CopyState::Starting,
	));
	f.hosts[3].copies.push(steady("cp", "x", "2.0", None));
	let outcome = decide(&s, &f);
	assert_eq!(version_on(&outcome, "h3", "1.0"), 1, "{:?}", outcome.notes);
	// h2 runs: h3's old copy goes.
	f.hosts[1].copies[0] = steady("cp", "x", "2.0", None);
	let outcome = decide(&s, &f);
	assert_eq!(version_on(&outcome, "h3", "1.0"), 0, "{:?}", outcome.notes);
}

fn failing() -> (Spec, Facts) {
	let rollout = Rollout {
		max_surge: 0,
		max_unavailable: 1,
		rollback: true,
	};
	let s = rolling(3, 4, Some(rollout));
	let mut f = old_on(&["h2", "h3"], 4);
	f.hosts[0].copies.push(with_state(
		steady("cp", "x", "2.0", None),
		CopyState::GaveUp,
	));
	(s, f)
}

#[test]
fn a_version_an_agent_gives_up_on_rolls_back_and_stays_back() {
	let (s, f) = failing();
	let outcome = decide(&s, &f);
	assert_eq!(
		outcome.rollbacks.get("cp/x"),
		Some(&Rollback {
			from: "2.0".into(),
			to: "1.0".into()
		}),
		"{:?}",
		outcome.notes
	);
	assert!(said(
		&outcome,
		"rolled back to 1.0 until the spec changes its version"
	));
	for h in ["h1", "h2", "h3", "h4"] {
		assert_eq!(version_on(&outcome, h, "2.0"), 0, "{h}");
	}
	assert_eq!(version_on(&outcome, "h2", "1.0"), 1);
	assert_eq!(version_on(&outcome, "h3", "1.0"), 1);
	assert_eq!(
		version_on(&outcome, "h4", "1.0"),
		1,
		"the third copy, off the host that gave up"
	);
	assert!(
		outcome.table["cp/x"].iter().all(|r| r.version == "1.0"),
		"{:?}",
		outcome.table
	);

	// The gave-up report is gone (the agent was told to stop it); the members carry the rollback, so
	// it does not roll forward again.
	let mut later = old_on(&["h2", "h3", "h4"], 4);
	later.rollbacks = outcome.rollbacks.clone();
	let again = decide(&s, &later);
	assert_eq!(again.rollbacks, outcome.rollbacks);
	let new: u32 = ["h1", "h2", "h3", "h4"]
		.iter()
		.map(|h| version_on(&again, h, "2.0"))
		.sum();
	assert_eq!(new, 0, "{:?}", again.notes);

	// A spec that changes the version ends it.
	let mut fixed = s.clone();
	fixed.services[0].versions[0].version = "2.1".into();
	let rolled = decide(&fixed, &later);
	assert!(rolled.rollbacks.is_empty());
	let new: u32 = ["h1", "h2", "h3", "h4"]
		.iter()
		.map(|h| version_on(&rolled, h, "2.1"))
		.sum();
	assert_eq!(
		new, 1,
		"rolling again, one host at a time: {:?}",
		rolled.notes
	);
}

#[test]
fn no_rollback_without_the_flag_nor_forward_to_a_canary() {
	let (mut s, f) = failing();
	s.services[0].rollout = Some(Rollout {
		max_surge: 0,
		max_unavailable: 1,
		rollback: false,
	});
	assert!(decide(&s, &f).rollbacks.is_empty());

	// Stable 1.0 at 9, canary 2.0 at 1: a stable copy giving up is not a reason to go to the canary.
	let (mut s, _) = failing();
	s.services[0].versions = vec![
		VersionShare {
			version: "1.0".into(),
			share: 9,
		},
		VersionShare {
			version: "2.0".into(),
			share: 1,
		},
	];
	let mut f = old_on(&["h2"], 4);
	f.hosts[0].copies.push(with_state(
		steady("cp", "x", "1.0", None),
		CopyState::GaveUp,
	));
	f.hosts[2].copies.push(steady("cp", "x", "2.0", None));
	assert!(decide(&s, &f).rollbacks.is_empty());
	// The canary giving up does roll back to the stable version.
	let mut f = old_on(&["h2", "h3"], 4);
	f.hosts[0].copies.push(with_state(
		steady("cp", "x", "2.0", None),
		CopyState::GaveUp,
	));
	let outcome = decide(&s, &f);
	assert_eq!(
		outcome.rollbacks.get("cp/x").map(|r| r.to.as_str()),
		Some("1.0")
	);
}

#[test]
fn a_logged_decision_with_moves_and_rollbacks_replays_the_same() {
	let s = lopsided();
	let mut f = lopsided_facts();
	f.moves = decide(&s, &f).moves;
	let once = decide(&s, &f);
	let read: Facts =
		serde_json::from_value(serde_json::to_value(&f).expect("json")).expect("facts");
	assert_eq!(decide(&s, &read), once);
}
