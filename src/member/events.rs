//! What a member saw, newest last: its role changing, the decisions it replicated, and the hosts it
//! heard from and lost. In memory only, for a watcher (`status`); a restart starts it again, and
//! "member up" is then its first line.

use serde::Serialize;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// How many lines a member keeps.
pub const KEEP: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Event {
	pub at_ms: u64,
	/// `member`, `role`, `decision`, `database`, `host`, `spec` or `placement`.
	pub kind: &'static str,
	pub text: String,
}

#[derive(Clone, Default)]
pub struct Events(Arc<Mutex<VecDeque<Event>>>);

impl Events {
	pub fn note(&self, kind: &'static str, text: impl Into<String>) {
		let event = Event {
			at_ms: super::decider::now_ms(),
			kind,
			text: text.into(),
		};
		eprintln!("snout-metis: {}: {}", event.kind, event.text);
		let mut lines = self.0.lock().unwrap_or_else(|e| e.into_inner());
		lines.push_back(event);
		while lines.len() > KEEP {
			lines.pop_front();
		}
	}

	/// The last `n`, oldest first.
	pub fn recent(&self, n: usize) -> Vec<Event> {
		let lines = self.0.lock().unwrap_or_else(|e| e.into_inner());
		lines
			.iter()
			.skip(lines.len().saturating_sub(n))
			.cloned()
			.collect()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn keeps_the_newest() {
		let events = Events::default();
		for i in 0..(KEEP + 5) {
			events.note("host", format!("line {i}"));
		}
		let all = events.recent(KEEP + 10);
		assert_eq!(all.len(), KEEP);
		assert_eq!(all.last().map(|e| e.text.as_str()), Some("line 204"));
		assert_eq!(events.recent(2).len(), 2);
	}
}
