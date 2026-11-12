//! Three members on this machine: they elect a leader, take a spec, hear the agents and decide;
//! the leader is killed and a host goes silent, and the two left decide again (MT8, MT3).

use metis::member::config::{AgentTls, HostAbout, SpecSource};
use metis::member::{Member, MemberConfig, start};
use metis::spec::{CopyFacts, HostSpec, Resources, RunSpec, ServiceSpec, Spec, VersionShare};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Duration;

const MEMBER: &str = "member-token-member-token-member-token";
const OPERATOR: &str = "operator-token-operator-token-operator";
const OBSERVER: &str = "observer-token-observer-token-observer";
const ROUTER: &str = "router-token-router-token-router-token";
const READER: &str = "reader-token-reader-token-reader-token";

fn agent_token(host: &str) -> String {
	format!("agent-token-for-{host}-agent-token-for-{host}")
}

fn spec() -> Spec {
	let host = |name: &str| HostSpec {
		name: name.into(),
		region: "r".into(),
		fault_domain: name.into(),
		serves: true,
		draining: false,
		serves_home: false,
		frozen: false,
		finishing: false,
		dedicated_to: None,
		capacity: Resources {
			millicpu: 1000,
			memory_mb: 1024,
		},
	};
	Spec {
		host_timeout_secs: 1,
		hosts: vec![host("h1"), host("h2"), host("h3")],
		services: vec![ServiceSpec {
			name: "functions".into(),
			environment: "control-plane".into(),
			copies: 2,
			versions: vec![VersionShare {
				version: "0.2.1".into(),
				share: 1,
			}],
			per_copy: Resources {
				millicpu: 100,
				memory_mb: 128,
			},
			one_clock: false,
			dedicated_to: None,
			run: Some(RunSpec {
				image_repo: "example/functions".into(),
				port: 9000,
				host_port: 9000,
				health_path: None,
				memory_mb: 128,
				env: BTreeMap::new(),
				command: vec![],
				..Default::default()
			}),
			placement: Default::default(),
			routed: true,
			scale: None,
			policy: Default::default(),
		}],
		database_cluster: None,
		agents: Vec::new(),
	}
}

struct Cluster {
	members: Vec<(Member, tokio::task::JoinHandle<()>, String)>,
	_dirs: Vec<tempfile::TempDir>,
	http: reqwest::Client,
}

async fn cluster(database_primary_region: Option<&str>) -> Cluster {
	cluster_with(database_primary_region, None).await
}

async fn cluster_with(
	database_primary_region: Option<&str>,
	spec_from: Option<SpecSource>,
) -> Cluster {
	cluster_tweaked(database_primary_region, |_, config| {
		config.spec_from = spec_from.clone();
	})
	.await
}

/// Three members, each file changed by `tweak` (by member id) before it starts.
async fn cluster_tweaked(
	database_primary_region: Option<&str>,
	tweak: impl Fn(u64, &mut MemberConfig),
) -> Cluster {
	let mut listeners = Vec::new();
	let mut addresses = BTreeMap::new();
	for id in 1..=3u64 {
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
			.await
			.expect("binds");
		addresses.insert(
			id,
			format!("http://{}", listener.local_addr().expect("address")),
		);
		listeners.push((id, listener));
	}
	let mut members = Vec::new();
	let mut dirs = Vec::new();
	for (id, listener) in listeners {
		let dir = tempfile::tempdir().expect("tempdir");
		let mut config = member_config(id, &dir, addresses.clone(), database_primary_region);
		tweak(id, &mut config);
		let (member, server) = start(config, listener).await.expect("starts");
		members.push((member, server, addresses[&id].clone()));
		dirs.push(dir);
	}
	Cluster {
		members,
		_dirs: dirs,
		http: reqwest::Client::new(),
	}
}

/// One member's file: `members` is who it would make the cluster with (`/init`) and nothing more.
fn member_config(
	id: u64,
	dir: &tempfile::TempDir,
	members: BTreeMap<u64, String>,
	database_primary_region: Option<&str>,
) -> MemberConfig {
	MemberConfig {
		id,
		listen: String::new(),
		data_dir: dir.path().to_path_buf(),
		members,
		member_token: MEMBER.into(),
		operator_token: OPERATOR.into(),
		observer_token: Some(OBSERVER.into()),
		about: ["h1", "h2", "h3"]
			.iter()
			.enumerate()
			.map(|(i, h)| HostAbout {
				host: h.to_string(),
				member: None,
				cloud: "test".into(),
				region: "r".into(),
				wg: format!("10.0.0.{}", i + 1),
				public: None,
				private: None,
			})
			.collect(),
		agent_tokens: ["h1", "h2", "h3"]
			.iter()
			.map(|h| (h.to_string(), agent_token(h)))
			.collect(),
		routers_file: None,
		router_tokens: BTreeMap::new(),
		routes_from: None,
		tick_ms: 100,
		heartbeat_ms: 100,
		election_min_ms: 400,
		election_max_ms: 800,
		database_primary_region: database_primary_region.map(Into::into),
		spec_from: None,
		agent_tls: None,
		relay_reports: false,
	}
}

impl Cluster {
	async fn leader(&self) -> usize {
		for _ in 0..100 {
			for (i, (member, _, _)) in self.members.iter().enumerate() {
				if member.raft.metrics().borrow().state == openraft::ServerState::Leader
					&& !member.stopping.load(std::sync::atomic::Ordering::SeqCst)
				{
					return i;
				}
			}
			tokio::time::sleep(Duration::from_millis(100)).await;
		}
		panic!("no leader");
	}

	async fn report(&self, host: &str, copies: Vec<CopyFacts>) {
		for (member, _, address) in &self.members {
			if member.stopping.load(std::sync::atomic::Ordering::SeqCst) {
				continue;
			}
			let _ = self
				.http
				.post(format!("{address}/report/{host}"))
				.bearer_auth(agent_token(host))
				.json(&copies)
				.send()
				.await;
		}
	}

	async fn report_database(&self, host: &str, databases: Value) {
		for (member, _, address) in &self.members {
			if member.stopping.load(std::sync::atomic::Ordering::SeqCst) {
				continue;
			}
			let _ = self
				.http
				.post(format!("{address}/report/{host}/database"))
				.bearer_auth(agent_token(host))
				.json(&databases)
				.send()
				.await;
		}
	}

	async fn report_projects(&self, host: &str, projects: Value) {
		for (member, _, address) in &self.members {
			if member.stopping.load(std::sync::atomic::Ordering::SeqCst) {
				continue;
			}
			let _ = self
				.http
				.post(format!("{address}/report/{host}/projects"))
				.bearer_auth(agent_token(host))
				.json(&projects)
				.send()
				.await;
		}
	}

	async fn status(&self, at: usize) -> Value {
		self.http
			.get(format!("{}/status", self.members[at].2))
			.bearer_auth(OBSERVER)
			.send()
			.await
			.expect("status")
			.json()
			.await
			.expect("json")
	}

	async fn desired(&self, at: usize, host: &str) -> Option<Value> {
		let response = self
			.http
			.get(format!("{}/desired/{host}", self.members[at].2))
			.bearer_auth(agent_token(host))
			.send()
			.await
			.ok()?;
		if !response.status().is_success() {
			return None;
		}
		response.json().await.ok()
	}
}

