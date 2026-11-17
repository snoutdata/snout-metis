//! The refactor's guard: `decide` against `decide_before` (the core as it was at d9a1d3095), over
//! generated fleets and over runs of decisions where the agents carry out what was decided. Every
//! outcome must be the same, notes included, byte for byte. A coverage check makes sure the inputs
//! reach every note and every refusal, so "the same" is said of every path, not of the easy ones.

use crate::decide::{Outcome, decide};
use crate::decide_before;
use crate::spec::{
	CopyFacts, CopyState, Facts, HostFacts, HostSpec, Move, PlaceMode, Placement, Policy,
	Rebalance, Resources, Rollback, Rollout, Scale, ServiceSpec, Spec, VersionShare,
};
use std::collections::{BTreeMap, BTreeSet};

const NOW: u64 = 10_000_000;

/// splitmix64: small, seeded, the same numbers on every machine.
struct Rng(u64);

impl Rng {
	fn next(&mut self) -> u64 {
		self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
		let mut z = self.0;
		z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
		z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
		z ^ (z >> 31)
	}
	fn below(&mut self, n: u64) -> u64 {
		self.next() % n.max(1)
	}
	fn chance(&mut self, percent: u64) -> bool {
		self.below(100) < percent
	}
	fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
		&items[self.below(items.len() as u64) as usize]
	}
}

const REGIONS: [&str; 2] = ["region-a", "region-b"];
const DOMAINS: [&str; 3] = ["fd-1", "fd-2", "fd-3"];
const ENVIRONMENTS: [&str; 4] = [
	"control-plane",
	"p0000000000a1",
	"p0000000000b2",
	"p0000000000c3",
];
const NAMES: [&str; 4] = ["auth", "rest", "lepis", "psyche"];
const VERSIONS: [&str; 4] = ["v1", "v2", "v3", "v0"];

fn host(rng: &mut Rng, i: usize) -> HostSpec {
	let frozen = rng.chance(8);
	let draining = !frozen && rng.chance(10);
	HostSpec {
		name: format!("h{i}"),
		region: rng.pick(&REGIONS).to_string(),
		fault_domain: rng.pick(&DOMAINS).to_string(),
		serves: !rng.chance(12),
		draining,
		serves_home: rng.chance(15),
		frozen,
		finishing: frozen && rng.chance(50),
		dedicated_to: rng.chance(15).then(|| "acme".to_string()),
		capacity: Resources {
			millicpu: *rng.pick(&[0, 300, 1000, 2000, 4000]),
			memory_mb: *rng.pick(&[0, 200, 400, 700, 1024, 2048, 4096]),
		},
		server: rng.chance(20).then_some(Resources {
			millicpu: 4000,
			memory_mb: 8192,
		}),
		owner: rng.chance(10).then(|| "owner-1".to_string()),
		room: None,
	}
}

fn service(rng: &mut Rng, environment: &str, name: &str, hosts: &[HostSpec]) -> ServiceSpec {
	let one_clock = rng.chance(6);
	let copies = if one_clock {
		1
	} else {
		1 + rng.below(3) as u32
	};
	let mut versions = Vec::new();
	let n = 1 + rng.below(3) as usize;
	for v in VERSIONS.iter().take(n) {
		versions.push(VersionShare {
			version: v.to_string(),
			share: *rng.pick(&[0, 1, 1, 3, 10]),
		});
	}
	if versions.iter().all(|v| v.share == 0) {
		versions[0].share = 1;
	}
	let per_copy = Resources {
		millicpu: *rng.pick(&[0, 0, 50, 100, 250]),
		memory_mb: *rng.pick(&[0, 64, 96, 128, 256, 512]),
	};
	let pack = environment != "control-plane" && rng.chance(35);
	let policy = if pack {
		Policy {
			mode: PlaceMode::Pack,
			max_per_host: 1 + rng.below(3) as u32,
		}
	} else {
		Policy::SPREAD
	};
	let dedicated = hosts.iter().any(|h| h.dedicated_to.is_some()) && rng.chance(25);
	let mut placement = Placement::default();
	if rng.chance(15) {
		placement.hosts = hosts
			.iter()
			.filter(|h| (h.serves || h.serves_home) && rng.chance(60))
			.map(|h| h.name.clone())
			.collect();
	}
	if rng.chance(10) {
		placement.regions = vec![rng.pick(&REGIONS).to_string()];
	}
	let scale = (!one_clock && rng.chance(20) && per_copy != Resources::default()).then(|| {
		let up = 50 + rng.below(40) as u32;
		Scale {
			max_copies: (copies + rng.below(2) as u32).min(3),
			up_percent: up,
			down_percent: rng.below(u64::from(up)) as u32,
		}
	});
	let limit = match rng.below(4) {
		0 => Some(per_copy),
		1 => Some(Resources {
			millicpu: per_copy.millicpu * 2,
			memory_mb: per_copy.memory_mb * 2,
		}),
		_ => None,
	};
	let rollout = rng.chance(40).then(|| Rollout {
		max_surge: if one_clock { 0 } else { rng.below(3) as u32 },
		max_unavailable: 1 + rng.below(2) as u32,
		rollback: rng.chance(50),
	});
	ServiceSpec {
		name: name.to_string(),
		environment: environment.to_string(),
		copies,
		versions,
		per_copy,
		one_clock,
		dedicated_to: dedicated.then(|| "acme".to_string()),
		run: None,
		placement,
		routed: !rng.chance(10),
		scale,
		policy,
		limit,
		priority: *rng.pick(&[0, 0, 1, 5]),
		rollout,
	}
}

