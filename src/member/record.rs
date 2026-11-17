//! The leader's side of the decision log (MT6, `replay.rs`): `decisions.jsonl` in the member's data
//! directory, one line per decision it replicated, with the spec written again before the first
//! decision taken from a spec this file has not seen. Past `MAX_BYTES` the file is moved to
//! `decisions.1.jsonl` (replacing the one before) and the next starts with its spec, so each file
//! replays on its own.
//!
//! Only the leader writes, so each member's file holds the decisions it took; the three together
//! are the whole history. A record that cannot be written is said in the member's events and never
//! stops a decision (MT2).

use crate::replay::{Logged, Record};
use crate::spec::Spec;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

/// How large one file grows before it is moved aside.
pub const MAX_BYTES: u64 = 64 * 1024 * 1024;

pub struct DecisionLog {
	dir: PathBuf,
	max_bytes: u64,
	/// The spec last written to the current file. None at start and after a move, so the next
	/// record begins with it.
	written: Mutex<Option<Spec>>,
}

impl DecisionLog {
	pub fn new(dir: PathBuf) -> Self {
		DecisionLog::with_limit(dir, MAX_BYTES)
	}

	pub fn with_limit(dir: PathBuf, max_bytes: u64) -> Self {
		DecisionLog {
			dir,
			max_bytes,
			written: Mutex::new(None),
		}
	}

	pub fn path(&self) -> PathBuf {
		self.dir.join("decisions.jsonl")
	}

	/// Appends one decision, and its spec first when the file does not hold it yet.
	pub fn write(&self, spec: &Spec, logged: Logged) -> std::io::Result<()> {
		let mut written = self.written.lock().unwrap_or_else(|e| e.into_inner());
		let path = self.path();
		let decided = line(&Record::Decided(logged))?;
		let size = std::fs::metadata(&path).map_or(0, |m| m.len());
		if size > 0 && size + decided.len() as u64 > self.max_bytes {
			std::fs::rename(&path, self.dir.join("decisions.1.jsonl"))?;
			*written = None;
		}
		let mut text = String::new();
		if written.as_ref() != Some(spec) {
			text.push_str(&line(&Record::Spec(spec.clone()))?);
		}
		text.push_str(&decided);
		std::fs::create_dir_all(&self.dir)?;
		let mut file = std::fs::OpenOptions::new()
			.create(true)
			.append(true)
			.open(&path)?;
		file.write_all(text.as_bytes())?;
		*written = Some(spec.clone());
		Ok(())
	}
}

fn line(record: &Record) -> std::io::Result<String> {
	let mut text = serde_json::to_string(record).map_err(std::io::Error::other)?;
	text.push('\n');
	Ok(text)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::decide::decide;
	use crate::replay::replay;
	use crate::spec::{Facts, HostFacts, HostSpec, Resources};

	fn spec(timeout: u64) -> Spec {
		Spec {
			host_timeout_secs: timeout,
			hosts: vec![HostSpec {
				name: "h1".into(),
				region: "r".into(),
				fault_domain: "a".into(),
				serves: true,
				draining: false,
				serves_home: false,
				frozen: false,
				finishing: false,
				dedicated_to: None,
				server: None,
				owner: None,
				room: None,
				capacity: Resources::default(),
			}],
			services: vec![],
			database_cluster: None,
			agents: Vec::new(),
			pressure_percent: None,
			rebalance: None,
		}
	}

	fn logged(spec: &Spec, at: u64) -> Logged {
		let facts = Facts {
			now_ms: at,
			hosts: vec![HostFacts {
				name: "h1".into(),
				last_heartbeat_ms: at,
				copies: vec![],
				report_only: false,
			}],
			..Default::default()
		};
		Logged {
			at_ms: at,
			by: 1,
			outcome: decide(spec, &facts),
			facts,
		}
	}

	#[test]
	fn the_spec_is_written_once_per_file_and_the_file_replays() {
		let dir = tempfile::tempdir().expect("a temporary directory");
		let log = DecisionLog::new(dir.path().to_path_buf());
		let (a, b) = (spec(60), spec(30));
		log.write(&a, logged(&a, 1)).expect("written");
		log.write(&a, logged(&a, 2)).expect("written");
		log.write(&b, logged(&b, 3)).expect("written");
		let text = std::fs::read_to_string(log.path()).expect("read");
		assert_eq!(text.lines().count(), 5);
		assert_eq!(
			text.lines().filter(|l| l.starts_with("{\"spec\"")).count(),
			2
		);
		let out = replay(&text).expect("it replays");
		assert_eq!((out.decisions, out.differ.len()), (3, 0));
	}

	#[test]
	fn a_full_file_is_moved_aside_and_the_next_starts_with_its_spec() {
		let dir = tempfile::tempdir().expect("a temporary directory");
		let log = DecisionLog::with_limit(dir.path().to_path_buf(), 600);
		let a = spec(60);
		for at in 1..=6 {
			log.write(&a, logged(&a, at)).expect("written");
		}
		let old = std::fs::read_to_string(dir.path().join("decisions.1.jsonl")).expect("moved");
		let now = std::fs::read_to_string(log.path()).expect("read");
		for text in [&old, &now] {
			assert!(text.starts_with("{\"spec\""));
			assert!(replay(text).expect("it replays").differ.is_empty());
		}
	}
}