fn functions_running() -> Vec<CopyFacts> {
	vec![CopyFacts {
		environment: "control-plane".into(),
		service: "functions".into(),
		version: "0.2.1".into(),
		state: metis::CopyState::Running,
		load: None,
		running_since_ms: None,
	}]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_members_decide_and_survive_their_leader() {
	let c = cluster(None).await;
	let init = c
		.http
		.post(format!("{}/init", c.members[0].2))
		.bearer_auth(OPERATOR)
		.send()
		.await
		.expect("init");
	assert!(
		init.status().is_success(),
		"{}",
		init.text().await.unwrap_or_default()
	);
	let leader = c.leader().await;

	// A follower refuses a spec write and names the leader; the leader takes it.
	let follower = (leader + 1) % 3;
	let refused = c
		.http
		.post(format!("{}/spec", c.members[follower].2))
		.bearer_auth(OPERATOR)
		.json(&spec())
		.send()
		.await
		.expect("spec");
	assert_eq!(refused.status(), 421);
	let applied = c
		.http
		.post(format!("{}/spec", c.members[leader].2))
		.bearer_auth(OPERATOR)
		.json(&spec())
		.send()
		.await
		.expect("spec");
	assert!(applied.status().is_success());

	// The agents report; within a host timeout the leader decides two copies, and every member,
	// a follower included, answers an agent with its share.
	let mut placed = BTreeMap::new();
	for _ in 0..60 {
		c.report("h1", vec![]).await;
		c.report("h2", vec![]).await;
		c.report("h3", vec![]).await;
		tokio::time::sleep(Duration::from_millis(100)).await;
		placed.clear();
		for host in ["h1", "h2", "h3"] {
			if let Some(desired) = c.desired(follower, host).await {
				placed.insert(host, desired["hostServices"].as_array().map_or(0, Vec::len));
			}
		}
		if placed.values().sum::<usize>() == 2 {
			break;
		}
	}
	assert_eq!(placed.values().sum::<usize>(), 2, "{placed:?}");
	let manages = c.desired(follower, "h1").await.expect("desired")["manages"].clone();
	assert_eq!(manages, serde_json::json!(["functions"]));
	// Each host is told every copy placed on it, and only its own (`placed`, packing's drain): the
	// same two copies, each on the host that asked.
	let mut told = 0;
	for host in ["h1", "h2", "h3"] {
		let desired = c.desired(follower, host).await.expect("desired");
		for entry in desired["placed"].as_array().expect("placed") {
			assert_eq!(entry["host"], host, "{desired}");
			assert_eq!(entry["environment"], "control-plane");
			assert_eq!(entry["service"], "functions");
			told += entry["count"].as_u64().expect("count");
		}
	}
	assert_eq!(told, 2);

	// The leader wrote each decision it replicated, with its facts, and they replay to the same
	// decisions (MT6).
	let log = c._dirs[leader].path().join("decisions.jsonl");
	let mut text = String::new();
	for _ in 0..20 {
		text = std::fs::read_to_string(&log).unwrap_or_default();
		if !text.is_empty() {
			break;
		}
		tokio::time::sleep(Duration::from_millis(50)).await;
	}
	let replayed = metis::replay::replay(&text).expect("the decision log replays");
	assert!(replayed.decisions >= 1, "{text}");
	assert!(replayed.differ.is_empty(), "{:?}", replayed.differ);

	// Any agent reads the table, with each host's WireGuard address beside it so its proxy can
	// forward to a copy on another host (MT4); a watcher may not.
	let table: Value = c
		.http
		.get(format!("{}/table", c.members[follower].2))
		.bearer_auth(agent_token("h3"))
		.send()
		.await
		.expect("table")
		.json()
		.await
		.expect("json");
	assert!(table["decided_at_ms"].is_u64(), "{table}");
	assert!(table["table"].is_object(), "{table}");
	assert_eq!(table["hosts"]["h1"], "10.0.0.1", "{table}");
	assert_eq!(table["hosts"]["h3"], "10.0.0.3", "{table}");
	// The hosts the decision heard, for a fleet door choosing between a home and its copies.
	assert_eq!(
		table["heard"],
		serde_json::json!(["h1", "h2", "h3"]),
		"{table}"
	);
	let watcher_table = c
		.http
		.get(format!("{}/table", c.members[follower].2))
		.bearer_auth(OBSERVER)
		.send()
		.await
		.expect("table");
	assert_eq!(watcher_table.status(), 401);
	// A router never asks: it is pushed the table, and its token reads
	// nothing here.
	let router_table = c
		.http
		.get(format!("{}/table", c.members[follower].2))
		.bearer_auth(ROUTER)
		.send()
		.await
		.expect("table");
	assert_eq!(router_table.status(), 401);

	// Another agent's token cannot read this host's state.
	let wrong = c
		.http
		.get(format!("{}/desired/h1", c.members[0].2))
		.bearer_auth(agent_token("h2"))
		.send()
		.await
		.expect("desired");
	assert_eq!(wrong.status(), 401);

	// A watcher reads a member's status, its role and what it saw, and may not write a spec.
	let watched: serde_json::Value = c
		.http
		.get(format!("{}/status", c.members[follower].2))
		.bearer_auth(OBSERVER)
		.send()
		.await
		.expect("status")
		.json()
		.await
		.expect("json");
	let lines: Vec<String> = watched["events"]
		.as_array()
		.expect("events")
		.iter()
		.map(|e| {
			format!(
				"{} {}",
				e["kind"].as_str().unwrap_or(""),
				e["text"].as_str().unwrap_or("")
			)
		})
		.collect();
	assert!(
		lines
			.iter()
			.any(|l| l.starts_with("role follower of member")),
		"{lines:?}"
	);
	assert!(
		lines.iter().any(|l| l == "host first report from h1"),
		"{lines:?}"
	);
	assert!(watched["reports"]["h1"].is_u64() && watched["now_ms"].is_u64());
	let leader_status: serde_json::Value = c
		.http
		.get(format!("{}/status", c.members[leader].2))
		.bearer_auth(OBSERVER)
		.send()
		.await
		.expect("status")
		.json()
		.await
		.expect("json");
	assert!(leader_status["replication"].is_object(), "{leader_status}");
	assert!(
		leader_status["events"]
			.as_array()
			.expect("events")
			.iter()
			.any(|e| e["kind"] == "decision"),
		"{leader_status}"
	);
	let observer_write = c
		.http
		.post(format!("{}/spec", c.members[leader].2))
		.bearer_auth(OBSERVER)
		.json(&spec())
		.send()
		.await
		.expect("spec");
	assert_eq!(observer_write.status(), 401);

	// The two chosen hosts run their copy; then the leader dies, and one of the two hosts goes silent.
	let chosen: Vec<&str> = placed
		.iter()
		.filter(|(_, n)| **n == 1)
		.map(|(h, _)| *h)
		.collect();
	let (silent, alive) = (chosen[0], chosen[1]);
	let spare = ["h1", "h2", "h3"]
		.into_iter()
		.find(|h| !chosen.contains(h))
		.expect("a spare host");
	c.members[leader].0.stop().await;
	c.members[leader].1.abort();
	let new_leader = c.leader().await;
	assert_ne!(new_leader, leader);

	let mut moved = false;
	for _ in 0..80 {
		c.report(alive, functions_running()).await;
		c.report(spare, vec![]).await;
		tokio::time::sleep(Duration::from_millis(100)).await;
		let on_silent = c
			.desired(new_leader, silent)
			.await
			.map(|d| d["hostServices"].as_array().map_or(0, Vec::len));
		let on_spare = c
			.desired(new_leader, spare)
			.await
			.map(|d| d["hostServices"].as_array().map_or(0, Vec::len));
		// The silent host is refused its share, which the leader decided without hearing it: its
		// agent keeps its file rather than remove its own copies as it comes back.
		if on_silent.is_none() && on_spare == Some(1) {
			moved = true;
			break;
		}
	}
	assert!(
		moved,
		"the silent host's copy was not placed on the spare one, or the silent host was answered"
	);
	let refused = c
		.http
		.get(format!("{}/desired/{silent}", c.members[new_leader].2))
		.bearer_auth(agent_token(silent))
		.send()
		.await
		.expect("desired");
	assert_eq!(refused.status(), 503);
	let refused: Value = refused.json().await.expect("json");
	assert!(
		refused["error"]
			.as_str()
			.unwrap_or("")
			.contains("keep what it runs"),
		"{refused}"
	);

	// It reports again: the next decision hears it, and it is answered again.
	let mut answered = None;
	for _ in 0..80 {
		c.report(alive, functions_running()).await;
		c.report(spare, functions_running()).await;
		c.report(silent, vec![]).await;
		tokio::time::sleep(Duration::from_millis(100)).await;
		answered = c.desired(new_leader, silent).await;
		if answered.is_some() {
			break;
		}
	}
	let answered = answered.expect("the host is answered once a decision hears it");
	assert!(answered["hostServices"].is_array(), "{answered}");

	let log: Value = c
		.http
		.get(format!("{}/log", c.members[new_leader].2))
		.bearer_auth(OPERATOR)
		.send()
		.await
		.expect("log")
		.json()
		.await
		.expect("json");
	let notes = log.to_string();
	assert!(
		notes.contains(&format!("host {silent} is silent")),
		"{notes}"
	);

	for (member, server, _) in &c.members {
		member.stop().await;
		server.abort();
	}
}

/// What a guarded copy's agent reports for it: its guard's role and epoch.
fn guard_says(role: &str, epoch: u64) -> Value {
	serde_json::json!([{"ref": "cpprojectref1", "cluster": "cp", "role": role, "epoch": epoch, "at_ms": 1}])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_database_primary_is_a_reported_fact() {
	let c = cluster(Some("east")).await;
	let init = c
		.http
		.post(format!("{}/init", c.members[0].2))
		.bearer_auth(OPERATOR)
		.send()
		.await
		.expect("init");
	assert!(init.status().is_success());
	let leader = c.leader().await;
	let follower = (leader + 1) % 3;
	let mut regions = spec();
	regions.hosts[0].region = "east".into();
	regions.hosts[1].region = "west".into();
	regions.hosts[2].region = "west".into();
	regions.services[0].copies = 3;
	let applied = c
		.http
		.post(format!("{}/spec", c.members[leader].2))
		.bearer_auth(OPERATOR)
		.json(&regions)
		.send()
		.await
		.expect("spec");
	assert!(applied.status().is_success());

	// No database report yet: the region is the config's.
	let first = c.status(leader).await;
	assert_eq!(first["database"]["from"], "config", "{}", first["database"]);
	assert_eq!(first["database"]["region"], "east");

	// The reports say h1 is the primary: the decision carries it, every member holds it, and the
	// table prefers h1's copy.
	let near = |table: &Value| -> BTreeMap<String, bool> {
		table["table"]["control-plane/functions"]
			.as_array()
			.map(|routes| {
				routes
					.iter()
					.map(|r| {
						(
							r["host"].as_str().unwrap_or("").to_string(),
							r["near_database"].as_bool().unwrap_or(false),
						)
					})
					.collect()
			})
			.unwrap_or_default()
	};
	let table = |at: usize| {
		let c = &c;
		async move {
			c.http
				.get(format!("{}/table", c.members[at].2))
				.bearer_auth(OPERATOR)
				.send()
				.await
				.ok()?
				.json::<Value>()
				.await
				.ok()
		}
	};
	let mut settled = false;
	for _ in 0..60 {
		for host in ["h1", "h2", "h3"] {
			c.report(host, functions_running()).await;
		}
		c.report_database("h1", guard_says("primary", 1)).await;
		c.report_database("h2", guard_says("standby", 1)).await;
		tokio::time::sleep(Duration::from_millis(100)).await;
		let held = c.status(follower).await;
		let routes = table(follower).await.map(|t| near(&t)).unwrap_or_default();
		if held["decision"]["database"]["host"] == "h1"
			&& routes.len() == 3
			&& routes.get("h1") == Some(&true)
			&& routes.get("h2") == Some(&false)
		{
			settled = true;
			break;
		}
	}
	assert!(settled, "{}", c.status(follower).await);
	let now = c.status(leader).await;
	assert_eq!(now["database"]["primary_host"], "h1", "{}", now["database"]);
	assert_eq!(now["database"]["from"], "report");
	assert_eq!(now["database"]["epoch"], 1);

	// A failover in progress: h1 fenced, h2 not yet promoted. Nobody says primary, so nothing flips.
	for _ in 0..15 {
		for host in ["h1", "h2", "h3"] {
			c.report(host, functions_running()).await;
		}
		c.report_database("h1", guard_says("fenced", 1)).await;
		c.report_database("h2", guard_says("standby", 1)).await;
		tokio::time::sleep(Duration::from_millis(100)).await;
	}
	let during = c.status(leader).await;
	assert_eq!(
		during["database"]["primary_host"], "h1",
		"{}",
		during["database"]
	);
	assert_eq!(during["database"]["from"], "kept");
	assert_eq!(
		near(&table(leader).await.expect("table")).get("h1"),
		Some(&true)
	);

	// h2 is promoted under a new epoch: routing follows it to its region, and the leader says so.
	let mut followed = false;
	for _ in 0..60 {
		for host in ["h1", "h2", "h3"] {
			c.report(host, functions_running()).await;
		}
		c.report_database("h1", guard_says("fenced", 1)).await;
		c.report_database("h2", guard_says("primary", 2)).await;
		tokio::time::sleep(Duration::from_millis(100)).await;
		let routes = table(follower).await.map(|t| near(&t)).unwrap_or_default();
		if routes.get("h1") == Some(&false)
			&& routes.get("h2") == Some(&true)
			&& routes.get("h3") == Some(&true)
		{
			followed = true;
			break;
		}
	}
	assert!(followed, "{}", c.status(follower).await);
	let after = c.status(leader).await;
	assert_eq!(after["database"]["region"], "west", "{}", after["database"]);
	assert_eq!(after["database"]["epoch"], 2);
	assert!(
		after["events"]
			.as_array()
			.expect("events")
			.iter()
			.any(|e| e["kind"] == "database"
				&& e["text"] == "database primary now h2 in region west (epoch 2)"),
		"{}",
		after["events"]
	);
	assert_eq!(
		after["database_reports"]["h2"]["databases"][0]["role"],
		"primary"
	);

	// Another host's agent cannot speak for this one's database.
	let wrong = c
		.http
		.post(format!("{}/report/h1/database", c.members[leader].2))
		.bearer_auth(agent_token("h2"))
		.json(&guard_says("primary", 9))
		.send()
		.await
		.expect("report");
	assert_eq!(wrong.status(), 401);

	for (member, server, _) in &c.members {
		member.stop().await;
		server.abort();
	}
}

/// What an agent reports for its copy of the control plane's project, guarded or not.
fn copy_is(role: &str) -> Value {
	serde_json::json!([{"ref": "cpprojectref1", "role": role, "epoch": 0}])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_projects_primary_is_in_the_table() {
	let c = cluster(None).await;
	let init = c
		.http
		.post(format!("{}/init", c.members[0].2))
		.bearer_auth(OPERATOR)
		.send()
		.await
		.expect("init");
	assert!(init.status().is_success());
	let leader = c.leader().await;
	let follower = (leader + 1) % 3;
	let applied = c
		.http
		.post(format!("{}/spec", c.members[leader].2))
		.bearer_auth(OPERATOR)
		.json(&spec())
		.send()
		.await
		.expect("spec");
	assert!(applied.status().is_success());
	let table = |at: usize| {
		let c = &c;
		async move {
			c.http
				.get(format!("{}/table", c.members[at].2))
				.bearer_auth(agent_token("h3"))
				.send()
				.await
				.ok()?
				.json::<Value>()
				.await
				.ok()
		}
	};

	// Unguarded copies, no cluster named anywhere: h2's entry says primary, the others standby.
	let mut found = Value::Null;
	for _ in 0..60 {
		for host in ["h1", "h2", "h3"] {
			c.report(host, functions_running()).await;
		}
		c.report_projects("h1", copy_is("standby")).await;
		c.report_projects("h2", copy_is("primary")).await;
		c.report_projects("h3", copy_is("standby")).await;
		tokio::time::sleep(Duration::from_millis(100)).await;
		found = table(follower).await.unwrap_or_default();
		if found["projects"]["cpprojectref1"]["primary"] == "h2" {
			break;
		}
	}
	assert_eq!(
		found["projects"]["cpprojectref1"]["primary"], "h2",
		"{found}"
	);
	assert_eq!(found["projects"]["cpprojectref1"]["wg"], "10.0.0.2");
	// The old fields are still there.
	assert!(
		found["table"].is_object() && found["hosts"].is_object(),
		"{found}"
	);

	// The primary moves by hand: h2 a standby, h1 the primary. The table follows.
	let mut moved = Value::Null;
	for _ in 0..60 {
		for host in ["h1", "h2", "h3"] {
			c.report(host, functions_running()).await;
		}
		c.report_projects("h1", copy_is("primary")).await;
		c.report_projects("h2", copy_is("standby")).await;
		c.report_projects("h3", copy_is("standby")).await;
		tokio::time::sleep(Duration::from_millis(100)).await;
		moved = table(leader).await.unwrap_or_default();
		if moved["projects"]["cpprojectref1"]["primary"] == "h1" {
			break;
		}
	}
	assert_eq!(
		moved["projects"]["cpprojectref1"]["primary"], "h1",
		"{moved}"
	);
	assert_eq!(moved["projects"]["cpprojectref1"]["wg"], "10.0.0.1");
	let status = c.status(leader).await;
	assert!(
		status["events"]
			.as_array()
			.expect("events")
			.iter()
			.any(|e| e["text"] == "project cpprojectref1 primary now h1 (epoch 0)"),
		"{}",
		status["events"]
	);
	assert_eq!(
		status["project_reports"]["h1"]["projects"][0]["role"],
		"primary"
	);

	// Another host's agent cannot speak for this one's projects.
	let wrong = c
		.http
		.post(format!("{}/report/h1/projects", c.members[leader].2))
		.bearer_auth(agent_token("h2"))
		.json(&copy_is("primary"))
		.send()
		.await
		.expect("report");
	assert_eq!(wrong.status(), 401);

	for (member, server, _) in &c.members {
		member.stop().await;
		server.abort();
	}
}

#[tokio::test]
async fn the_store_keeps_its_vote_and_log_across_a_restart() {
	use metis::member::store::LogStore;
	use openraft::storage::RaftLogStorage;
	let dir = tempfile::tempdir().expect("tempdir");
	{
		let mut store = LogStore::open(dir.path()).expect("opens");
		let vote = openraft::Vote::new(3, 2);
		store.save_vote(&vote).await.expect("saves");
	}
	let mut reopened = LogStore::open(dir.path()).expect("reopens");
	assert_eq!(
		reopened.read_vote().await.expect("reads"),
		Some(openraft::Vote::new(3, 2))
	);
	assert_eq!(
		reopened.get_log_state().await.expect("state").last_log_id,
		None
	);
}

/// A member replaced: the leader itself is swapped for a new member 4,
/// which catches up as a learner before it votes; with the old leader stopped, the three left elect,
/// take a write, and send a follower's writer to the new leader's address, which no member's file has.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_is_replaced_and_the_majority_moves_with_it() {
	let mut c = cluster(None).await;
	let init = c
		.http
		.post(format!("{}/init", c.members[0].2))
		.bearer_auth(OPERATOR)
		.send()
		.await
		.expect("init");
	assert!(init.status().is_success());
	let leader = c.leader().await;
	let applied = c
		.http
		.post(format!("{}/spec", c.members[leader].2))
		.bearer_auth(OPERATOR)
		.json(&spec())
		.send()
		.await
		.expect("spec");
	assert!(applied.status().is_success());

	let http = c.http.clone();
	let urls: Vec<String> = c.members.iter().map(|(_, _, a)| a.clone()).collect();
	let change = |at: usize, members: Value, token: &'static str| {
		let request = http
			.post(format!("{}/membership", urls[at]))
			.bearer_auth(token)
			.json(&serde_json::json!({ "members": members }));
		async move { request.send().await.expect("membership") }
	};
	let addresses: BTreeMap<u64, String> = c
		.members
		.iter()
		.enumerate()
		.map(|(i, (_, _, a))| (i as u64 + 1, a.clone()))
		.collect();
	let same_three = serde_json::to_value(&addresses).expect("json");

	// Refused: no operator token, an even number, two, an id moved in place, and on a follower.
	assert_eq!(
		change(leader, same_three.clone(), OBSERVER).await.status(),
		401
	);
	let mut four = addresses.clone();
	four.insert(9, "http://127.0.0.1:1".into());
	assert_eq!(
		change(leader, serde_json::to_value(&four).unwrap(), OPERATOR)
			.await
			.status(),
		422
	);
	let two: BTreeMap<_, _> = addresses.iter().take(2).collect();
	assert_eq!(
		change(leader, serde_json::to_value(&two).unwrap(), OPERATOR)
			.await
			.status(),
		422
	);
	let mut moved = addresses.clone();
	moved.insert(1, "http://127.0.0.1:2".into());
	let refused = change(leader, serde_json::to_value(&moved).unwrap(), OPERATOR).await;
	assert_eq!(refused.status(), 422);
	assert!(refused.text().await.unwrap().contains("new id"));
	let follower = (leader + 1) % 3;
	assert_eq!(change(follower, same_three, OPERATOR).await.status(), 421);

	// Member 4, started with its own file; it never makes a cluster, it is let in.
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
		.await
		.expect("binds");
	let address4 = format!("http://{}", listener.local_addr().expect("address"));
	let dir = tempfile::tempdir().expect("tempdir");
	let leader_id = leader as u64 + 1;
	let mut wanted: BTreeMap<u64, String> = addresses
		.iter()
		.filter(|(id, _)| **id != leader_id)
		.map(|(id, a)| (*id, a.clone()))
		.collect();
	wanted.insert(4, address4.clone());
	let (member4, server4) = start(member_config(4, &dir, wanted.clone(), None), listener)
		.await
		.expect("starts");
	c.members.push((member4, server4, address4.clone()));
	c._dirs.push(dir);

	let answer = change(leader, serde_json::to_value(&wanted).unwrap(), OPERATOR).await;
	assert!(
		answer.status().is_success(),
		"{}",
		answer.text().await.unwrap_or_default()
	);
	let answer: Value = answer.json().await.expect("json");
	let voters: Vec<u64> = wanted.keys().copied().collect();
	assert_eq!(answer["voters"], serde_json::json!(voters), "{answer}");
	assert_eq!(answer["learners"], serde_json::json!([]), "{answer}");
	assert_eq!(answer["nodes"]["4"], address4.as_str(), "{answer}");

	// The old leader stops, as the procedure says; the three left elect among themselves.
	c.members[leader].0.stop().await;
	c.members[leader].1.abort();
	let new_leader = c.leader().await;
	assert_ne!(new_leader, leader);
	let status = c.status(new_leader).await;
	assert_eq!(
		status["membership"]["voters"],
		serde_json::json!(voters),
		"{status}"
	);

	// Member 4 holds the replicated state, and the new set takes a write.
	let mut has_spec = false;
	for _ in 0..50 {
		if c.status(3).await["has_spec"] == true {
			has_spec = true;
			break;
		}
		tokio::time::sleep(Duration::from_millis(100)).await;
	}
	assert!(has_spec, "member 4 caught up on the spec");
	let written = c
		.http
		.post(format!("{}/spec", c.members[new_leader].2))
		.bearer_auth(OPERATOR)
		.json(&spec())
		.send()
		.await
		.expect("spec");
	assert!(written.status().is_success());

	// A follower names the leader by the membership's address, which is right even for member 4.
	let other = (0..4)
		.find(|i| *i != leader && *i != new_leader)
		.expect("a follower");
	let redirected = c
		.http
		.post(format!("{}/spec", c.members[other].2))
		.bearer_auth(OPERATOR)
		.json(&spec())
		.send()
		.await
		.expect("spec");
	assert_eq!(redirected.status(), 421);
	let redirected: Value = redirected.json().await.expect("json");
	assert_eq!(
		redirected["address"],
		c.members[new_leader].2.as_str(),
		"{redirected}"
	);
}

/// An agent gives up on a copy (MT3): it reports `gave-up`, and the
/// leader places that copy on the spare host and says why. The agent side is not built yet; this is
/// the members' half of it, over the same HTTP an agent would use.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_copy_the_agent_gave_up_on_is_placed_on_another_host() {
	let c = cluster(None).await;
	let init = c
		.http
		.post(format!("{}/init", c.members[0].2))
		.bearer_auth(OPERATOR)
		.send()
		.await
		.expect("init");
	assert!(init.status().is_success());
	let leader = c.leader().await;
	let applied = c
		.http
		.post(format!("{}/spec", c.members[leader].2))
		.bearer_auth(OPERATOR)
		.json(&spec())
		.send()
		.await
		.expect("spec");
	assert!(applied.status().is_success());

	let count = |d: Option<Value>| d.map(|d| d["hostServices"].as_array().map_or(0, Vec::len));
	let mut placed: BTreeMap<&str, usize> = BTreeMap::new();
	for _ in 0..60 {
		for host in ["h1", "h2", "h3"] {
			c.report(host, vec![]).await;
		}
		tokio::time::sleep(Duration::from_millis(100)).await;
		placed.clear();
		for host in ["h1", "h2", "h3"] {
			if let Some(n) = count(c.desired(leader, host).await) {
				placed.insert(host, n);
			}
		}
		if placed.values().sum::<usize>() == 2 {
			break;
		}
	}
	let chosen: Vec<&str> = placed
		.iter()
		.filter(|(_, n)| **n == 1)
		.map(|(h, _)| *h)
		.collect();
	assert_eq!(chosen.len(), 2, "{placed:?}");
	let (failing, fine) = (chosen[0], chosen[1]);
	let spare = ["h1", "h2", "h3"]
		.into_iter()
		.find(|h| !chosen.contains(h))
		.expect("spare");

	// The failing host's agent gave up on its copy, in the agent's own words on the wire.
	let gave_up = serde_json::json!([{"environment": "control-plane", "service": "functions", "version": "0.2.1", "state": "gave-up"}]);
	let mut moved = false;
	for _ in 0..60 {
		for (member, _, address) in &c.members {
			if member.stopping.load(std::sync::atomic::Ordering::SeqCst) {
				continue;
			}
			let _ = c
				.http
				.post(format!("{address}/report/{failing}"))
				.bearer_auth(agent_token(failing))
				.json(&gave_up)
				.send()
				.await;
		}
		c.report(fine, functions_running()).await;
		c.report(spare, vec![]).await;
		tokio::time::sleep(Duration::from_millis(100)).await;
		if count(c.desired(leader, failing).await) == Some(0)
			&& count(c.desired(leader, spare).await) == Some(1)
		{
			moved = true;
			break;
		}
	}
	assert!(
		moved,
		"the copy given up on was not placed on the spare host"
	);
	let log: Value = c
		.http
		.get(format!("{}/log", c.members[leader].2))
		.bearer_auth(OPERATOR)
		.send()
		.await
		.expect("log")
		.json()
		.await
		.expect("json");
	assert!(
		log.to_string()
			.contains(&format!("the agent on {failing} gave up on it")),
		"{log}"
	);
	for (member, server, _) in &c.members {
		member.stop().await;
		server.abort();
	}
}