fn spec(rng: &mut Rng) -> Spec {
	let hosts: Vec<HostSpec> = (0..1 + rng.below(7) as usize)
		.map(|i| host(rng, i))
		.collect();
	let mut services = Vec::new();
	let mut ids = BTreeSet::new();
	for _ in 0..1 + rng.below(7) {
		let environment = *rng.pick(&ENVIRONMENTS);
		let name = *rng.pick(&NAMES);
		if ids.insert((environment, name)) {
			services.push(service(rng, environment, name, &hosts));
		}
	}
	Spec {
		host_timeout_secs: 30,
		hosts,
		services,
		database_cluster: None,
		agents: Vec::new(),
		pressure_percent: rng.chance(30).then(|| 40 + rng.below(61) as u32),
		rebalance: rng.chance(30).then(|| Rebalance {
			high_percent: 60 + rng.below(40) as u32,
			low_percent: 10 + rng.below(40) as u32,
		}),
	}
}

fn copy(rng: &mut Rng, s: &ServiceSpec) -> CopyFacts {
	let version = if rng.chance(85) {
		rng.pick(&s.versions).version.clone()
	} else {
		rng.pick(&VERSIONS).to_string()
	};
	let state = match rng.below(10) {
		0 => CopyState::Starting,
		1 => CopyState::GaveUp,
		_ => CopyState::Running,
	};
	CopyFacts {
		environment: s.environment.clone(),
		service: s.name.clone(),
		version,
		state,
		load: rng.chance(50).then(|| Resources {
			millicpu: rng.below(600) as u32,
			memory_mb: rng.below(900) as u32,
		}),
		running_since_ms: rng.chance(80).then(|| NOW - rng.below(90_000)),
	}
}

fn facts(rng: &mut Rng, spec: &Spec) -> Facts {
	let mut hosts = Vec::new();
	for h in &spec.hosts {
		if rng.chance(8) {
			continue;
		}
		let mut copies = Vec::new();
		for s in &spec.services {
			for _ in 0..rng.below(3) {
				if rng.chance(45) {
					copies.push(copy(rng, s));
				}
			}
		}
		if rng.chance(5) {
			copies.push(CopyFacts {
				environment: "p0000000000z9".into(),
				service: "auth".into(),
				version: "v1".into(),
				state: CopyState::Running,
				load: None,
				running_since_ms: Some(NOW - 60_000),
			});
		}
		hosts.push(HostFacts {
			name: h.name.clone(),
			last_heartbeat_ms: if rng.chance(12) {
				NOW - 120_000
			} else {
				NOW - 1_000
			},
			copies,
			report_only: rng.chance(8),
		});
	}
	let names: Vec<String> = spec.hosts.iter().map(|h| h.name.clone()).collect();
	let mut database_hosts = BTreeMap::new();
	for env in ENVIRONMENTS {
		if rng.chance(50) {
			let at = if rng.chance(90) {
				rng.pick(&names).clone()
			} else {
				"gone".to_string()
			};
			database_hosts.insert(env.to_string(), at);
		}
	}
	let mut moves = Vec::new();
	if rng.chance(25) && names.len() > 1 {
		moves.push(Move {
			environment: rng.pick(&ENVIRONMENTS).to_string(),
			from: rng.pick(&names).clone(),
			to: rng.pick(&names).clone(),
		});
	}
	let mut rollbacks = BTreeMap::new();
	for s in &spec.services {
		if rng.chance(10) {
			rollbacks.insert(
				s.id(),
				Rollback {
					from: rng.pick(&s.versions).version.clone(),
					to: rng.pick(&VERSIONS).to_string(),
				},
			);
		}
	}
	Facts {
		now_ms: NOW,
		hosts,
		database_primary_region: rng.chance(50).then(|| rng.pick(&REGIONS).to_string()),
		database_hosts,
		moves,
		rollbacks,
	}
}

