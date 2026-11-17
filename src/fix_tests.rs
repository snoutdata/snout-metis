//! The bugs the refactor's review and its invariant found (2026-11-17), one case each. The generated
//! fleets of `differential_tests.rs` check the same over a million decisions (`within_share`); these
//! say each one in a few lines, so it stays fixed after the old core is gone.

use crate::decide::decide;
use crate::orchestration_tests::{facts, host, mb, on, service, spec, steady};
use crate::spec::{PlaceMode, Policy};

fn said(outcome: &crate::decide::Outcome, part: &str) -> bool {
	outcome.notes.iter().any(|n| n.contains(part))
}

/// A: a finishing host's copy, kept since nothing replaces it yet, is not held a second time for its
/// drain: one copy there, never two of a service a host holds one of.
#[test]
fn a_finishing_hosts_kept_copy_is_held_once() {
	let mut finishing = host("h2", "b", 1000);
	finishing.serves = false;
	finishing.frozen = true;
	finishing.finishing = true;
	let s = spec(
		vec![host("h1", "a", 1000), finishing, host("h3", "c", 1000)],
		vec![service("e", "auth", 2, 100)],
	);
	let outcome = decide(
		&s,
		&facts(vec![
			("h1", vec![]),
			("h2", vec![steady("e", "auth", "1.0", None)]),
			("h3", vec![]),
		]),
	);
	assert_eq!(on(&outcome, "h2", "e"), 1, "{:?}", outcome.notes);
}

/// H: a draining host that still serves keeps what runs there and takes nothing new.
#[test]
fn a_draining_host_takes_no_new_copy() {
	let mut draining = host("h1", "a", 1000);
	draining.draining = true;
	let s = spec(
		vec![draining, host("h2", "b", 1000)],
		vec![service("e", "auth", 1, 100)],
	);
	let outcome = decide(&s, &facts(vec![("h1", vec![]), ("h2", vec![])]));
	assert_eq!(
		(on(&outcome, "h1", "e"), on(&outcome, "h2", "e")),
		(0, 1),
		"{:?}",
		outcome.notes
	);
}

fn packing(copies: u32, max_per_host: u32) -> crate::spec::ServiceSpec {
	let mut x = service("p", "x", copies, 100);
	x.policy = Policy {
		mode: PlaceMode::Pack,
		max_per_host,
	};
	x
}

/// E and G: a copy moving home is no copy of its host's to give away. With a home that holds one of
/// two copies, the copy running off the home stays where it is and the home takes the other: the old
/// core sent the running copy home and then added the second copy beside it, two on one host.
#[test]
fn a_copy_moving_home_never_gets_a_new_copy_beside_it() {
	let s = spec(
		vec![
			host("home", "a", 1000),
			host("a", "b", 1000),
			host("b", "c", 1000),
		],
		vec![packing(2, 1)],
	);
	let mut f = facts(vec![
		("home", vec![]),
		("a", vec![steady("p", "x", "1.0", None)]),
		("b", vec![]),
	]);
	f.database_hosts.insert("p".into(), "home".into());
	let outcome = decide(&s, &f);
	for h in ["home", "a", "b"] {
		assert!(on(&outcome, h, "p") <= 1, "{h}: {:?}", outcome.notes);
	}
	assert_eq!(
		(on(&outcome, "home", "p"), on(&outcome, "a", "p")),
		(1, 1),
		"{:?}",
		outcome.notes
	);
}

/// B: a home under pressure gets nothing, so nothing is said to have moved there and the copies stay
/// where they run.
#[test]
fn a_home_under_pressure_is_not_moved_to() {
	let mut s = spec(
		vec![
			host("home", "a", 1000),
			host("a", "b", 1000),
			host("b", "c", 1000),
			host("c", "a", 1000),
		],
		vec![packing(2, 2)],
	);
	let mut hog = service("hog", "y", 1, 100);
	hog.limit = Some(mb(100));
	hog.placement.hosts = vec!["home".into()];
	s.services.push(hog);
	s.pressure_percent = Some(50);
	let mut f = facts(vec![
		("home", vec![steady("hog", "y", "1.0", Some(900))]),
		("a", vec![steady("p", "x", "1.0", None)]),
		("b", vec![steady("p", "x", "1.0", None)]),
		("c", vec![]),
	]);
	f.database_hosts.insert("p".into(), "home".into());
	let outcome = decide(&s, &f);
	assert!(!said(&outcome, "moved to its home"), "{:?}", outcome.notes);
	assert_eq!(
		(
			on(&outcome, "a", "p"),
			on(&outcome, "b", "p"),
			on(&outcome, "home", "p")
		),
		(1, 1, 0),
		"{:?}",
		outcome.notes
	);
}