/// An agent that says it only reports (`x-snoutpod-metis-mode: report`) is never given a copy: the
/// two copies go to the other hosts, which is where they will run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_whose_agent_only_reports_is_never_given_a_copy() {
	let c = cluster(None).await;
	let init = c
		.http
		.post(format!("{}/init", c.members[0].2))
		.bearer_auth(OPERATOR)
		.send()
		.await
		.expect("init");
	assert!(init.status().is_success());
	let leader = c.leader().await;
	let applied = c
		.http
		.post(format!("{}/spec", c.members[leader].2))
		.bearer_auth(OPERATOR)
		.json(&spec())
		.send()
		.await
		.expect("spec");
	assert!(applied.status().is_success());

	let count = |d: Option<Value>| d.map(|d| d["hostServices"].as_array().map_or(0, Vec::len));
	let mut placed = BTreeMap::new();
	for _ in 0..60 {
		// h1 first by name, so without the fact it would get a copy.
		for (_, _, address) in &c.members {
			let _ = c
				.http
				.post(format!("{address}/report/h1"))
				.bearer_auth(agent_token("h1"))
				.header(metis::member::server::MODE_HEADER, "report")
				.json(&serde_json::json!([]))
				.send()
				.await;
		}
		c.report("h2", vec![]).await;
		c.report("h3", vec![]).await;
		tokio::time::sleep(Duration::from_millis(100)).await;
		placed.clear();
		for host in ["h1", "h2", "h3"] {
			if let Some(n) = count(c.desired(leader, host).await) {
				placed.insert(host, n);
			}
		}
		if placed.values().sum::<usize>() == 2 {
			break;
		}
	}
	assert_eq!(placed.get("h1"), Some(&0), "{placed:?}");
	assert_eq!(placed.get("h2"), Some(&1), "{placed:?}");
	assert_eq!(placed.get("h3"), Some(&1), "{placed:?}");
	for (member, server, _) in &c.members {
		member.stop().await;
		server.abort();
	}
}

