//! The leader reads the spec from where `spec_from` says (the fleet's members: the control plane's
//! `cloud-metis-fleet-spec`) and replicates it when it changed,
//! as an operator's `/spec` would. A read that fails, or a spec that does not pass `check`, changes
//! nothing: the members keep deciding from the last spec they agreed on (MT2), and the member says
//! so once in its events, and once more when a read succeeds again.
//!
//! One more refusal, because a spec that is valid can still be wrong: a spec with no host serving
//! while the last one had some, and wanted copies, would take every copy off every host. It is kept
//! out, and said. When the last wanted none there is nothing to take off, and it is taken.
//!
//! With `placement_url`, the leader also posts each decision's placement there (the control
//! plane keeps it, tells each copy host its copies and gives it their spec). Posted whenever the
//! replicated placement differs from the last one the control plane took, checked every few seconds,
//! so a post that failed is sent again and a new leader sends its own. What the control plane refused
//! is said in the events when it changes. A placement refused in part is posted again, with the
//! newest decision, every `REFUSED_AGAIN_MS` while it stands: the refusal can be the control plane's
//! state moving on under a decision a little behind it (a project
//! unspread and spread again within a minute: the placement made before the unspread is refused
//! "not spread", and the same placement after the spread would otherwise never be posted).

use super::server::Member;
use super::types::Command;
use crate::decide::Assignment;
use crate::spec::Spec;
use openraft::ServerState;
use std::time::{Duration, Instant};

/// How often the leader checks whether its placement has reached the control plane.
pub const PLACEMENT_CHECK_MS: u64 = 3000;

/// How long a placement the control plane refused in part waits before it is posted again.
pub const REFUSED_AGAIN_MS: u64 = 30_000;

/// The placement last posted and answered: whether the control plane refused part of it, and when.
struct Posted {
	placement: Vec<Assignment>,
	refused: bool,
	at: Instant,
}

/// Whether `placement` is posted now: one never posted, one other than the last answered, or the
/// last again when part of it was refused `REFUSED_AGAIN_MS` ago or more.
fn due(last: Option<&Posted>, placement: &[Assignment], now: Instant) -> bool {
	match last {
		None => true,
		Some(p) if p.placement != placement => true,
		Some(p) => {
			p.refused
				&& now.saturating_duration_since(p.at) >= Duration::from_millis(REFUSED_AGAIN_MS)
		}
	}
}

pub async fn run(member: Member) {
	let Some(source) = member.config.spec_from.clone() else {
		return;
	};
	if let Some(url) = source.placement_url.clone() {
		tokio::spawn(post_placements(member.clone(), url, source.token.clone()));
	}
	let Ok(client) = reqwest::Client::builder()
		.timeout(Duration::from_secs(10))
		.build()
	else {
		member
			.events
			.note("spec", "no HTTP client: the spec is never read");
		return;
	};
	let mut interval = tokio::time::interval(Duration::from_secs(source.every_secs));
	let mut failing: Option<String> = None;
	loop {
		interval.tick().await;
		if member.stopping.load(std::sync::atomic::Ordering::SeqCst) {
			return;
		}
		if member.raft.metrics().borrow().state != ServerState::Leader {
			continue;
		}
		let outcome = match read(&client, &source.url, &source.token).await {
			Ok(spec) => take(&member, spec).await,
			Err(e) => Err(e),
		};
		match outcome {
			Ok(changed) => {
				if failing.take().is_some() {
					member.events.note("spec", "the spec is read again");
				}
				if let Some(text) = changed {
					member.events.note("spec", text);
				}
			}
			Err(e) => {
				if failing.as_ref() != Some(&e) {
					member
						.events
						.note("spec", format!("{e}; deciding from the last spec"));
					failing = Some(e);
				}
			}
		}
	}
}

async fn read(client: &reqwest::Client, url: &str, token: &str) -> Result<Spec, String> {
	let response = client
		.get(url)
		.bearer_auth(token)
		.send()
		.await
		.map_err(|e| format!("the spec could not be read: {e}"))?;
	let status = response.status();
	if !status.is_success() {
		return Err(format!(
			"the spec could not be read: HTTP {}",
			status.as_u16()
		));
	}
	let spec: Spec = response
		.json()
		.await
		.map_err(|e| format!("the spec read is not one: {e}"))?;
	spec.check()
		.map_err(|e| format!("the spec read is refused: {e}"))?;
	Ok(spec)
}