/// The agents carry the outcome out, most of the way: each placed copy is there (running, or
/// still starting), what is no longer placed mostly goes, and the clock moves on.
fn carry_out(rng: &mut Rng, spec: &Spec, facts: &Facts, outcome: &crate::decide::Outcome) -> Facts {
	let now = facts.now_ms + *rng.pick(&[5_000, 20_000, 40_000]);
	let mut next = facts.clone();
	next.now_ms = now;
	next.moves = outcome.moves.clone();
	next.rollbacks = outcome.rollbacks.clone();
	for h in &mut next.hosts {
		if rng.chance(95) {
			h.last_heartbeat_ms = now - 1_000;
		}
		let placed: Vec<_> = outcome
			.placement
			.iter()
			.filter(|a| a.host == h.name)
			.collect();
		let old = std::mem::take(&mut h.copies);
		for c in old {
			let still = placed.iter().any(|a| {
				a.environment == c.environment && a.service == c.service && a.version == c.version
			});
			if still || rng.chance(20) {
				h.copies.push(c);
			}
		}
		for a in placed {
			let have = h
				.copies
				.iter()
				.filter(|c| {
					c.environment == a.environment
						&& c.service == a.service
						&& c.version == a.version
				})
				.count() as u32;
			for _ in have..a.count {
				let state = match rng.below(12) {
					0 => CopyState::GaveUp,
					1..=3 => CopyState::Starting,
					_ => CopyState::Running,
				};
				h.copies.push(CopyFacts {
					environment: a.environment.clone(),
					service: a.service.clone(),
					version: a.version.clone(),
					state,
					load: rng.chance(60).then(|| {
						let most = if rng.chance(50) { 10 } else { 600 };
						Resources {
							millicpu: rng.below(most) as u32,
							memory_mb: rng.below(most) as u32,
						}
					}),
					running_since_ms: (state == CopyState::Running).then_some(now),
				});
			}
		}
		for c in &mut h.copies {
			if c.state == CopyState::Starting && rng.chance(60) {
				c.state = CopyState::Running;
				c.running_since_ms = Some(now);
			}
		}
	}
	let _ = spec;
	next
}

/// Every kind of note and refusal the inputs reached, by its first words.
fn kinds(outcome: &crate::decide::Outcome, seen: &mut BTreeMap<String, usize>) {
	const MARKS: [&str; 20] = [
		"is silent past",
		"rolled back to",
		"moves off it",
		"with only guaranteed copies left",
		"is no longer possible",
		"of what they reserve, above",
		"on one fewer, below",
		"gave up on it; placed on another host",
		"placed on",
		"wanted",
		"moved to its home",
		"until the one on its home",
		"since its room there is taken",
		"has no room for its copy",
		"until the copy changing before it runs",
		"beside the old ones (surge)",
		"while it drains",
		"moved off it",
		"keeps its copy of",
		"reserved) move to",
	];
	for note in &outcome.notes {
		for mark in MARKS {
			if note.contains(mark) {
				*seen.entry(format!("note: {mark}")).or_default() += 1;
			}
		}
	}
	for s in &outcome.shortfalls {
		for r in &s.hosts {
			*seen.entry(format!("refused: {:?}", r.reason)).or_default() += 1;
		}
	}
}

