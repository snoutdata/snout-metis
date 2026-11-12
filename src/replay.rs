//! The decision log (MT6): each decision a leader replicated, with the facts it decided from, and
//! the spec before the first decision taken from it. Replaying it runs every logged decision's
//! facts through the core again and says where the outcome differs from the one logged, so a
//! decision can be explained after the fact, and a change to the core checked against real ones
//! (MT7: the same input gives the same decision).
//!
//! A member writes it (`member/record.rs`); `snout-metis replay --log <file>` reads it. One JSON
//! record per line: `{"spec": …}` or `{"decided": …}`.

use crate::decide::{Outcome, decide};
use crate::spec::{Facts, Spec};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Record {
	/// The spec the decisions after it were taken from, until the next one.
	Spec(Spec),
	Decided(Logged),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Logged {
	/// The leader's clock when it decided, and who it was.
	pub at_ms: u64,
	pub by: u64,
	pub facts: Facts,
	pub outcome: Outcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Replayed {
	pub decisions: usize,
	/// One line for each decision the core no longer takes as logged.
	pub differ: Vec<String>,
}

/// Replays a log's text. Refuses a log it cannot read: a line that is no record, or a decision with
/// no spec before it in the file.
pub fn replay(text: &str) -> Result<Replayed, String> {
	let mut spec: Option<Spec> = None;
	let mut out = Replayed::default();
	for (i, line) in text.lines().enumerate() {
		if line.trim().is_empty() {
			continue;
		}
		let record: Record =
			serde_json::from_str(line).map_err(|e| format!("line {}: {e}", i + 1))?;
		match record {
			Record::Spec(next) => {
				next.check()
					.map_err(|e| format!("line {}: the spec is refused: {e}", i + 1))?;
				spec = Some(next);
			}
			Record::Decided(logged) => {
				let Some(spec) = &spec else {
					return Err(format!("line {}: a decision with no spec before it", i + 1));
				};
				out.decisions += 1;
				let again = decide(spec, &logged.facts);
				let parts = differs(&logged.outcome, &again);
				if !parts.is_empty() {
					out.differ.push(format!(
						"line {}: the decision at {} by member {} differs in its {}",
						i + 1,
						logged.at_ms,
						logged.by,
						parts.join(", ")
					));
				}
			}
		}
	}
	Ok(out)
}

fn differs(logged: &Outcome, again: &Outcome) -> Vec<&'static str> {
	let mut parts = Vec::new();
	if logged.placement != again.placement {
		parts.push("placement");
	}
	if logged.table != again.table {
		parts.push("table");
	}
	if logged.notes != again.notes {
		parts.push("notes");
	}
	if logged.heard != again.heard {
		parts.push("heard hosts");
	}
	parts
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::spec::{
		CopyFacts, CopyState, HostFacts, HostSpec, Resources, ServiceSpec, VersionShare,
	};

	fn spec(copies: u32) -> Spec {
		let host = |name: &str| HostSpec {
			name: name.into(),
			region: "region-c".into(),
			fault_domain: name.into(),
			serves: true,
			draining: false,
			serves_home: false,
			frozen: false,
			finishing: false,
			dedicated_to: None,
			capacity: Resources {
				millicpu: 0,
				memory_mb: 4096,
			},
		};
		Spec {
			host_timeout_secs: 60,
			hosts: vec![host("h1"), host("h2")],
			services: vec![ServiceSpec {
				name: "auth".into(),
				environment: "p1".into(),
				copies,
				versions: vec![VersionShare {
					version: "1.0.0".into(),
					share: 1,
				}],
				per_copy: Resources {
					millicpu: 0,
					memory_mb: 256,
				},
				one_clock: false,
				dedicated_to: None,
				run: None,
				placement: Default::default(),
				routed: true,
				scale: None,
				policy: Default::default(),
			}],
			database_cluster: None,
			agents: Vec::new(),
		}
	}

	fn facts(at: u64, running_on: &[&str]) -> Facts {
		Facts {
			now_ms: at,
			hosts: ["h1", "h2"]
				.iter()
				.map(|h| HostFacts {
					name: (*h).into(),
					last_heartbeat_ms: at,
					copies: if running_on.contains(h) {
						vec![CopyFacts {
							environment: "p1".into(),
							service: "auth".into(),
							version: "1.0.0".into(),
							state: CopyState::Running,
							load: None,
							running_since_ms: None,
						}]
					} else {
						vec![]
					},
					report_only: false,
				})
				.collect(),
			..Default::default()
		}
	}

	fn line(record: &Record) -> String {
		serde_json::to_string(record).expect("a record serializes")
	}

	fn logged(spec: &Spec, facts: Facts) -> Record {
		Record::Decided(Logged {
			at_ms: facts.now_ms,
			by: 1,
			outcome: decide(spec, &facts),
			facts,
		})
	}

	#[test]
	fn a_log_replays_to_the_decisions_it_holds() {
		let one = spec(1);
		let two = spec(2);
		let text = [
			line(&Record::Spec(one.clone())),
			line(&logged(&one, facts(1000, &[]))),
			line(&logged(&one, facts(2000, &["h1"]))),
			line(&Record::Spec(two.clone())),
			line(&logged(&two, facts(3000, &["h1"]))),
		]
		.join("\n");
		assert_eq!(
			replay(&text),
			Ok(Replayed {
				decisions: 3,
				differ: vec![]
			})
		);
	}

	#[test]
	fn a_decision_the_core_no_longer_takes_is_named() {
		let one = spec(1);
		let mut changed = logged(&one, facts(1000, &[]));
		if let Record::Decided(l) = &mut changed {
			l.outcome.placement.clear();
			l.outcome.notes.push("a note it never wrote".into());
		}
		let text = [line(&Record::Spec(one)), line(&changed)].join("\n");
		let out = replay(&text).expect("it reads");
		assert_eq!(out.decisions, 1);
		assert_eq!(
			out.differ,
			["line 2: the decision at 1000 by member 1 differs in its placement, notes"]
		);
	}

	#[test]
	fn a_log_it_cannot_read_is_refused() {
		let one = spec(1);
		assert!(
			replay(&line(&logged(&one, facts(1000, &[]))))
				.unwrap_err()
				.contains("no spec before it")
		);
		assert!(replay("{\"nope\": 1}").unwrap_err().starts_with("line 1:"));
		assert!(
			replay(&line(&Record::Spec(spec(0))))
				.unwrap_err()
				.contains("the spec is refused")
		);
	}
}
