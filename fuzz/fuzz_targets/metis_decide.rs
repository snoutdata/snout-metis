//! A spec and the facts, as the members read them: the spec from a file or from the control plane,
//! the facts from what the agents report. Split at the first NUL byte. A spec that passes `check`
//! is decided and compiled for every host; none of it may panic, whatever the facts say.
#![no_main]

use metis::compile::{host_services, merge};
use metis::{Facts, Spec, decide};

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
	let (spec, facts) = match data.iter().position(|b| *b == 0) {
		Some(at) => (&data[..at], &data[at + 1..]),
		None => (data, &b""[..]),
	};
	let Ok(spec) = serde_json::from_slice::<Spec>(spec) else {
		return;
	};
	if spec.check().is_err() {
		return;
	}
	let Ok(facts) = serde_json::from_slice::<Facts>(facts) else {
		return;
	};
	let outcome = decide(&spec, &facts);
	assert_eq!(decide(&spec, &facts), outcome, "the same input gave another decision");
	let _ = serde_json::to_string(&outcome).expect("an outcome serializes");
	for host in &spec.hosts {
		if let Ok(entries) = host_services(&spec, &outcome.placement, &host.name) {
			let _ = merge(&spec, r#"{"projects":[],"hostServices":[]}"#, &entries);
		}
	}
});
