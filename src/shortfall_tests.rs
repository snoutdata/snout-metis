//! Why a decision placed a service short (`Outcome::shortfalls`): every host of the spec and the
//! first reason it took no more copies, the same tests in the same order as the decision's own.
//! What the admin console's hosts, the shortfall alert (sql/395) and the dashboard read.

use crate::decide::{Refused, decide};
use crate::orchestration_tests::{NOW, facts, host, mb, said, service, spec, steady};
use crate::spec::{CopyState, Resources};

fn reasons(outcome: &crate::decide::Outcome) -> Vec<(String, Refused)> {
	outcome.shortfalls[0]
		.hosts
		.iter()
		.map(|r| (r.host.clone(), r.reason))
		.collect()
}

#[test]
fn a_full_shared_host_and_a_customers_own_host_are_told_apart() {
	// The 2026-11 alert: a shared service with no room, while a dedicated server had plenty.
	let mut own = host("h2", "b", 4096);
	own.dedicated_to = Some("solarpanda".into());
	let s = spec(
		vec![host("h1", "a", 256), own, host("h3", "c", 4096)],
		vec![{
			let mut gw = service("cp", "gateway", 2, 512);
			gw.placement.hosts = vec!["h1".into(), "h2".into()];
			gw
		}],
	);
	let out = decide(&s, &facts(vec![("h1", vec![]), ("h2", vec![]), ("h3", vec![])]));
	assert_eq!(out.shortfalls.len(), 1);
	let short = &out.shortfalls[0];
	assert_eq!((short.service.as_str(), short.wanted, short.placed), ("cp/gateway", 2, 0));
	assert_eq!(short.needs, mb(512));
	assert_eq!(
		reasons(&out),
		vec![
			("h1".into(), Refused::Full),
			("h2".into(), Refused::Dedicated),
			("h3".into(), Refused::NotOnItsList),
		]
	);
	assert_eq!(short.hosts[1].dedicated_to.as_deref(), Some("solarpanda"));
	assert_eq!(short.hosts[0].free, mb(256));
	assert_eq!(short.hosts[1].capacity, mb(4096));
	assert!(
		said(
			&out,
			"cp/gateway: wanted 2 copies of 1.0, placed 0: no eligible host has room (1 dedicated to another customer: h2 (solarpanda); 1 not on its list of hosts: h3; 1 full: h1 256 of 256 MB free)"
		),
		"{:?}",
		out.notes
	);
}

#[test]
fn a_dedicated_service_names_the_shared_hosts_it_never_uses() {
	let mut own = host("h2", "b", 256);
	own.dedicated_to = Some("acme".into());
	let mut svc = service("p", "api", 2, 256);
	svc.dedicated_to = Some("acme".into());
	let out = decide(
		&spec(vec![host("h1", "a", 4096), own], vec![svc]),
		&facts(vec![("h1", vec![]), ("h2", vec![])]),
	);
	assert_eq!(
		reasons(&out),
		vec![("h1".into(), Refused::Shared), ("h2".into(), Refused::HoldsItsShare)]
	);
}

#[test]
fn silent_draining_and_given_up_hosts_say_so() {
	let mut draining = host("h2", "b", 4096);
	draining.draining = true;
	draining.serves = false;
	let mut witness = host("h4", "d", 0);
	witness.serves = false;
	let out = decide(
		&spec(
			vec![host("h1", "a", 4096), draining, host("h3", "c", 4096), witness, host("h5", "e", 4096)],
			vec![service("p", "api", 3, 256)],
		),
		&facts(vec![
			("h2", vec![]),
			(
				"h3",
				vec![{
					let mut c = steady("p", "api", "1.0", None);
					c.state = CopyState::GaveUp;
					c.running_since_ms = None;
					c
				}],
			),
			("h4", vec![]),
			("h5", vec![]),
		]),
	);
	assert_eq!(out.shortfalls[0].placed, 1, "h5 alone takes one");
	assert_eq!(
		reasons(&out),
		vec![
			("h1".into(), Refused::NotReporting),
			("h2".into(), Refused::Draining),
			("h3".into(), Refused::GaveUp),
			("h4".into(), Refused::NotServing),
			("h5".into(), Refused::HoldsItsShare),
		]
	);
	assert!(said(&out, "placed 1: no eligible host has room (1 not reporting: h1; 1 draining: h2; 1 serving nothing: h4; 1 where its agent gave up on it: h3; 1 already holding its copy: h5)"), "{:?}", out.notes);
}

