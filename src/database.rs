//! Where the database's primary is, as a FACT the agents report rather than a line of config.
//! Metis follows the database and never moves it (MT1): the table
//! prefers copies in the primary's region (MT4), so after a failover the calls follow the new
//! primary without anybody editing a member's config.
//!
//! The agent on a guarded copy's host reads its guard's status file (`guard.ts` in the host agent:
//! `parseGuardStatus`, `statusFresh`, `effectiveRole`) and reports the copy's role and the guard's
//! epoch. A stale or `down` file is no word at all, so the agent reports nothing for it. Here:
//!
//!  - Only a FRESH report counts: received within the spec's `host_timeout_secs`, from a host the
//!    spec names (its region is the spec's).
//!  - The primary is the copy reporting `primary` with the HIGHEST epoch. A guard's epoch only
//!    grows (it is the grant it acts on), so a copy still saying `primary` under an older epoch is
//!    one that has not heard it was superseded, and is not taken over the newer one.
//!  - Nothing flips on a copy saying it is NOT the primary: during a failover both copies may say
//!    so for a lease or two, and the last known primary is kept until another copy reports
//!    `primary` under an epoch at least as new.
//!  - No report and nothing known: none, and the member falls back to its config's region.
//!
//! Pure, like `decide.rs`: the time and the last known primary are arguments.

use crate::spec::Spec;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// One guarded database copy, as its host's agent reports it from the guard's status file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatabaseReport {
	/// The project whose database it is.
	#[serde(rename = "ref")]
	pub project: String,
	/// The guard's cluster: both copies of one database name the same one.
	pub cluster: String,
	pub role: DatabaseRole,
	pub epoch: u64,
	/// When the guard wrote the status file, on the agent's clock. Information only: freshness is
	/// the member's own clock, when the report arrived.
	pub at_ms: u64,
}

/// The guard's effective role, as the host agent reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DatabaseRole {
	Primary,
	Standby,
	/// A primary its guard has fenced or been told is superseded: not the primary.
	Fenced,
}

/// One host's last database report, and when this member received it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostDatabases {
	pub host: String,
	pub received_ms: u64,
	pub databases: Vec<DatabaseReport>,
}

/// The primary Metis takes as known: replicated with each decision, so a new leader starts from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatabasePrimary {
	pub cluster: String,
	pub host: String,
	pub region: String,
	pub epoch: u64,
}

/// Where a [`DatabasePrimary`] came from this time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PrimaryFrom {
	/// A fresh report says so.
	Report,
	/// No fresh report says otherwise: the last known one, kept.
	Kept,
}

/// The database primary for the spec's cluster, from the hosts' reports and the last one known.
pub fn database_primary(
	spec: &Spec,
	now_ms: u64,
	reports: &[HostDatabases],
	last: Option<&DatabasePrimary>,
) -> Option<(DatabasePrimary, PrimaryFrom)> {
	let timeout_ms = spec.host_timeout_secs.saturating_mul(1000);
	let fresh: Vec<&HostDatabases> = reports
		.iter()
		.filter(|r| {
			now_ms.saturating_sub(r.received_ms) <= timeout_ms
				&& spec.hosts.iter().any(|h| h.name == r.host)
		})
		.collect();
	let cluster = match (&spec.database_cluster, last) {
		(Some(named), _) => named.clone(),
		(None, Some(known)) => known.cluster.clone(),
		(None, None) => {
			let reported: BTreeSet<&str> = fresh
				.iter()
				.flat_map(|r| r.databases.iter().map(|d| d.cluster.as_str()))
				.collect();
			// Two clusters and no word on which is the environment's database: neither.
			if reported.len() != 1 {
				return None;
			}
			reported.first()?.to_string()
		}
	};
	let last = last.filter(|l| l.cluster == cluster);
	// The last known primary's epoch stops being a floor once its own host freshly says the copy
	// is not primary there (standby, fenced, or the cluster gone from its report): then any copy
	// saying primary takes over, whatever its epoch, so a hand restore at a lower epoch is followed
	// (otherwise Metis would hold a promoted copy that lost its data while a restored one serves).
	// With nobody saying primary it is still kept, so a failover in progress does not flip routing.
	let disowned = last.is_some_and(|l| {
		fresh.iter().find(|r| r.host == l.host).is_some_and(|r| {
			!r.databases
				.iter()
				.any(|d| d.cluster == cluster && d.role == DatabaseRole::Primary)
		})
	});
	let best = fresh
		.iter()
		.flat_map(|r| {
			r.databases
				.iter()
				.filter(|d| d.cluster == cluster && d.role == DatabaseRole::Primary)
				.map(move |d| (r.host.as_str(), d.epoch))
		})
		// Highest epoch; at the same epoch the last known host, then the first by name.
		.max_by(|a, b| {
			a.1.cmp(&b.1)
				.then_with(|| {
					let known = |host: &str| last.is_some_and(|l| l.host == host);
					known(a.0).cmp(&known(b.0))
				})
				.then_with(|| b.0.cmp(a.0))
		});
	if let Some((host, epoch)) = best
		&& (disowned || last.is_none_or(|l| epoch >= l.epoch))
		&& let Some(spec_host) = spec.hosts.iter().find(|h| h.name == host)
	{
		return Some((
			DatabasePrimary {
				cluster,
				host: host.to_string(),
				region: spec_host.region.clone(),
				epoch,
			},
			PrimaryFrom::Report,
		));
	}
	last.map(|l| {
		let mut kept = l.clone();
		// The host's region as the spec says it now, if it still names the host.
		if let Some(spec_host) = spec.hosts.iter().find(|h| h.name == l.host) {
			kept.region = spec_host.region.clone();
		}
		(kept, PrimaryFrom::Kept)
	})
}