/// The fleet's members read their spec from the control plane: the leader takes
/// it, an operator cannot apply one over it, and a read that fails or a spec with no host serving
/// keeps the last one (MT2).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_fleets_members_read_their_spec_and_keep_the_last() {
	use axum::http::{HeaderMap, StatusCode};
	use std::sync::{Arc, Mutex};
	const READER: &str = "reader-token-reader-token-reader-token";
	// What the source answers: a status and a spec.
	let answer: Arc<Mutex<(u16, Spec)>> = Arc::new(Mutex::new((200, spec())));
	let served = answer.clone();
	let app = axum::Router::new().route(
		"/spec",
		axum::routing::get(move |headers: HeaderMap| {
			let served = served.clone();
			async move {
				let ok = headers.get("authorization").and_then(|v| v.to_str().ok())
					== Some(&format!("Bearer {READER}"));
				let (status, spec) = served.lock().expect("lock").clone();
				if !ok {
					return (StatusCode::UNAUTHORIZED, String::new());
				}
				(
					StatusCode::from_u16(status).expect("status"),
					serde_json::to_string(&spec).expect("json"),
				)
			}
		}),
	);
	// What the control plane was posted, and its answer.
	let posted: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(vec![]));
	let kept = posted.clone();
	let app = app.route(
		"/placement",
		axum::routing::post(move |headers: HeaderMap, body: axum::Json<Value>| {
			let kept = kept.clone();
			async move {
				let ok = headers.get("authorization").and_then(|v| v.to_str().ok())
					== Some(&format!("Bearer {READER}"));
				if !ok {
					return (StatusCode::UNAUTHORIZED, String::new());
				}
				kept.lock().expect("lock").push(body.0);
				(
					StatusCode::OK,
					serde_json::json!({"taken": 0, "refused": ["control-plane: not spread"]})
						.to_string(),
				)
			}
		}),
	);
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
		.await
		.expect("binds");
	let base = format!("http://{}", listener.local_addr().expect("address"));
	let url = format!("{base}/spec");
	let placement_url = Some(format!("{base}/placement"));
	tokio::spawn(async move {
		let _ = axum::serve(listener, app).await;
	});
	let c = cluster_with(
		None,
		Some(SpecSource {
			url,
			token: READER.into(),
			every_secs: 1,
			placement_url,
		}),
	)
	.await;
	let init = c
		.http
		.post(format!("{}/init", c.members[0].2))
		.bearer_auth(OPERATOR)
		.send()
		.await
		.expect("init");
	assert!(init.status().is_success());
	let leader = c.leader().await;
	let copies_agreed = |status: &Value| status["spec"]["services"][0]["copies"].as_u64();
	let mut agreed = None;
	for _ in 0..50 {
		agreed = copies_agreed(&c.status((leader + 1) % 3).await);
		if agreed.is_some() {
			break;
		}
		tokio::time::sleep(Duration::from_millis(100)).await;
	}
	assert_eq!(agreed, Some(2), "a follower holds the spec read");

	// The agents report; the leader decides, and its placement is posted to the control plane with
	// the decision's time, and what the control plane refused is said.
	let mut sent = vec![];
	for _ in 0..80 {
		for host in ["h1", "h2", "h3"] {
			c.report(host, vec![]).await;
		}
		tokio::time::sleep(Duration::from_millis(100)).await;
		sent = posted.lock().expect("lock").clone();
		if sent
			.last()
			.is_some_and(|p| p["placement"].as_array().is_some_and(|a| a.len() == 2))
		{
			break;
		}
	}
	let last = sent.last().expect("a placement was posted").clone();
	assert!(last["decided_at_ms"].as_u64().is_some_and(|t| t > 0));
	assert_eq!(
		last["placement"].as_array().map(Vec::len),
		Some(2),
		"{last}"
	);
	let mut placement_events: Vec<String> = vec![];
	for _ in 0..20 {
		placement_events = c.status(leader).await["events"]
			.as_array()
			.into_iter()
			.flatten()
			.filter(|e| e["kind"] == "placement")
			.filter_map(|e| e["text"].as_str().map(String::from))
			.collect();
		if !placement_events.is_empty() {
			break;
		}
		tokio::time::sleep(Duration::from_millis(100)).await;
	}
	assert!(
		placement_events
			.iter()
			.any(|t| t
				== "the control plane refused part of the placement: control-plane: not spread"),
		"{placement_events:?}"
	);
	// Not posted again while it stands.
	let count = posted.lock().expect("lock").len();
	tokio::time::sleep(Duration::from_millis(3500)).await;
	if c.status(leader).await["decision"]["placement"] == last["placement"] {
		assert_eq!(
			posted.lock().expect("lock").len(),
			count,
			"posted again with nothing changed"
		);
	}

	// An operator cannot apply one over it.
	let applied = c
		.http
		.post(format!("{}/spec", c.members[leader].2))
		.bearer_auth(OPERATOR)
		.json(&spec())
		.send()
		.await
		.expect("spec");
	assert_eq!(applied.status(), 409);

	let events = |status: &Value| -> Vec<String> {
		status["events"]
			.as_array()
			.into_iter()
			.flatten()
			.filter(|e| e["kind"] == "spec")
			.filter_map(|e| e["text"].as_str().map(String::from))
			.collect()
	};
	let wait_for = |words: &'static str| {
		let c = &c;
		async move {
			for _ in 0..40 {
				if events(&c.status(leader).await)
					.iter()
					.any(|t| t.contains(words))
				{
					return true;
				}
				tokio::time::sleep(Duration::from_millis(100)).await;
			}
			false
		}
	};

	// The source fails: the last spec stays, and the member says so.
	answer.lock().expect("lock").0 = 500;
	assert!(wait_for("HTTP 500; deciding from the last spec").await);
	assert_eq!(copies_agreed(&c.status(leader).await), Some(2));

	// A spec with no host serving is not taken: an empty one passes `check` and would take every
	// copy off every host.
	let mut none = spec();
	none.hosts.clear();
	none.services.clear();
	*answer.lock().expect("lock") = (200, none);
	assert!(wait_for("no host serving").await);
	assert_eq!(
		c.status(leader).await["spec"]["hosts"][0]["serves"],
		Value::Bool(true)
	);

	// A real change is taken, and the member says the source answers again.
	let mut three = spec();
	three.services[0].copies = 3;
	*answer.lock().expect("lock") = (200, three);
	let mut agreed = None;
	for _ in 0..40 {
		agreed = copies_agreed(&c.status((leader + 2) % 3).await);
		if agreed == Some(3) {
			break;
		}
		tokio::time::sleep(Duration::from_millis(100)).await;
	}
	assert_eq!(agreed, Some(3), "a follower holds the new spec");
	assert!(
		events(&c.status(leader).await)
			.iter()
			.any(|t| t == "the spec is read again")
	);

	// A quiet fleet: a spec that wants no copies is taken; then one with no host serving is taken too,
	// since the last wanted nothing it could take off (it was not, and the members held a dead spec).
	let mut quiet = spec();
	quiet.services.clear();
	*answer.lock().expect("lock") = (200, quiet);
	let services_agreed = |s: &Value| s["spec"]["services"].as_array().map(|a| a.len());
	let mut taken = None;
	for _ in 0..40 {
		taken = services_agreed(&c.status(leader).await);
		if taken == Some(0) {
			break;
		}
		tokio::time::sleep(Duration::from_millis(100)).await;
	}
	assert_eq!(taken, Some(0), "the spec wanting no copies is taken");
	let mut empty = spec();
	empty.hosts.clear();
	empty.services.clear();
	*answer.lock().expect("lock") = (200, empty);
	let mut hosts = None;
	for _ in 0..40 {
		hosts = c.status(leader).await["spec"]["hosts"]
			.as_array()
			.map(|a| a.len());
		if hosts == Some(0) {
			break;
		}
		tokio::time::sleep(Duration::from_millis(100)).await;
	}
	assert_eq!(
		hosts,
		Some(0),
		"no host serving, after a spec that wanted nothing: taken"
	);
}

