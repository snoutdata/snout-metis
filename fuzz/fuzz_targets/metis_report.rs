//! What an agent posts to a member: its copies (`/report/{host}`), its guarded databases
//! (`/report/{host}/database`) and its projects' copies (`/report/{host}/projects`), taken into the
//! facts and the primaries as a member takes them. The first byte picks the route.
#![no_main]

use metis::database::{HostDatabases, HostProjects, ProjectReport, project_primaries};
use metis::{CopyFacts, DatabaseReport, Facts, HostFacts, Spec, database_primary, decide};
use std::collections::BTreeMap;

const SPEC: &str = r#"{"host_timeout_secs":30,
	"hosts":[
		{"name":"h1","region":"r1","fault_domain":"d1","capacity":{"millicpu":2000,"memory_mb":2048}},
		{"name":"h2","region":"r2","fault_domain":"d2","capacity":{"millicpu":2000,"memory_mb":2048}}],
	"services":[{"name":"auth","environment":"control-plane","copies":2,
		"versions":[{"version":"1","share":1}],"per_copy":{"millicpu":100,"memory_mb":128}}]}"#;
const NOW: u64 = 1_000_000;

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
	let Some((route, body)) = data.split_first() else {
		return;
	};
	let spec: Spec = serde_json::from_str(SPEC).expect("the spec parses");
	match route % 3 {
		0 => {
			let Ok(copies) = serde_json::from_slice::<Vec<CopyFacts>>(body) else {
				return;
			};
			let facts = Facts {
				now_ms: NOW,
				hosts: vec![HostFacts {
					name: "h1".into(),
					last_heartbeat_ms: NOW,
					copies,
					report_only: false,
				}],
				database_primary_region: None,
				database_hosts: BTreeMap::new(),
				..Default::default()
			};
			let _ = decide(&spec, &facts);
		}
		1 => {
			let Ok(databases) = serde_json::from_slice::<Vec<DatabaseReport>>(body) else {
				return;
			};
			let reports = [HostDatabases {
				host: "h1".into(),
				received_ms: NOW,
				databases,
			}];
			let _ = database_primary(&spec, NOW, &reports, None);
		}
		_ => {
			let Ok(projects) = serde_json::from_slice::<Vec<ProjectReport>>(body) else {
				return;
			};
			let reports = [HostProjects {
				host: "h1".into(),
				received_ms: NOW,
				projects,
			}];
			let _ = project_primaries(&spec, NOW, &reports, &BTreeMap::new());
		}
	}
});