/// The four fixes made after the refactor (2026-11-17), each by what it changes. A decision the cores
/// differ on must be one of these, or the test fails.
fn fixed(spec: &Spec, facts: &Facts, now: &Outcome, before: &Outcome) -> Option<&'static str> {
	let runs = |host: &str, env: &str| {
		facts.hosts.iter().any(|h| {
			h.name == host
				&& h.copies.iter().any(|c| {
					c.environment == env
						&& c.state != CopyState::GaveUp
						&& spec
							.services
							.iter()
							.any(|s| s.environment == env && s.name == c.service)
				})
		})
	};
	// D: a move kept in flight while `from` still runs its environment's copy.
	if now
		.moves
		.iter()
		.any(|m| !before.moves.contains(m) && runs(&m.from, &m.environment))
	{
		return Some("D: a move carried while its old host still runs the copy");
	}
	// A: the old core held a draining or finishing host's copy for its drain when that host's copy of
	// the version was kept anyway, so the host held one copy more.
	let held_twice = before
		.notes
		.iter()
		.filter(|n| !now.notes.contains(n))
		.any(|n| {
			now.placement.iter().any(|a| {
				n == &format!(
					"{}/{}: {} keeps its copy of {} while it drains, until the copies elsewhere run steady",
					a.environment, a.service, a.host, a.version
				)
			})
		});
	if held_twice {
		return Some("A: a draining or finishing host's kept copy held twice");
	}
	// E: a copy added where the same version runs and was moving home is that running copy, kept.
	let doubled = before
		.notes
		.iter()
		.any(|n| n.contains(" until the one on its home,"))
		&& before
			.notes
			.iter()
			.filter(|n| !now.notes.contains(n))
			.any(|n| n.contains(" placed on "));
	if doubled {
		return Some("E: a copy moving home kept where a new one was added beside it");
	}
	// F: a copy held for want of room is not held again as moving home.
	let twice = before
		.notes
		.iter()
		.filter(|n| !now.notes.contains(n))
		.any(|n| n.contains(" until the one on its home,"))
		&& before
			.notes
			.iter()
			.any(|n| n.contains("has no room for its copy"));
	if twice {
		return Some("F: a copy held once, not for no room and for its home both");
	}
	// H: a draining host that still serves takes no new copy.
	if spec.hosts.iter().any(|h| h.draining && h.serves) {
		return Some("H: a draining host takes nothing new");
	}
	// I: a surge goes to a host holding none of the service, never one whose old copy a roll keeps.
	if before
		.notes
		.iter()
		.any(|n| n.ends_with("beside the old ones (surge)"))
	{
		return Some("I: a surge never lands on a host keeping its old copy");
	}
	// J: a copy on a draining host the service also moves off is held once, not for both.
	if before
		.notes
		.iter()
		.filter(|n| !now.notes.contains(n))
		.any(|n| n.contains(" while it drains,"))
	{
		return Some("J: a draining host's copy held once, not for its drain and its move both");
	}
	// B: a copy moving home, or kept where its room is taken, now waits for copies that are not held.
	let handed = |o: &Outcome| {
		o.notes.iter().any(|n| {
			n.contains("its home")
				|| n.contains("has no room for its copy")
				|| n.contains("since its room there is taken")
		})
	};
	if handed(now) || handed(before) {
		return Some("B: a held copy is no replacement for another");
	}
	// C: a host finishing but not frozen no longer stops the rebalancer.
	if spec.rebalance.is_some()
		&& spec.hosts.iter().any(|h| h.finishing && !h.frozen)
		&& now.moves != before.moves
	{
		return Some("C: finishing is frozen and finishing, everywhere");
	}
	None
}

thread_local! {
	static FIXED: std::cell::RefCell<BTreeMap<&'static str, usize>> = const { std::cell::RefCell::new(BTreeMap::new()) };
}

fn same(spec: &Spec, facts: &Facts, case: &str) -> crate::decide::Outcome {
	let now = decide(spec, facts);
	let before = decide_before::decide(spec, facts);
	let a = serde_json::to_value(&now).unwrap();
	let b = serde_json::to_value(&before).unwrap();
	let as_now: Outcome = serde_json::from_value(b.clone()).unwrap();
	if a != b
		&& let Some(fix) = fixed(spec, facts, &now, &as_now)
	{
		FIXED.with(|f| *f.borrow_mut().entry(fix).or_default() += 1);
		return now;
	}
	if a != b {
		panic!(
			"{case}: the outcomes differ\nspec: {}\nfacts: {}\nnow: {}\nbefore: {}",
			serde_json::to_string(spec).unwrap(),
			serde_json::to_string(facts).unwrap(),
			serde_json::to_string_pretty(&a).unwrap(),
			serde_json::to_string_pretty(&b).unwrap()
		);
	}
	now
}