#[test]
fn a_host_short_of_cpu_says_cpu() {
	let mut s = service("p", "api", 2, 0);
	s.per_copy = Resources {
		millicpu: 500,
		memory_mb: 64,
	};
	let mut h1 = host("h1", "a", 4096);
	h1.capacity.millicpu = 600;
	let mut h2 = host("h2", "b", 4096);
	h2.capacity.millicpu = 200;
	let out = decide(&spec(vec![h1, h2], vec![s]), &facts(vec![("h1", vec![]), ("h2", vec![])]));
	assert!(said(&out, "1 full: h2 200 of 200 millicpu free"), "{:?}", out.notes);
	assert_eq!(reasons(&out), vec![("h1".into(), Refused::HoldsItsShare), ("h2".into(), Refused::Full)]);
}

#[test]
fn five_hosts_are_named_then_a_count() {
	let hosts: Vec<_> = (1..=8).map(|i| host(&format!("h{i}"), "a", 100)).collect();
	let reported: Vec<_> = (1..=8).map(|i| (format!("h{i}"), vec![])).collect();
	let out = decide(
		&spec(hosts, vec![service("p", "api", 1, 256)]),
		&facts(reported.iter().map(|(h, c)| (h.as_str(), c.clone())).collect()),
	);
	assert!(said(&out, "(8 full: h1 100 of 100 MB free, h2 100 of 100 MB free, h3 100 of 100 MB free, h4 100 of 100 MB free, h5 100 of 100 MB free, 3 more)"), "{:?}", out.notes);
	assert_eq!(out.shortfalls[0].hosts.len(), 8, "the field keeps every host");
}

#[test]
fn nothing_short_writes_no_field_and_the_same_input_says_the_same() {
	let s = spec(vec![host("h1", "a", 4096)], vec![service("p", "api", 1, 256)]);
	let f = facts(vec![("h1", vec![])]);
	let out = decide(&s, &f);
	assert!(out.shortfalls.is_empty());
	let json = serde_json::to_value(&out).unwrap();
	assert!(json.get("shortfalls").is_none(), "{json}");

	let short = spec(vec![host("h1", "a", 100)], vec![service("p", "api", 1, 256)]);
	let a = decide(&short, &f);
	let b = decide(&short, &f);
	assert_eq!(a.shortfalls, b.shortfalls);
	let json = serde_json::to_value(&a).unwrap();
	assert_eq!(json["shortfalls"][0]["hosts"][0]["reason"], "full", "{json}");
	let back: crate::decide::Outcome = serde_json::from_value(json).unwrap();
	assert_eq!(back.shortfalls, a.shortfalls);
	let _ = NOW;
}

#[test]
fn a_pool_server_says_its_size_and_whose_it_is() {
	// The fleet's pools (sql/337): a pool project's copies are kept on its servers by a list, and a
	// server's capacity is what its databases leave. The 2026-11 alert, a dedicated project's gateway:
	// both its servers had a few hundred MB left of 8 GB, and the note said only "no room".
	let pool = |name: &str, domain: &str| {
		let mut h = host(name, domain, 300);
		h.server = Some(mb(8192));
		h.owner = Some("solarpanda".into());
		h
	};
	let mut gw = service("a19x3mjw9p0j1", "psyche", 2, 512);
	gw.placement.hosts = vec!["sd-a".into(), "sd-b".into()];
	let mut shared = service("other", "psyche", 1, 512);
	shared.placement.hosts = vec!["sd-c".into()];
	let out = decide(
		&spec(vec![pool("sd-a", "a"), pool("sd-b", "b"), host("sd-c", "c", 4096)], vec![gw, shared]),
		&facts(vec![("sd-a", vec![]), ("sd-b", vec![]), ("sd-c", vec![])]),
	);
	assert_eq!(out.shortfalls.len(), 1, "the shared project is placed");
	assert!(
		said(
			&out,
			"a19x3mjw9p0j1/psyche: wanted 2 copies of 1.0, placed 0: no eligible host has room (1 not on its list of hosts: sd-c; 2 full: sd-a 300 of 300 MB free, of a 8192 MB server, sd-b 300 of 300 MB free, of a 8192 MB server)"
		),
		"{:?}",
		out.notes
	);
	let short = &out.shortfalls[0];
	assert_eq!(short.hosts[0].server, Some(mb(8192)));
	assert_eq!(short.hosts[0].dedicated_to.as_deref(), Some("solarpanda"));
	assert_eq!(short.hosts[2].dedicated_to, None);
}