/// Replicates `spec` when it differs from the one agreed on; what changed, for the events.
async fn take(member: &Member, spec: Spec) -> Result<Option<String>, String> {
	let last = member.state.state().await.spec;
	if last.as_ref() == Some(&spec) {
		return Ok(None);
	}
	let serving = |s: &Spec| s.hosts.iter().filter(|h| h.serves).count();
	// Only when the last spec wanted copies: one that wanted none has none to take off, so a quiet
	// fleet whose last serving host went (its hosts retired, the next not reporting yet) takes the new spec rather than deciding from a dead one until a host serves again.
	let wants = |s: &Spec| s.services.iter().any(|v| v.copies > 0);
	if let Some(last) = &last
		&& serving(last) > 0
		&& serving(&spec) == 0
		&& wants(last)
	{
		return Err(format!(
			"the spec read has no host serving, where the last had {}: not taken",
			serving(last)
		));
	}
	let text = format!(
		"spec read: {} hosts ({} serving), {} services",
		spec.hosts.len(),
		serving(&spec),
		spec.services.len()
	);
	member
		.raft
		.client_write(Command::SetSpec(spec))
		.await
		.map_err(|e| format!("the spec read could not be replicated: {e}"))?;
	Ok(Some(text))
}

/// Posts the replicated placement to the control plane whenever it differs from the last one taken.
async fn post_placements(member: Member, url: String, token: String) {
	let Ok(client) = reqwest::Client::builder()
		.timeout(Duration::from_secs(10))
		.build()
	else {
		member
			.events
			.note("placement", "no HTTP client: the placement is never posted");
		return;
	};
	let mut interval = tokio::time::interval(Duration::from_millis(PLACEMENT_CHECK_MS));
	// The placement the control plane last answered, and what it last said.
	let mut taken: Option<Posted> = None;
	let mut said: Option<String> = None;
	loop {
		interval.tick().await;
		if member.stopping.load(std::sync::atomic::Ordering::SeqCst) {
			return;
		}
		if member.raft.metrics().borrow().state != ServerState::Leader {
			// A new leader posts its own; this one may be behind by the time it leads again.
			taken = None;
			continue;
		}
		let Some(decision) = member.state.state().await.decision else {
			continue;
		};
		if !due(taken.as_ref(), &decision.outcome.placement, Instant::now()) {
			continue;
		}
		let text = match post(&client, &url, &token, &decision).await {
			Ok(refused) => {
				taken = Some(Posted {
					placement: decision.outcome.placement.clone(),
					refused: !refused.is_empty(),
					at: Instant::now(),
				});
				if refused.is_empty() {
					"the control plane took the placement".to_string()
				} else {
					format!(
						"the control plane refused part of the placement: {}",
						refused.join("; ")
					)
				}
			}
			Err(e) => e,
		};
		if said.as_ref() != Some(&text) {
			member.events.note("placement", text.clone());
			said = Some(text);
		}
	}
}

async fn post(
	client: &reqwest::Client,
	url: &str,
	token: &str,
	decision: &super::types::Decision,
) -> Result<Vec<String>, String> {
	let response = client
		.post(url)
		.bearer_auth(token)
		.json(&serde_json::json!({
			"decided_at_ms": decision.at_ms,
			"placement": decision.outcome.placement,
		}))
		.send()
		.await
		.map_err(|e| format!("the placement could not be posted: {e}"))?;
	let status = response.status();
	if !status.is_success() {
		return Err(format!(
			"the placement could not be posted: HTTP {}",
			status.as_u16()
		));
	}
	#[derive(serde::Deserialize)]
	struct Answer {
		refused: Vec<String>,
	}
	let answer: Answer = response
		.json()
		.await
		.map_err(|e| format!("the placement's answer is not one: {e}"))?;
	Ok(answer.refused)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn on(host: &str) -> Vec<Assignment> {
		vec![Assignment {
			host: host.into(),
			environment: "e".into(),
			service: "auth".into(),
			version: "1".into(),
			count: 1,
		}]
	}

	#[test]
	fn a_refused_placement_is_posted_again_after_a_while_and_a_taken_one_is_not() {
		let t0 = Instant::now();
		let later = t0 + Duration::from_millis(REFUSED_AGAIN_MS);
		assert!(due(None, &on("h1"), t0), "never posted");
		let taken = Posted {
			placement: on("h1"),
			refused: false,
			at: t0,
		};
		assert!(!due(Some(&taken), &on("h1"), later), "taken, unchanged");
		assert!(due(Some(&taken), &on("h2"), t0), "changed");
		let refused = Posted {
			placement: on("h1"),
			refused: true,
			at: t0,
		};
		assert!(
			!due(
				Some(&refused),
				&on("h1"),
				t0 + Duration::from_millis(PLACEMENT_CHECK_MS)
			),
			"refused, too soon"
		);
		assert!(
			due(Some(&refused), &on("h1"), later),
			"refused, posted again"
		);
		assert!(due(Some(&refused), &on("h2"), t0), "refused, changed");
	}
}