/// The fleet's agents reach their members over TLS on a public address, which serves their routes
/// and nothing else, and a report reaches every member though the agent called only one of them
/// (`relay_reports`: home-2 has no public address). Needs `openssl` to make the certificate.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agents_reach_one_member_over_tls_and_every_member_hears_them() {
	let dir = tempfile::tempdir().expect("tempdir");
	// As an operator makes them: the members' own CA, and a member's certificate
	// signed by it naming the address the agents dial.
	let at = |name: &str| dir.path().join(name);
	let (cert, key) = (at("agent.crt"), at("agent.key"));
	std::fs::write(
		at("ext"),
		"subjectAltName=IP:127.0.0.1\nbasicConstraints=critical,CA:FALSE\nextendedKeyUsage=serverAuth\n",
	)
	.expect("ext");
	let ec = [
		"-newkey",
		"ec",
		"-pkeyopt",
		"ec_paramgen_curve:P-256",
		"-nodes",
	];
	let steps: [Vec<std::ffi::OsString>; 3] = [
		[
			"req",
			"-x509",
			"-days",
			"1",
			"-subj",
			"/CN=snout-metis-fleet CA",
		]
		.iter()
		.chain(ec.iter())
		.map(Into::into)
		.chain([
			"-keyout".into(),
			at("ca.key").into(),
			"-out".into(),
			at("ca.crt").into(),
		])
		.collect(),
		["req", "-subj", "/CN=snout-metis-fleet 1"]
			.iter()
			.chain(ec.iter())
			.map(Into::into)
			.chain([
				"-keyout".into(),
				key.clone().into(),
				"-out".into(),
				at("csr").into(),
			])
			.collect(),
		["x509", "-req", "-days", "1", "-CAcreateserial"]
			.iter()
			.map(Into::into)
			.chain([
				"-in".into(),
				at("csr").into(),
				"-CA".into(),
				at("ca.crt").into(),
				"-CAkey".into(),
				at("ca.key").into(),
				"-extfile".into(),
				at("ext").into(),
				"-out".into(),
				cert.clone().into(),
			])
			.collect(),
	];
	for step in &steps {
		let made = std::process::Command::new("openssl").args(step).output();
		if !made.as_ref().is_ok_and(|o| o.status.success()) {
			eprintln!("no openssl to make a certificate with: skipped ({made:?})");
			return;
		}
	}
	let port = {
		let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("binds");
		probe.local_addr().expect("address").port()
	};
	let (cert_path, key_path) = (cert.clone(), key.clone());
	let c = cluster_tweaked(None, move |id, config| {
		config.relay_reports = true;
		if id == 1 {
			config.agent_tls = Some(AgentTls {
				listen: format!("127.0.0.1:{port}"),
				cert: cert_path.clone(),
				key: key_path.clone(),
			});
		}
	})
	.await;
	let ca =
		reqwest::Certificate::from_pem(&std::fs::read(at("ca.crt")).expect("ca")).expect("pem");
	let agent = reqwest::Client::builder()
		.add_root_certificate(ca)
		.build()
		.expect("client");
	let public = format!("https://127.0.0.1:{port}");

	// Its own routes, over TLS, with its own token.
	let reported = agent
		.post(format!("{public}/report/h1"))
		.bearer_auth(agent_token("h1"))
		.json(&functions_running())
		.send()
		.await
		.expect("report");
	assert_eq!(reported.status(), 204);
	let projects = agent
		.post(format!("{public}/report/h1/projects"))
		.bearer_auth(agent_token("h1"))
		.json(&serde_json::json!([{"ref": "abcdefghjkmnp", "role": "primary", "epoch": 0}]))
		.send()
		.await
		.expect("projects");
	assert_eq!(projects.status(), 204);
	// Nothing else: no status, no spec, no raft, whatever the token.
	for (path, token) in [
		("/status", OBSERVER),
		("/spec", OPERATOR),
		("/raft/vote", MEMBER),
	] {
		let answer = agent
			.post(format!("{public}{path}"))
			.bearer_auth(token)
			.send()
			.await
			.expect("answer");
		assert_eq!(
			answer.status(),
			404,
			"{path} answered over the agents' listener"
		);
	}
	// Another host's token speaks for that host only, here as on the members' own listener.
	let other = agent
		.post(format!("{public}/report/h2"))
		.bearer_auth(agent_token("h1"))
		.json(&functions_running())
		.send()
		.await
		.expect("report");
	assert_eq!(other.status(), 401);

	// Every member heard h1, its copies and its project, though only member 1 was called.
	for at in 0..3 {
		let mut heard = Value::Null;
		for _ in 0..30 {
			let status = c.status(at).await;
			if status["reports"]["h1"].is_u64() && status["project_reports"]["h1"].is_object() {
				heard = status;
				break;
			}
			tokio::time::sleep(Duration::from_millis(100)).await;
		}
		assert!(
			heard["reports"]["h1"].is_u64(),
			"member {} never heard h1",
			at + 1
		);
		assert_eq!(heard["copies"]["h1"][0]["service"], "functions");
		assert_eq!(
			heard["project_reports"]["h1"]["projects"][0]["ref"],
			"abcdefghjkmnp"
		);
	}
	// A relay is a member's call: an agent's token cannot make one.
	let forged = c
		.http
		.post(format!("{}/relay/h2", c.members[1].2))
		.bearer_auth(agent_token("h1"))
		.json(&functions_running())
		.send()
		.await
		.expect("relay");
	assert_eq!(forged.status(), 401);
}