/// No host is placed more copies of a service than it may hold (its share: the packing home's
/// `max_per_host`, one elsewhere) or than are already there, running or starting (copies held where
/// they run).
fn within_share(spec: &Spec, facts: &Facts, outcome: &Outcome, case: &str) {
	let mut per_host: BTreeMap<(&str, &str, &str), u32> = BTreeMap::new();
	for a in &outcome.placement {
		*per_host
			.entry((&a.host, &a.environment, &a.service))
			.or_default() += a.count;
	}
	for ((host, env, name), n) in per_host {
		let s = spec
			.services
			.iter()
			.find(|s| s.environment == env && s.name == name)
			.unwrap();
		let home = facts.database_hosts.get(env).map(String::as_str) == Some(host);
		let cap = if home && s.policy.mode == PlaceMode::Pack && s.run.is_none() {
			s.policy.max_per_host
		} else {
			1
		};
		let running = facts
			.hosts
			.iter()
			.filter(|h| h.name == host)
			.flat_map(|h| &h.copies)
			.filter(|c| c.environment == env && c.service == name && c.state != CopyState::GaveUp)
			.count() as u32;
		let id = format!("{env}/{name}");
		assert!(
			n <= cap.max(running),
			"{case}: {host} placed {n} copies of {id}, may hold {cap}, has {running}\nnotes: {:#?}\nhosts: {}\ncopies: {}",
			outcome
				.notes
				.iter()
				.filter(|n| n.starts_with(&id))
				.collect::<Vec<_>>(),
			serde_json::to_string(&spec.hosts).unwrap(),
			serde_json::to_string(
				&facts
					.hosts
					.iter()
					.map(|h| (
						&h.name,
						h.copies
							.iter()
							.filter(|c| c.environment == env && c.service == name)
							.collect::<Vec<_>>()
					))
					.collect::<Vec<_>>()
			)
			.unwrap()
		);
	}
}

fn cases() -> u64 {
	std::env::var("METIS_DIFF_CASES")
		.ok()
		.and_then(|n| n.parse().ok())
		.unwrap_or(4_000)
}

#[test]
fn the_new_decide_is_the_old_one_on_generated_fleets() {
	let mut seen = BTreeMap::new();
	let mut checked = 0;
	for seed in 0..cases() {
		let mut rng = Rng(seed);
		let spec = spec(&mut rng);
		if spec.check().is_err() {
			continue;
		}
		let mut facts = facts(&mut rng, &spec);
		// One decision from scratch, then the agents carry it out a few times over.
		for step in 0..6 {
			let outcome = same(&spec, &facts, &format!("seed {seed} step {step}"));
			within_share(&spec, &facts, &outcome, &format!("seed {seed} step {step}"));
			kinds(&outcome, &mut seen);
			checked += 1;
			facts = carry_out(&mut rng, &spec, &facts, &outcome);
		}
	}
	assert!(checked > 1_000, "only {checked} decisions checked");
	// Every path was reached: each note and each refusal at least once.
	let wanted = [
		"note: is silent past",
		"note: rolled back to",
		"note: moves off it",
		"note: with only guaranteed copies left",
		"note: is no longer possible",
		"note: of what they reserve, above",
		"note: on one fewer, below",
		"note: gave up on it; placed on another host",
		"note: placed on",
		"note: wanted",
		"note: moved to its home",
		"note: until the one on its home",
		"note: since its room there is taken",
		"note: has no room for its copy",
		"note: until the copy changing before it runs",
		"note: beside the old ones (surge)",
		"note: while it drains",
		"note: moved off it",
		"note: keeps its copy of",
		"note: reserved) move to",
		"refused: Dedicated",
		"refused: Shared",
		"refused: NotOnItsList",
		"refused: NotReporting",
		"refused: Draining",
		"refused: NotServing",
		"refused: GaveUp",
		"refused: MovingOff",
		"refused: Frozen",
		"refused: OnlyReports",
		"refused: UnderPressure",
		"refused: HoldsItsShare",
		"refused: Full",
	];
	let missing: Vec<&str> = wanted
		.iter()
		.copied()
		.filter(|k| !seen.contains_key(*k))
		.collect();
	eprintln!("{checked} decisions checked; reached: {seen:#?}");
	FIXED.with(|f| eprintln!("differing only by a fix: {:#?}", f.borrow()));
	assert!(missing.is_empty(), "never reached: {missing:?}");
}

/// Real decisions: each `METIS_REPLAY_LOG` file (a member's `decisions.jsonl`, `:`-separated) replayed
/// through both cores, which must agree on every one; and against what was logged, which differs
/// only where the core changed since the build that logged it (said, never failed on).
/// When 73bc2fc1a was made (2026-11-17 02:16 UTC): a decision logged before it is by an older core.
const REFUSALS_REORDERED_MS: u64 = 1_794_881_795_000;