/// One copy of a project's database on a host, guarded or not, as its agent reports it
/// (`/report/{host}/projects`). A guarded copy
/// carries its guard's role and epoch, as [`DatabaseReport`]; an unguarded one its standalone
/// entry's role and epoch 0. What the proxies need to send a call that reached a standby's door to
/// the door of the project's primary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectReport {
	#[serde(rename = "ref")]
	pub project: String,
	pub role: DatabaseRole,
	#[serde(default)]
	pub epoch: u64,
	/// The guard's cluster, for a guarded copy.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub cluster: Option<String>,
}

impl From<&DatabaseReport> for ProjectReport {
	fn from(d: &DatabaseReport) -> Self {
		ProjectReport {
			project: d.project.clone(),
			role: d.role,
			epoch: d.epoch,
			cluster: Some(d.cluster.clone()),
		}
	}
}

/// One host's last project report, and when this member received it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostProjects {
	pub host: String,
	pub received_ms: u64,
	pub projects: Vec<ProjectReport>,
}

/// Where one project's primary copy is, as Metis takes it: replicated with each decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectPrimary {
	pub host: String,
	pub epoch: u64,
}

/// Each project's primary, by ref, from the hosts' project reports and the ones last known. The
/// rule is [`database_primary`]'s with the ref as the cluster, so it needs no cluster name:
///
///  - Only fresh reports from hosts the spec names count.
///  - The copy saying `primary` with the highest epoch wins (a guarded copy's epoch beats an
///    unguarded one's 0); at the same epoch the last known host, then the first by name. So two
///    entries both saying primary for a moment during a hand cutover do not flip the routing until
///    the old one says otherwise.
///  - With nobody saying `primary`, the last known one is kept, unless its host is gone from the
///    spec or freshly reports without the project at all (the copy was removed there).
pub fn project_primaries(
	spec: &Spec,
	now_ms: u64,
	reports: &[HostProjects],
	last: &BTreeMap<String, ProjectPrimary>,
) -> BTreeMap<String, ProjectPrimary> {
	let timeout_ms = spec.host_timeout_secs.saturating_mul(1000);
	let fresh: Vec<&HostProjects> = reports
		.iter()
		.filter(|r| {
			now_ms.saturating_sub(r.received_ms) <= timeout_ms
				&& spec.hosts.iter().any(|h| h.name == r.host)
		})
		.collect();
	let refs: BTreeSet<&str> = fresh
		.iter()
		.flat_map(|r| r.projects.iter().map(|p| p.project.as_str()))
		.chain(last.keys().map(String::as_str))
		.collect();
	let mut out = BTreeMap::new();
	for project in refs {
		let known = last.get(project);
		// As in [`database_primary`]: once the known primary's own host says the copy is not primary
		// there, its epoch is no floor for whichever copy says primary next.
		let disowned = known.is_some_and(|k| {
			fresh.iter().find(|r| r.host == k.host).is_some_and(|r| {
				!r.projects
					.iter()
					.any(|p| p.project == project && p.role == DatabaseRole::Primary)
			})
		});
		let best = fresh
			.iter()
			.flat_map(|r| {
				r.projects
					.iter()
					.filter(|p| p.project == project && p.role == DatabaseRole::Primary)
					.map(move |p| (r.host.as_str(), p.epoch))
			})
			.max_by(|a, b| {
				a.1.cmp(&b.1)
					.then_with(|| {
						let is_known = |host: &str| known.is_some_and(|k| k.host == host);
						is_known(a.0).cmp(&is_known(b.0))
					})
					.then_with(|| b.0.cmp(a.0))
			});
		if let Some((host, epoch)) = best
			&& (disowned || known.is_none_or(|k| epoch >= k.epoch))
		{
			out.insert(
				project.to_string(),
				ProjectPrimary {
					host: host.to_string(),
					epoch,
				},
			);
			continue;
		}
		if let Some(k) = known {
			let named = spec.hosts.iter().any(|h| h.name == k.host);
			let removed = fresh
				.iter()
				.find(|r| r.host == k.host)
				.is_some_and(|r| !r.projects.iter().any(|p| p.project == project));
			if named && !removed {
				out.insert(project.to_string(), k.clone());
			}
		}
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::spec::{HostSpec, Resources};

	const NOW: u64 = 1_000_000;

	fn spec(cluster: Option<&str>) -> Spec {
		let host = |name: &str, region: &str| HostSpec {
			name: name.into(),
			region: region.into(),
			fault_domain: name.into(),
			serves: true,
			draining: false,
			serves_home: false,
			frozen: false,
			finishing: false,
			dedicated_to: None,
			server: None,
			owner: None,
			capacity: Resources::default(),
		};
		Spec {
			host_timeout_secs: 60,
			hosts: vec![
				host("remote-1", "region-c"),
				host("home-1", "region-a"),
				host("home-2", "region-a"),
			],
			services: vec![],
			database_cluster: cluster.map(Into::into),
			agents: Vec::new(),
			pressure_percent: None,
			rebalance: None,
		}
	}

	fn says(
		host: &str,
		age_ms: u64,
		cluster: &str,
		role: DatabaseRole,
		epoch: u64,
	) -> HostDatabases {
		HostDatabases {
			host: host.into(),
			received_ms: NOW - age_ms,
			databases: vec![DatabaseReport {
				project: "cpprojectref1".into(),
				cluster: cluster.into(),
				role,
				epoch,
				at_ms: NOW - age_ms,
			}],
		}
	}

	fn known(host: &str, region: &str, epoch: u64) -> DatabasePrimary {
		DatabasePrimary {
			cluster: "cp".into(),
			host: host.into(),
			region: region.into(),
			epoch,
		}
	}

	use DatabaseRole::{Fenced, Primary, Standby};

	#[test]
	fn the_primary_with_the_highest_epoch_is_taken() {
		let reports = [
			says("remote-1", 1000, "cp", Primary, 3),
			says("home-1", 1000, "cp", Primary, 4),
			says("home-2", 1000, "cp", Standby, 4),
		];
		let (primary, from) =
			database_primary(&spec(None), NOW, &reports, None).expect("a primary");
		assert_eq!(
			primary,
			DatabasePrimary {
				cluster: "cp".into(),
				host: "home-1".into(),
				region: "region-a".into(),
				epoch: 4,
			}
		);
		assert_eq!(from, PrimaryFrom::Report);
	}

	#[test]
	fn a_stale_report_is_not_a_word() {
		let reports = [
			says("home-1", 61_000, "cp", Primary, 9),
			says("remote-1", 2000, "cp", Primary, 3),
		];
		let (primary, _) = database_primary(&spec(None), NOW, &reports, None).expect("a primary");
		assert_eq!(primary.host, "remote-1");
		// And with only the stale one, nothing is known: the member uses its config.
		assert_eq!(
			database_primary(&spec(None), NOW, &reports[..1], None),
			None
		);
	}

	#[test]
	fn no_flip_while_nobody_says_primary() {
		// A failover in progress: the old primary fenced, the new one not yet promoted.
		let last = known("remote-1", "region-c", 3);
		let reports = [
			says("remote-1", 1000, "cp", Fenced, 3),
			says("home-1", 1000, "cp", Standby, 3),
		];
		let (primary, from) =
			database_primary(&spec(None), NOW, &reports, Some(&last)).expect("kept");
		assert_eq!(primary, last);
		assert_eq!(from, PrimaryFrom::Kept);
		// Then the new one says so, under a newer epoch, and routing follows it.
		let promoted = [
			says("remote-1", 1000, "cp", Fenced, 3),
			says("home-1", 1000, "cp", Primary, 4),
		];
		let (primary, from) =
			database_primary(&spec(None), NOW, &promoted, Some(&last)).expect("moved");
		assert_eq!(
			(
				primary.host.as_str(),
				primary.region.as_str(),
				primary.epoch
			),
			("home-1", "region-a", 4)
		);
		assert_eq!(from, PrimaryFrom::Report);
	}

	#[test]
	fn an_older_epoch_does_not_take_the_primary_back() {
		// The old primary, cut off, still says primary under the epoch it was superseded in.
		let last = known("home-1", "region-a", 4);
		let reports = [says("remote-1", 1000, "cp", Primary, 3)];
		let (primary, from) =
			database_primary(&spec(None), NOW, &reports, Some(&last)).expect("kept");
		assert_eq!((primary.host.as_str(), from), ("home-1", PrimaryFrom::Kept));
	}

	#[test]
	fn a_disowned_primary_is_no_floor_for_a_restored_one() {
		// home-2 promoted at epoch 2 and lost its copy; home-1 was restored by hand at a lower
		// epoch. Once home-2 itself says it is not primary, the restore is taken.
		let last = known("home-2", "region-a", 2);
		let reports = [
			says("home-2", 1000, "cp", Fenced, 2),
			says("home-1", 1000, "cp", Primary, 1),
		];
		let (primary, from) =
			database_primary(&spec(None), NOW, &reports, Some(&last)).expect("restored");
		assert_eq!(
			(primary.host.as_str(), primary.epoch, from),
			("home-1", 1, PrimaryFrom::Report)
		);
		// While the old one still says primary, the lower epoch does not take it.
		let contested = [
			says("home-2", 1000, "cp", Primary, 2),
			says("home-1", 1000, "cp", Primary, 1),
		];
		let (primary, _) =
			database_primary(&spec(None), NOW, &contested, Some(&last)).expect("kept");
		assert_eq!(primary.host, "home-2");
	}

	#[test]
	fn a_project_follows_an_unguarded_restore_once_the_old_primary_disowns_it() {
		let host = |name: &str, role: DatabaseRole, epoch: u64| HostProjects {
			host: name.into(),
			received_ms: NOW,
			projects: vec![ProjectReport {
				project: "cpprojectref1".into(),
				role,
				epoch,
				cluster: None,
			}],
		};
		let last = BTreeMap::from([(
			"cpprojectref1".to_string(),
			ProjectPrimary {
				host: "home-2".into(),
				epoch: 2,
			},
		)]);
		let reports = [
			host("home-2", DatabaseRole::Fenced, 0),
			host("home-1", DatabaseRole::Primary, 0),
		];
		let out = project_primaries(&spec(None), NOW, &reports, &last);
		assert_eq!(out["cpprojectref1"].host, "home-1");
		// Nobody saying primary yet: the old one is kept, so a failover does not flip routing.
		let waiting = [
			host("home-2", DatabaseRole::Fenced, 2),
			host("home-1", DatabaseRole::Standby, 2),
		];
		let out = project_primaries(&spec(None), NOW, &waiting, &last);
		assert_eq!(out["cpprojectref1"].host, "home-2");
	}

	#[test]
	fn the_same_epoch_keeps_the_known_host() {
		let last = known("home-2", "region-a", 5);
		let reports = [
			says("home-1", 1000, "cp", Primary, 5),
			says("home-2", 1000, "cp", Primary, 5),
		];
		let (primary, _) =
			database_primary(&spec(None), NOW, &reports, Some(&last)).expect("a primary");
		assert_eq!(primary.host, "home-2");
		let (first, _) = database_primary(&spec(None), NOW, &reports, None).expect("a primary");
		assert_eq!(first.host, "home-1");
	}

	#[test]
	fn nothing_reported_and_nothing_known_is_none() {
		assert_eq!(database_primary(&spec(None), NOW, &[], None), None);
		let silent = [HostDatabases {
			host: "home-1".into(),
			received_ms: NOW,
			databases: vec![],
		}];
		assert_eq!(database_primary(&spec(None), NOW, &silent, None), None);
	}

	#[test]
	fn the_cluster_is_the_specs_or_the_only_one_reported() {
		let reports = [
			says("remote-1", 1000, "cp", Primary, 3),
			says("home-1", 1000, "other", Primary, 8),
		];
		// Two clusters, and the spec does not say which: neither.
		assert_eq!(database_primary(&spec(None), NOW, &reports, None), None);
		let (primary, _) =
			database_primary(&spec(Some("cp")), NOW, &reports, None).expect("a primary");
		assert_eq!(
			(primary.cluster.as_str(), primary.host.as_str()),
			("cp", "remote-1")
		);
	}

	#[test]
	fn a_host_the_spec_does_not_name_is_not_a_word() {
		let reports = [says("pod-elsewhere", 1000, "cp", Primary, 9)];
		assert_eq!(
			database_primary(&spec(Some("cp")), NOW, &reports, None),
			None
		);
	}

	#[test]
	fn reads_what_the_agent_sends() {
		let body =
			r#"[{"ref":"cpprojectref1","cluster":"cp","role":"fenced","epoch":7,"at_ms":12}]"#;
		let parsed: Vec<DatabaseReport> = serde_json::from_str(body).expect("parses");
		assert_eq!(parsed[0].project, "cpprojectref1");
		assert_eq!(parsed[0].role, Fenced);
	}

	fn projects(host: &str, age_ms: u64, entries: &[(&str, DatabaseRole, u64)]) -> HostProjects {
		HostProjects {
			host: host.into(),
			received_ms: NOW - age_ms,
			projects: entries
				.iter()
				.map(|(r, role, epoch)| ProjectReport {
					project: (*r).into(),
					role: *role,
					epoch: *epoch,
					cluster: None,
				})
				.collect(),
		}
	}

	fn primary_of(map: &BTreeMap<String, ProjectPrimary>, project: &str) -> Option<(String, u64)> {
		map.get(project).map(|p| (p.host.clone(), p.epoch))
	}

	#[test]
	fn each_project_has_its_own_primary_with_no_cluster_name() {
		// An unguarded database: the primary in one region, standbys elsewhere.
		let reports = [
			projects(
				"home-1",
				1000,
				&[("cpprojectref1", Primary, 0), ("aaaaaaaaaaaa", Standby, 0)],
			),
			projects("home-2", 1000, &[("cpprojectref1", Standby, 0)]),
			projects(
				"remote-1",
				1000,
				&[("cpprojectref1", Standby, 0), ("aaaaaaaaaaaa", Primary, 0)],
			),
		];
		let found = project_primaries(&spec(None), NOW, &reports, &BTreeMap::new());
		assert_eq!(
			primary_of(&found, "cpprojectref1"),
			Some(("home-1".into(), 0))
		);
		assert_eq!(
			primary_of(&found, "aaaaaaaaaaaa"),
			Some(("remote-1".into(), 0))
		);
	}

	#[test]
	fn a_guarded_epoch_beats_an_unguarded_claim() {
		let reports = [
			projects("remote-1", 1000, &[("cpprojectref1", Primary, 0)]),
			projects("home-1", 1000, &[("cpprojectref1", Primary, 4)]),
		];
		let found = project_primaries(&spec(None), NOW, &reports, &BTreeMap::new());
		assert_eq!(
			primary_of(&found, "cpprojectref1"),
			Some(("home-1".into(), 4))
		);
	}

	#[test]
	fn a_project_primary_is_kept_while_nobody_says_so_and_follows_a_hand_cutover() {
		let mut last = BTreeMap::new();
		last.insert(
			"cpprojectref1".to_string(),
			ProjectPrimary {
				host: "remote-1".into(),
				epoch: 0,
			},
		);
		// Both say primary for a moment: the known one keeps it.
		let both = [
			projects("remote-1", 1000, &[("cpprojectref1", Primary, 0)]),
			projects("home-1", 1000, &[("cpprojectref1", Primary, 0)]),
		];
		let found = project_primaries(&spec(None), NOW, &both, &last);
		assert_eq!(
			primary_of(&found, "cpprojectref1"),
			Some(("remote-1".into(), 0))
		);
		// Nobody says primary: kept.
		let none = [
			projects("remote-1", 1000, &[("cpprojectref1", Standby, 0)]),
			projects("home-1", 1000, &[("cpprojectref1", Standby, 0)]),
		];
		let found = project_primaries(&spec(None), NOW, &none, &last);
		assert_eq!(
			primary_of(&found, "cpprojectref1"),
			Some(("remote-1".into(), 0))
		);
		// The old entry is a standby now and the new one the primary: routing follows.
		let moved = [
			projects("remote-1", 1000, &[("cpprojectref1", Standby, 0)]),
			projects("home-1", 1000, &[("cpprojectref1", Primary, 0)]),
		];
		let found = project_primaries(&spec(None), NOW, &moved, &last);
		assert_eq!(
			primary_of(&found, "cpprojectref1"),
			Some(("home-1".into(), 0))
		);
	}

	#[test]
	fn an_older_epoch_does_not_take_a_project_back_and_stale_is_no_word() {
		let mut last = BTreeMap::new();
		last.insert(
			"cpprojectref1".to_string(),
			ProjectPrimary {
				host: "home-1".into(),
				epoch: 4,
			},
		);
		let reports = [
			projects("remote-1", 1000, &[("cpprojectref1", Primary, 3)]),
			projects("home-2", 61_000, &[("cpprojectref1", Primary, 9)]),
		];
		let found = project_primaries(&spec(None), NOW, &reports, &last);
		assert_eq!(
			primary_of(&found, "cpprojectref1"),
			Some(("home-1".into(), 4))
		);
	}

	#[test]
	fn a_project_removed_from_its_primarys_host_is_forgotten() {
		let mut last = BTreeMap::new();
		last.insert(
			"cpprojectref1".to_string(),
			ProjectPrimary {
				host: "home-1".into(),
				epoch: 0,
			},
		);
		// Its host reports, without the project: gone.
		let reports = [projects("home-1", 1000, &[])];
		assert!(project_primaries(&spec(None), NOW, &reports, &last).is_empty());
		// Its host silent: kept.
		let found = project_primaries(&spec(None), NOW, &[], &last);
		assert_eq!(
			primary_of(&found, "cpprojectref1"),
			Some(("home-1".into(), 0))
		);
		// A host the spec no longer names: gone, and never taken from a report either.
		let mut elsewhere = BTreeMap::new();
		elsewhere.insert(
			"cpprojectref1".to_string(),
			ProjectPrimary {
				host: "pod-elsewhere".into(),
				epoch: 0,
			},
		);
		assert!(project_primaries(&spec(None), NOW, &[], &elsewhere).is_empty());
		let stranger = [projects(
			"pod-elsewhere",
			1000,
			&[("cpprojectref1", Primary, 9)],
		)];
		assert!(project_primaries(&spec(None), NOW, &stranger, &BTreeMap::new()).is_empty());
	}

	#[test]
	fn reads_what_the_agent_sends_for_its_projects() {
		let body = r#"[{"ref":"cpprojectref1","role":"standby","epoch":0},{"ref":"aaaaaaaaaaaa","role":"primary","epoch":7,"cluster":"cp"}]"#;
		let parsed: Vec<ProjectReport> = serde_json::from_str(body).expect("parses");
		assert_eq!(parsed[0].role, Standby);
		assert_eq!(parsed[1].cluster.as_deref(), Some("cp"));
		let bare: Vec<ProjectReport> =
			serde_json::from_str(r#"[{"ref":"cpprojectref1","role":"primary"}]"#).expect("parses");
		assert_eq!(bare[0].epoch, 0);
	}
}