/// A fleet copy host's agent is admitted by the token the control plane gave it at enrolment, named
/// in the spec by its sha256: before the spec names it, with another
/// host's token, or once the spec no longer names it, it is refused; with its own, its report and
/// the table are taken.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_copy_hosts_agent_is_admitted_by_the_spec() {
	let c = cluster(None).await;
	let init = c
		.http
		.post(format!("{}/init", c.members[0].2))
		.bearer_auth(OPERATOR)
		.send()
		.await
		.expect("init");
	assert!(init.status().is_success());
	let leader = c.leader().await;
	let address = c.members[leader].2.clone();
	let token = "copy-host-token-copy-host-token-copy-host-token-0123";
	let digest: String = ring::digest::digest(&ring::digest::SHA256, token.as_bytes())
		.as_ref()
		.iter()
		.map(|b| format!("{b:02x}"))
		.collect();
	let report = |bearer: &'static str| {
		let http = c.http.clone();
		let address = address.clone();
		async move {
			http.post(format!("{address}/report/h4"))
				.bearer_auth(bearer)
				.json(&serde_json::json!([]))
				.send()
				.await
				.expect("report")
				.status()
				.as_u16()
		}
	};
	let apply = |spec: Spec| {
		let http = c.http.clone();
		let address = address.clone();
		async move {
			let applied = http
				.post(format!("{address}/spec"))
				.bearer_auth(OPERATOR)
				.json(&spec)
				.send()
				.await
				.expect("spec");
			assert!(applied.status().is_success(), "{}", applied.status());
		}
	};
	let mut admitting = spec();
	let mut h4 = admitting.hosts[0].clone();
	h4.name = "h4".into();
	admitting.hosts.push(h4);
	let without = admitting.clone();
	admitting.agents = vec![metis::spec::SpecAgent {
		host: "h4".into(),
		token_sha256: digest,
		address: "203.0.113.4".into(),
	}];
	assert!(admitting.check().is_ok());

	apply(without.clone()).await;
	assert_eq!(report(token).await, 401, "before the spec names it");
	apply(admitting).await;
	assert!(
		report(token).await < 300,
		"its own token, once the spec names it"
	);
	let wrong: &'static str = Box::leak(agent_token("h1").into_boxed_str());
	assert_eq!(report(wrong).await, 401, "another host's token");
	let table = c
		.http
		.get(format!("{address}/table"))
		.bearer_auth(token)
		.send()
		.await
		.expect("table");
	assert_ne!(
		table.status().as_u16(),
		401,
		"a spec agent may read the table"
	);
	apply(without).await;
	assert_eq!(report(token).await, 401, "once the spec no longer names it");
	for (member, server, _) in &c.members {
		member.stop().await;
		server.abort();
	}
}