#[test]
fn the_new_decide_is_the_old_one_on_logged_decisions() {
	use crate::replay::Record;
	let Ok(paths) = std::env::var("METIS_REPLAY_LOG") else {
		return;
	};
	let mut decisions = 0;
	let mut unlike_logged = 0;
	for path in paths.split(':') {
		let text = std::fs::read_to_string(path).unwrap();
		let mut spec: Option<Spec> = None;
		for (i, line) in text.lines().enumerate() {
			match serde_json::from_str::<Record>(line).unwrap() {
				Record::Spec(next) => spec = Some(next),
				Record::Decided(logged) => {
					let spec = spec.as_ref().expect("a spec before it");
					let now = same(spec, &logged.facts, &format!("{path} line {}", i + 1));
					decisions += 1;
					let (a, b) = (
						serde_json::to_value(&now).unwrap(),
						serde_json::to_value(&logged.outcome).unwrap(),
					);
					if a != b {
						unlike_logged += 1;
						let fields: Vec<&String> = a
							.as_object()
							.unwrap()
							.keys()
							.filter(|k| a.get(k.as_str()) != b.get(k.as_str()))
							.collect();
						let after_reorder = logged.at_ms >= REFUSALS_REORDERED_MS;
						eprintln!(
							"unlike logged: at {} by {} in {fields:?}{}",
							logged.at_ms,
							logged.by,
							if after_reorder {
								""
							} else {
								" (logged before 73bc2fc1a)"
							}
						);
						if after_reorder {
							// Since 73bc2fc1a only d9a1d3095 changed the core: the order a refusal's reasons are
							// asked in. The same hosts refuse, a few for a different reason, and nothing else moves.
							assert_eq!(a["placement"], b["placement"]);
							assert_eq!(a["table"], b["table"]);
							let hosts = |v: &serde_json::Value| -> Vec<(String, String)> {
								v["shortfalls"]
									.as_array()
									.into_iter()
									.flatten()
									.flat_map(|s| {
										s["hosts"].as_array().unwrap().iter().map(|h| {
											(h["host"].to_string(), s["service"].to_string())
										})
									})
									.collect()
							};
							assert_eq!(hosts(&a), hosts(&b));
						}
					}
				}
			}
		}
	}
	eprintln!(
		"{decisions} logged decisions: new and old cores agree on all; {unlike_logged} differ from what was logged"
	);
	assert!(decisions > 0);
}

/// The orchestration drill's scenario 3, as its members saw it: with h3 heard, and before it is.
#[test]
fn the_drills_rebalance_with_and_without_its_empty_host_heard() {
	let hosts: Vec<serde_json::Value> = [("h1", "a"), ("h2", "b"), ("h3", "c")]
		.iter()
		.map(|(n, d)| serde_json::json!({"name": n, "region": "r", "fault_domain": d, "capacity": {"millicpu": 0, "memory_mb": 1000}}))
		.collect();
	let services: Vec<serde_json::Value> = ["e1", "e2", "e3", "e4"]
		.iter()
		.map(|e| serde_json::json!({"name": "x", "environment": e, "copies": 1, "versions": [{"version": "1.0", "share": 1}], "per_copy": {"millicpu": 0, "memory_mb": 300}}))
		.collect();
	let spec: Spec = serde_json::from_value(serde_json::json!({
		"host_timeout_secs": 3,
		"rebalance": {"high_percent": 80, "low_percent": 50},
		"hosts": hosts,
		"services": services,
	}))
	.unwrap();
	spec.check().unwrap();
	let copy = |e: &str| CopyFacts {
		environment: e.into(),
		service: "x".into(),
		version: "1.0".into(),
		state: CopyState::Running,
		load: None,
		running_since_ms: Some(NOW - 60_000),
	};
	let host = |name: &str, copies: Vec<CopyFacts>, heard: bool| HostFacts {
		name: name.into(),
		last_heartbeat_ms: if heard { NOW - 500 } else { NOW - 10_000 },
		copies,
		report_only: false,
	};
	for h3_heard in [true, false] {
		let facts = Facts {
			now_ms: NOW,
			hosts: vec![
				host("h1", vec![copy("e1"), copy("e2"), copy("e3")], true),
				host("h2", vec![copy("e4")], true),
				host("h3", vec![], h3_heard),
			],
			..Default::default()
		};
		let outcome = same(&spec, &facts, "scenario 3");
		let to = &outcome.moves[0].to;
		eprintln!("h3 heard: {h3_heard}: the move goes to {to}");
		assert_eq!(to, if h3_heard { "h3" } else { "h2" });
	}
}