/// A router as it runs: a certificate of its own, made at boot, served
/// over TLS, taking `POST /table` and keeping what it was sent. Needs `openssl`; None without it.
async fn a_router(
	dir: &std::path::Path,
) -> Option<(
	String,
	String,
	std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>>,
)> {
	let (cert, key) = (dir.join("router.crt"), dir.join("router.key"));
	let made = std::process::Command::new("openssl")
		.args([
			"req",
			"-x509",
			"-days",
			"1",
			"-subj",
			"/CN=sr-aaaaaaaa",
			"-newkey",
			"ec",
		])
		.args(["-pkeyopt", "ec_paramgen_curve:P-256", "-nodes", "-keyout"])
		.arg(&key)
		.arg("-out")
		.arg(&cert)
		.output();
	if !made.as_ref().is_ok_and(|o| o.status.success()) {
		eprintln!("no openssl to make a certificate with: skipped ({made:?})");
		return None;
	}
	let der = std::process::Command::new("openssl")
		.args(["x509", "-outform", "der", "-in"])
		.arg(&cert)
		.output()
		.expect("der");
	let sha256: String = ring::digest::digest(&ring::digest::SHA256, &der.stdout)
		.as_ref()
		.iter()
		.map(|b| format!("{b:02x}"))
		.collect();
	let got = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
	let kept = got.clone();
	let app = axum::Router::new().route(
		"/table",
		axum::routing::post(
			move |headers: axum::http::HeaderMap, axum::Json(body): axum::Json<Value>| {
				let kept = kept.clone();
				async move {
					let auth = headers
						.get("authorization")
						.and_then(|v| v.to_str().ok())
						.unwrap_or_default()
						.to_string();
					kept.lock().expect("lock").push((auth, body));
					axum::http::StatusCode::NO_CONTENT
				}
			},
		),
	);
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
		.await
		.expect("binds");
	let url = format!("https://{}", listener.local_addr().expect("address"));
	let tls = metis::member::tls::server_config(&cert, &key).expect("tls");
	tokio::spawn(metis::member::tls::serve(listener, tls, app));
	Some((url, sha256, got))
}

/// Every member pushes the table to the routers in its routers' file, with the region's token, to
/// the certificate pinned there and no other.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_member_pushes_the_table_to_the_routers() {
	let dir = tempfile::tempdir().expect("tempdir");
	let Some((url, sha256, pushed)) = a_router(dir.path()).await else {
		return;
	};
	// A second router answering with the same certificate, listed under another one's pin: never
	// sent anything.
	let other = tempfile::tempdir().expect("tempdir");
	let Some((other_url, _, wrongly)) = a_router(other.path()).await else {
		return;
	};
	let file = dir.path().join("routers.json");
	std::fs::write(
		&file,
		serde_json::json!({"routers": [
			{"name": "sr-aaaaaaaa", "region": "r", "url": url, "cert_sha256": sha256},
			{"name": "sr-bbbbbbbb", "region": "r", "url": other_url, "cert_sha256": "ab".repeat(32)},
		]})
		.to_string(),
	)
	.expect("routers file");
	let routers_file = file.clone();
	// The control plane's routes for the routers, as each member reads them with its own token.
	let routes = tokio::net::TcpListener::bind("127.0.0.1:0")
		.await
		.expect("routes listener");
	let routes_url = format!(
		"http://{}/cloud-metis-router-routes",
		routes.local_addr().expect("addr")
	);
	tokio::spawn(async move {
		let app = axum::Router::new().route(
			"/cloud-metis-router-routes",
			axum::routing::get(|headers: axum::http::HeaderMap| async move {
				if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some(&format!("Bearer {READER}")) {
					return (axum::http::StatusCode::UNAUTHORIZED, axum::Json(serde_json::json!({"error": "no"})));
				}
				(
					axum::http::StatusCode::OK,
					axum::Json(serde_json::json!({"at_ms": 1_700_000_000_000u64, "routes": {"abcdefghjkmnp": {"ref": "abcdefghjkmnp", "state": "ready"}}, "domains": {}})),
				)
			}),
		);
		axum::serve(routes, app).await.ok();
	});
	let c = cluster_tweaked(None, move |_, config| {
		config.routers_file = Some(routers_file.clone());
		config.router_tokens = [("r".to_string(), ROUTER.to_string())].into();
		config.routes_from = Some(metis::member::config::RoutesSource {
			url: routes_url.clone(),
			token: READER.to_string(),
		});
	})
	.await;
	c.http
		.post(format!("{}/init", c.members[0].2))
		.bearer_auth(OPERATOR)
		.send()
		.await
		.expect("init");
	let leader = c.leader().await;
	c.http
		.post(format!("{}/spec", c.members[leader].2))
		.bearer_auth(OPERATOR)
		.json(&spec())
		.send()
		.await
		.expect("spec");
	// Each of the three members sends the table once it has a decision, so three pushes at least.
	let mut seen = Vec::new();
	for _ in 0..100 {
		c.report("h1", vec![]).await;
		c.report("h2", vec![]).await;
		c.report("h3", vec![]).await;
		tokio::time::sleep(Duration::from_millis(100)).await;
		seen = pushed.lock().expect("lock").clone();
		if seen.len() >= 3 {
			break;
		}
	}
	assert!(seen.len() >= 3, "{} pushes", seen.len());
	for (auth, body) in &seen {
		assert_eq!(auth, &format!("Bearer {ROUTER}"));
		assert!(body["decided_at_ms"].is_u64(), "{body}");
		assert!(body["table"].is_object(), "{body}");
		assert!(body["heard"].is_array(), "{body}");
	}
	// The routes ride with the table once each member has read them: the router never asks.
	let mut carried = false;
	for _ in 0..100 {
		if pushed
			.lock()
			.expect("lock")
			.iter()
			.any(|(_, body)| body["access"]["routes"]["abcdefghjkmnp"]["state"] == "ready")
		{
			carried = true;
			break;
		}
		tokio::time::sleep(Duration::from_millis(100)).await;
	}
	assert!(carried, "no push carried the control plane's routes");
	assert!(
		wrongly.lock().expect("lock").is_empty(),
		"a router whose certificate is not the pinned one was sent the table"
	);
}
