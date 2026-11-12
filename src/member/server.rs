//! A member's HTTP side: openraft's calls from the other members, the agents' reports (their copies,
//! and their guarded database copies' roles) and their desired state, the proxies' table, and the operator's spec, status and log. Each caller has its
//! own token (member, operator, one per host), and an agent can only speak for its own host.

use super::config::MemberConfig;
use super::events::Events;
use super::network::Network;
use super::store::{LogStore, StateMachineStore};
use super::types::{Command, Node, NodeId, Raft, TypeConfig};
use crate::compile::host_services;
use crate::database::{DatabaseReport, ProjectReport};
use crate::decide::Assignment;
use crate::spec::{CopyFacts, Spec};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use openraft::raft::{AppendEntriesRequest, InstallSnapshotRequest, VoteRequest};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Mutex;

/// The last report from each host: when this member received it, and the copies it listed.
pub type Reports = HashMap<String, (u64, Vec<CopyFacts>)>;

/// The last database report from each host (`/report/{host}/database`): when this member received
/// it, and the guarded copies' roles it listed. Like [`Reports`], each member's own and not
/// replicated; what the leader concluded from them is (`Decision::database`).
pub type DatabaseReports = HashMap<String, (u64, Vec<DatabaseReport>)>;

/// The last project report from each host (`/report/{host}/projects`): every copy of a project's
/// database there, guarded or not, with its role. Each member's own, like [`Reports`].
pub type ProjectReports = HashMap<String, (u64, Vec<ProjectReport>)>;

#[derive(Clone)]
pub struct Member {
	pub raft: Raft,
	pub state: StateMachineStore,
	pub config: Arc<MemberConfig>,
	pub reports: Arc<Mutex<Reports>>,
	pub databases: Arc<Mutex<DatabaseReports>>,
	pub projects: Arc<Mutex<ProjectReports>>,
	pub leader_since: Arc<Mutex<Option<u64>>>,
	pub started_ms: u64,
	pub stopping: Arc<AtomicBool>,
	/// What this member saw, for a watcher.
	pub events: Events,
	/// The hosts this member has noted as silent, so it says so once and says when they return.
	pub silent: Arc<Mutex<HashSet<String>>>,
	/// The hosts whose agent said, on its last report, that it only reports
	/// (`x-snoutpod-metis-mode: report`): nothing is placed there (`HostFacts::report_only`).
	pub report_only: Arc<Mutex<HashSet<String>>>,
	/// Each decision this member replicated as leader, with its facts (MT6, `record.rs`).
	pub decisions: Arc<super::record::DecisionLog>,
	/// What it passes the agents' reports on with, when it relays them (`relay_reports`).
	pub relay: reqwest::Client,
}

/// The header an agent says its mode in on `/report/{host}`: `report` or `apply`. An agent that does
/// not send it is taken as applying, as every agent before it did.
pub const MODE_HEADER: &str = "x-snoutpod-metis-mode";

impl Member {
	/// Stops Raft and the decide loop. The HTTP server is the caller's task to abort.
	pub async fn stop(&self) {
		self.stopping.store(true, Ordering::SeqCst);
		let _ = self.raft.shutdown().await;
	}
}

/// Opens the member's stores, starts Raft, the decide loop and the HTTP server on `listener`.
pub async fn start(
	config: MemberConfig,
	listener: tokio::net::TcpListener,
) -> Result<(Member, tokio::task::JoinHandle<()>), String> {
	config.check()?;
	let log = LogStore::open(&config.data_dir.join("raft")).map_err(|e| e.to_string())?;
	let state =
		StateMachineStore::open(&config.data_dir.join("state")).map_err(|e| e.to_string())?;
	let raft_config = openraft::Config {
		cluster_name: "snout-metis".into(),
		heartbeat_interval: config.heartbeat_ms,
		election_timeout_min: config.election_min_ms,
		election_timeout_max: config.election_max_ms,
		..Default::default()
	}
	.validate()
	.map_err(|e| e.to_string())?;
	let network = Network {
		client: reqwest::Client::new(),
		token: config.member_token.clone(),
	};
	let raft = Raft::new(
		config.id,
		Arc::new(raft_config),
		network,
		log,
		state.clone(),
	)
	.await
	.map_err(|e| e.to_string())?;
	let decisions = Arc::new(super::record::DecisionLog::new(config.data_dir.clone()));
	let member = Member {
		raft,
		state,
		config: Arc::new(config),
		reports: Arc::new(Mutex::new(HashMap::new())),
		databases: Arc::new(Mutex::new(HashMap::new())),
		projects: Arc::new(Mutex::new(HashMap::new())),
		leader_since: Arc::new(Mutex::new(None)),
		started_ms: super::decider::now_ms(),
		stopping: Arc::new(AtomicBool::new(false)),
		events: Events::default(),
		silent: Arc::new(Mutex::new(HashSet::new())),
		report_only: Arc::new(Mutex::new(HashSet::new())),
		decisions,
		relay: reqwest::Client::new(),
	};
	member.events.note(
		"member",
		format!(
			"member {} up, version {}",
			member.config.id,
			env!("CARGO_PKG_VERSION")
		),
	);
	tokio::spawn(watch_role(member.clone()));
	tokio::spawn(super::decider::run(member.clone()));
	tokio::spawn(super::spec_source::run(member.clone()));
	tokio::spawn(super::push::run(member.clone()));
	let app = axum::Router::new()
		.route("/raft/append", post(raft_append))
		.route("/raft/vote", post(raft_vote))
		.route("/raft/snapshot", post(raft_snapshot))
		.route("/init", post(init))
		.route("/membership", get(membership).post(change_membership))
		.route("/spec", post(set_spec))
		.route("/status", get(status))
		.route("/log", get(history))
		.route("/relay/{host}", post(relay))
		.route("/relay/{host}/database", post(relay_database))
		.route("/relay/{host}/projects", post(relay_projects))
		.merge(agent_routes())
		.with_state(member.clone());
	// The agents' own listener over TLS (`agent_tls`): their routes alone, so the internet reaches no
	// member, operator or raft route.
	let agents = match &member.config.agent_tls {
		Some(tls) => {
			let config = super::tls::server_config(&tls.cert, &tls.key)?;
			let bound = tokio::net::TcpListener::bind(&tls.listen)
				.await
				.map_err(|e| format!("{}: {e}", tls.listen))?;
			Some((bound, config, agent_routes().with_state(member.clone())))
		}
		None => None,
	};
	let server = tokio::spawn(async move {
		let members = async {
			let _ = axum::serve(listener, app).await;
		};
		match agents {
			Some((bound, config, routes)) => {
				tokio::join!(members, super::tls::serve(bound, config, routes));
			}
			None => members.await,
		}
	});
	Ok((member, server))
}

/// What an agent may call: its reports, its desired state and the table.
fn agent_routes() -> axum::Router<Member> {
	axum::Router::new()
		.route("/report/{host}", post(report))
		.route("/report/{host}/database", post(report_database))
		.route("/report/{host}/projects", post(report_projects))
		.route("/desired/{host}", get(desired))
		.route("/table", get(table))
}

fn same(a: &str, b: &str) -> bool {
	a.len() == b.len()
		&& a.bytes()
			.zip(b.bytes())
			.fold(0u8, |acc, (x, y)| acc | (x ^ y))
			== 0
}

fn bearer(headers: &HeaderMap) -> &str {
	headers
		.get("authorization")
		.and_then(|v| v.to_str().ok())
		.and_then(|v| v.strip_prefix("Bearer "))
		.unwrap_or("")
}

fn refused() -> Response {
	(
		StatusCode::UNAUTHORIZED,
		Json(json!({"error": "not allowed"})),
	)
		.into_response()
}

fn is_member(member: &Member, headers: &HeaderMap) -> bool {
	same(bearer(headers), &member.config.member_token)
}

fn is_operator(member: &Member, headers: &HeaderMap) -> bool {
	same(bearer(headers), &member.config.operator_token)
}

/// The operator, or a watcher with the observer token: what may read `status` and `log`.
fn may_read(member: &Member, headers: &HeaderMap) -> bool {
	is_operator(member, headers)
		|| member
			.config
			.observer_token
			.as_ref()
			.is_some_and(|t| same(bearer(headers), t))
}

/// Notes each change of role or leader, so a watcher reads an election as lines, not a guess.
async fn watch_role(member: Member) {
	let mut metrics = member.raft.metrics();
	let mut last = None;
	loop {
		let now = {
			let m = metrics.borrow();
			(m.state, m.current_leader, m.current_term)
		};
		if last != Some(now) {
			let (state, leader, term) = now;
			let text = match (state, leader) {
				(openraft::ServerState::Leader, _) => format!("leader in term {term}"),
				(openraft::ServerState::Follower, Some(l)) => {
					format!("follower of member {l} in term {term}")
				}
				(openraft::ServerState::Follower, None) => {
					format!("follower with no leader in term {term}")
				}
				(openraft::ServerState::Candidate, _) => format!("candidate in term {term}"),
				(openraft::ServerState::Learner, _) => format!("learner in term {term}"),
				(openraft::ServerState::Shutdown, _) => "stopped".to_string(),
			};
			member.events.note("role", text);
			last = Some(now);
		}
		if metrics.changed().await.is_err() {
			return;
		}
	}
}

/// Whether the caller is `host`'s agent: by the token in this member's file, or by one the spec
/// names (a fleet copy host's), compared by its sha256 as the control plane keeps it.
async fn is_agent_of(member: &Member, headers: &HeaderMap, host: &str) -> bool {
	if member
		.config
		.agent_tokens
		.get(host)
		.is_some_and(|t| same(bearer(headers), t))
	{
		return true;
	}
	let token = bearer(headers);
	if token.len() < 32 {
		return false;
	}
	let digest = sha256_hex(token);
	member
		.state
		.spec_agents()
		.await
		.iter()
		.any(|(name, hash)| name == host && same(&digest, hash))
}

/// Whether `host` is an agent at all: in this member's file, or in the spec.
async fn is_known_agent(member: &Member, host: &str) -> bool {
	member.config.agent_tokens.contains_key(host)
		|| member
			.state
			.spec_agents()
			.await
			.iter()
			.any(|(name, _)| name == host)
}

/// Whether the caller is any agent (for the table, which every agent may read).
async fn is_any_agent(member: &Member, headers: &HeaderMap) -> bool {
	let token = bearer(headers);
	if member.config.agent_tokens.values().any(|t| same(token, t)) {
		return true;
	}
	if token.len() < 32 {
		return false;
	}
	let digest = sha256_hex(token);
	member
		.state
		.spec_agents()
		.await
		.iter()
		.any(|(_, hash)| same(&digest, hash))
}

fn sha256_hex(value: &str) -> String {
	ring::digest::digest(&ring::digest::SHA256, value.as_bytes())
		.as_ref()
		.iter()
		.map(|b| format!("{b:02x}"))
		.collect()
}

/// Where the leader is, for a write sent to a follower: its address from the membership the members
/// agree on, which a membership change keeps current, else from this member's file.
async fn not_leader(member: &Member) -> Response {
	let leader = member.raft.current_leader().await;
	let address = leader.and_then(|id| {
		member
			.raft
			.metrics()
			.borrow()
			.membership_config
			.membership()
			.get_node(&id)
			.map(|node| node.addr.clone())
			.or_else(|| member.config.members.get(&id).cloned())
	});
	(
		StatusCode::MISDIRECTED_REQUEST,
		Json(json!({"error": "not the leader", "leader": leader, "address": address})),
	)
		.into_response()
}

async fn raft_append(
	State(member): State<Member>,
	headers: HeaderMap,
	Json(rpc): Json<AppendEntriesRequest<TypeConfig>>,
) -> Response {
	if !is_member(&member, &headers) {
		return refused();
	}
	Json(member.raft.append_entries(rpc).await).into_response()
}

async fn raft_vote(
	State(member): State<Member>,
	headers: HeaderMap,
	Json(rpc): Json<VoteRequest<NodeId>>,
) -> Response {
	if !is_member(&member, &headers) {
		return refused();
	}
	Json(member.raft.vote(rpc).await).into_response()
}

async fn raft_snapshot(
	State(member): State<Member>,
	headers: HeaderMap,
	Json(rpc): Json<InstallSnapshotRequest<TypeConfig>>,
) -> Response {
	if !is_member(&member, &headers) {
		return refused();
	}
	Json(member.raft.install_snapshot(rpc).await).into_response()
}

/// Makes the cluster from the configured members, once, on one of them.
async fn init(State(member): State<Member>, headers: HeaderMap) -> Response {
	if !is_operator(&member, &headers) {
		return refused();
	}
	let nodes: BTreeMap<NodeId, Node> = member
		.config
		.members
		.iter()
		.map(|(id, addr)| (*id, Node { addr: addr.clone() }))
		.collect();
	match member.raft.initialize(nodes).await {
		Ok(()) => Json(json!({"initialized": true})).into_response(),
		Err(e) => (StatusCode::CONFLICT, Json(json!({"error": e.to_string()}))).into_response(),
	}
}

/// The voting members and any learners, with their addresses, as the members agree on them: after a
/// membership change this, not each member's file, is who votes.
fn membership_now(member: &Member) -> serde_json::Value {
	let metrics = member.raft.metrics().borrow().clone();
	let membership = metrics.membership_config.membership();
	let nodes: BTreeMap<NodeId, &str> = membership
		.nodes()
		.map(|(id, node)| (*id, node.addr.as_str()))
		.collect();
	json!({
		"voters": membership.voter_ids().collect::<Vec<_>>(),
		"learners": membership.learner_ids().collect::<Vec<_>>(),
		"nodes": nodes,
		"log_id": metrics.membership_config.log_id(),
	})
}

async fn membership(State(member): State<Member>, headers: HeaderMap) -> Response {
	if !may_read(&member, &headers) {
		return refused();
	}
	Json(membership_now(&member)).into_response()
}

/// The voting members an operator asks for, by id and address (`{"members": {"2": "http://…"}}`).
#[derive(serde::Deserialize)]
struct MembershipChange {
	members: BTreeMap<NodeId, String>,
}

/// How long a new member may take to catch up on the log before it is given a vote.
const CATCH_UP: std::time::Duration = std::time::Duration::from_secs(60);

/// Adds or replaces members, on the leader. The body is the WHOLE set
/// of voters wanted, an odd number of at least three (MT8). Each new one is added as a learner first
/// and given a vote only once it has caught up on the log, so the majority never counts a member
/// that holds nothing; then openraft's joint consensus moves to the new set, so at every step a
/// majority of both the old and the new set agree. A member left out is removed outright and should
/// be stopped right after. An existing id at a new address is refused: replace it under a new id.
async fn change_membership(
	State(member): State<Member>,
	headers: HeaderMap,
	Json(change): Json<MembershipChange>,
) -> Response {
	if !is_operator(&member, &headers) {
		return refused();
	}
	let unprocessable = |error: String| {
		(
			StatusCode::UNPROCESSABLE_ENTITY,
			Json(json!({"error": error})),
		)
			.into_response()
	};
	let wanted = change.members;
	if wanted.len() < 3 || wanted.len().is_multiple_of(2) {
		return unprocessable(format!(
			"{} members: an odd number of at least three, so a majority survives one loss (MT8)",
			wanted.len()
		));
	}
	if let Some((id, _)) = wanted.iter().find(|(_, addr)| !addr.starts_with("http")) {
		return unprocessable(format!(
			"member {id}: an address is http://<its WireGuard address>:<port>"
		));
	}
	if member.raft.current_leader().await != Some(member.config.id) {
		return not_leader(&member).await;
	}
	let current: BTreeMap<NodeId, String> = member
		.raft
		.metrics()
		.borrow()
		.membership_config
		.membership()
		.nodes()
		.map(|(id, node)| (*id, node.addr.clone()))
		.collect();
	if let Some((id, addr)) = wanted
		.iter()
		.find(|(id, addr)| current.get(id).is_some_and(|known| known != *addr))
	{
		return unprocessable(format!(
			"member {id} is at {} and cannot move to {addr} in place: give the new one a new id",
			current[id]
		));
	}
	for (id, addr) in wanted.iter().filter(|(id, _)| !current.contains_key(id)) {
		member.events.note(
			"member",
			format!("member {id} at {addr} added as a learner, catching up"),
		);
		let added = tokio::time::timeout(
			CATCH_UP,
			member
				.raft
				.add_learner(*id, Node { addr: addr.clone() }, true),
		)
		.await;
		match added {
			Ok(Ok(_)) => {}
			Ok(Err(e)) => return write_refused(&member, e).await,
			Err(_) => {
				return (
					StatusCode::GATEWAY_TIMEOUT,
					Json(json!({
						"error": format!("member {id} did not catch up within {}s: it is a learner without a vote; nothing else changed", CATCH_UP.as_secs()),
					})),
				)
					.into_response();
			}
		}
	}
	let voters: BTreeSet<NodeId> = wanted.keys().copied().collect();
	match member.raft.change_membership(voters, false).await {
		Ok(written) => {
			member.events.note(
				"member",
				format!(
					"voters now {:?}",
					wanted.keys().copied().collect::<Vec<_>>()
				),
			);
			let mut answer = membership_now(&member);
			answer["applied_at"] = json!(written.log_id);
			Json(answer).into_response()
		}
		Err(e) => write_refused(&member, e).await,
	}
}

/// A membership write openraft refused: the leader's address when this member is no longer it.
async fn write_refused(
	member: &Member,
	error: openraft::error::RaftError<NodeId, openraft::error::ClientWriteError<NodeId, Node>>,
) -> Response {
	match error {
		openraft::error::RaftError::APIError(
			openraft::error::ClientWriteError::ForwardToLeader(_),
		) => not_leader(member).await,
		e => (
			StatusCode::SERVICE_UNAVAILABLE,
			Json(json!({"error": e.to_string()})),
		)
			.into_response(),
	}
}

async fn set_spec(
	State(member): State<Member>,
	headers: HeaderMap,
	Json(spec): Json<Spec>,
) -> Response {
	if !is_operator(&member, &headers) {
		return refused();
	}
	if member.config.spec_from.is_some() {
		return (
			StatusCode::CONFLICT,
			Json(
				json!({"error": "this member reads its spec from spec_from; an operator does not apply one"}),
			),
		)
			.into_response();
	}
	if let Err(e) = spec.check() {
		return (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({"error": e}))).into_response();
	}
	match member.raft.client_write(Command::SetSpec(spec)).await {
		Ok(written) => Json(json!({"applied_at": written.log_id})).into_response(),
		Err(openraft::error::RaftError::APIError(
			openraft::error::ClientWriteError::ForwardToLeader(_),
		)) => not_leader(&member).await,
		Err(e) => (
			StatusCode::SERVICE_UNAVAILABLE,
			Json(json!({"error": e.to_string()})),
		)
			.into_response(),
	}
}

async fn status(State(member): State<Member>, headers: HeaderMap) -> Response {
	if !may_read(&member, &headers) {
		return refused();
	}
	let metrics = member.raft.metrics().borrow().clone();
	let state = member.state.state().await;
	let held = member.reports.lock().await.clone();
	let reports: BTreeMap<&String, u64> = held.iter().map(|(h, (at, _))| (h, *at)).collect();
	let copies: BTreeMap<&String, &Vec<CopyFacts>> =
		held.iter().map(|(h, (_, c))| (h, c)).collect();
	// What the leader has replicated to each member, as the last log index it matched.
	let replication: Option<BTreeMap<NodeId, Option<u64>>> =
		metrics.replication.as_ref().map(|r| {
			r.iter()
				.map(|(id, matched)| (*id, matched.map(|l| l.index)))
				.collect()
		});
	let recent: Vec<_> = state.history.iter().rev().take(20).collect();
	let now = super::decider::now_ms();
	let database = super::decider::database_status(&member, &state, now).await;
	let database_reports: BTreeMap<String, serde_json::Value> = member
		.databases
		.lock()
		.await
		.iter()
		.map(|(h, (at, d))| (h.clone(), json!({"at_ms": at, "databases": d})))
		.collect();
	let project_reports: BTreeMap<String, serde_json::Value> = member
		.projects
		.lock()
		.await
		.iter()
		.map(|(h, (at, p))| (h.clone(), json!({"at_ms": at, "projects": p})))
		.collect();
	Json(json!({
		"id": member.config.id,
		"version": env!("CARGO_PKG_VERSION"),
		// What this build decides that an older one does not, for an operator's preflight to read (the
		// version is the crate's, which no change bumps): `draining` is packing's make-before-break.
		"features": ["draining"],
		"now_ms": now,
		"started_ms": member.started_ms,
		"state": format!("{:?}", metrics.state),
		"term": metrics.current_term,
		"leader": metrics.current_leader,
		"last_log_index": metrics.last_log_index,
		"last_applied": metrics.last_applied,
		"replication": replication,
		"members": member.config.members,
		"membership": membership_now(&member),
		"about": member.config.about,
		"has_spec": state.spec.is_some(),
		"spec": state.spec,
		"decision": state.decision.as_ref().map(|d| json!({"at_ms": d.at_ms, "by": d.by, "notes": d.outcome.notes, "placement": d.outcome.placement, "database": d.database, "projects": d.projects})),
		"history": recent.iter().map(|d| json!({"at_ms": d.at_ms, "by": d.by, "notes": d.outcome.notes})).collect::<Vec<_>>(),
		"reports": reports,
		"copies": copies,
		"database": database,
		"database_reports": database_reports,
		"project_reports": project_reports,
		"events": member.events.recent(60),
	}))
	.into_response()
}

async fn history(State(member): State<Member>, headers: HeaderMap) -> Response {
	if !may_read(&member, &headers) {
		return refused();
	}
	Json(member.state.state().await.history).into_response()
}

async fn report(
	State(member): State<Member>,
	Path(host): Path<String>,
	headers: HeaderMap,
	Json(copies): Json<Vec<CopyFacts>>,
) -> Response {
	if !is_agent_of(&member, &headers, &host).await {
		return refused();
	}
	let reports_only = headers
		.get(MODE_HEADER)
		.and_then(|v| v.to_str().ok())
		.is_some_and(|mode| mode.trim() == "report");
	pass_on(&member, &host, "", reports_only, &copies);
	take_copies(&member, host, copies, reports_only).await
}

/// A report another member received from an agent (`relay_reports`): taken as if the agent had
/// sent it here, and never passed on again.
async fn relay(
	State(member): State<Member>,
	Path(host): Path<String>,
	headers: HeaderMap,
	Json(copies): Json<Vec<CopyFacts>>,
) -> Response {
	if !is_member(&member, &headers) || !is_known_agent(&member, &host).await {
		return refused();
	}
	let reports_only = headers
		.get(MODE_HEADER)
		.and_then(|v| v.to_str().ok())
		.is_some_and(|mode| mode.trim() == "report");
	take_copies(&member, host, copies, reports_only).await
}

async fn relay_database(
	State(member): State<Member>,
	Path(host): Path<String>,
	headers: HeaderMap,
	Json(databases): Json<Vec<DatabaseReport>>,
) -> Response {
	if !is_member(&member, &headers) || !is_known_agent(&member, &host).await {
		return refused();
	}
	member
		.databases
		.lock()
		.await
		.insert(host, (super::decider::now_ms(), databases));
	StatusCode::NO_CONTENT.into_response()
}

async fn relay_projects(
	State(member): State<Member>,
	Path(host): Path<String>,
	headers: HeaderMap,
	Json(projects): Json<Vec<ProjectReport>>,
) -> Response {
	if !is_member(&member, &headers) || !is_known_agent(&member, &host).await {
		return refused();
	}
	member
		.projects
		.lock()
		.await
		.insert(host, (super::decider::now_ms(), projects));
	StatusCode::NO_CONTENT.into_response()
}

/// Sends an agent's report on to the other members, when this member relays (`relay_reports`):
/// in the background, each at most once, a member that does not answer simply not hearing it.
/// `kind` is the path after `/relay/{host}`: empty, `/database` or `/projects`.
fn pass_on<T: serde::Serialize>(
	member: &Member,
	host: &str,
	kind: &str,
	reports_only: bool,
	body: &T,
) {
	if !member.config.relay_reports {
		return;
	}
	let Ok(text) = serde_json::to_string(body) else {
		return;
	};
	for (id, address) in &member.config.members {
		if *id == member.config.id {
			continue;
		}
		let url = format!("{address}/relay/{host}{kind}");
		let (client, token, text) = (
			member.relay.clone(),
			member.config.member_token.clone(),
			text.clone(),
		);
		tokio::spawn(async move {
			let _ = client
				.post(url)
				.bearer_auth(token)
				.header("content-type", "application/json")
				.header(MODE_HEADER, if reports_only { "report" } else { "apply" })
				.body(text)
				.timeout(std::time::Duration::from_secs(3))
				.send()
				.await;
		});
	}
}

async fn take_copies(
	member: &Member,
	host: String,
	copies: Vec<CopyFacts>,
	reports_only: bool,
) -> Response {
	{
		let mut report_only = member.report_only.lock().await;
		if reports_only {
			report_only.insert(host.clone());
		} else {
			report_only.remove(&host);
		}
	}
	let previous = member
		.reports
		.lock()
		.await
		.insert(host.clone(), (super::decider::now_ms(), copies));
	if previous.is_none() {
		member
			.events
			.note("host", format!("first report from {host}"));
	} else if member.silent.lock().await.remove(&host) {
		member.events.note("host", format!("{host} reports again"));
	}
	StatusCode::NO_CONTENT.into_response()
}

/// The roles of the guarded database copies on the host, as its agent read them from their guards'
/// status files (`guard.ts`). A separate call from `/report/{host}`, whose body stays the bare list
/// of copies a member on an older build takes: an older member answers this one 404 and the agent
/// carries on. An empty list is a host whose guards have no fresh word: it replaces the last one.
async fn report_database(
	State(member): State<Member>,
	Path(host): Path<String>,
	headers: HeaderMap,
	Json(databases): Json<Vec<DatabaseReport>>,
) -> Response {
	if !is_agent_of(&member, &headers, &host).await {
		return refused();
	}
	pass_on(&member, &host, "/database", false, &databases);
	member
		.databases
		.lock()
		.await
		.insert(host, (super::decider::now_ms(), databases));
	StatusCode::NO_CONTENT.into_response()
}

/// Every copy of a project's database on the host, guarded or not, with its role: guarded ones as
/// their guards say (as `/report/{host}/database`), the others as their standalone entries do. What
/// the per-project primaries in `/table` are chosen from. Its own call, so an older member answers
/// it 404 and the agent carries on; an empty list is a host with no project, and replaces the last.
async fn report_projects(
	State(member): State<Member>,
	Path(host): Path<String>,
	headers: HeaderMap,
	Json(projects): Json<Vec<ProjectReport>>,
) -> Response {
	if !is_agent_of(&member, &headers, &host).await {
		return refused();
	}
	pass_on(&member, &host, "/projects", false, &projects);
	member
		.projects
		.lock()
		.await
		.insert(host, (super::decider::now_ms(), projects));
	StatusCode::NO_CONTENT.into_response()
}

/// What the host's agent should run: its `hostServices` from the last decision, from this member's
/// own copy, so any member answers it.
async fn desired(
	State(member): State<Member>,
	Path(host): Path<String>,
	headers: HeaderMap,
) -> Response {
	if !is_agent_of(&member, &headers, &host).await {
		return refused();
	}
	let state = member.state.state().await;
	let (Some(spec), Some(decision)) = (state.spec, state.decision) else {
		return (
			StatusCode::SERVICE_UNAVAILABLE,
			Json(json!({"error": "nothing decided yet"})),
		)
			.into_response();
	};
	// Decided while this host was silent: its share there is what the leader could place WITHOUT it,
	// not what it should run. An agent coming back would otherwise take that as it starts and remove
	// its own copies until the next decision put them back. Its agent keeps its file on a refusal
	// (MT2), and the decision after its first report names it again.
	if !decision.outcome.heard.is_empty() && !decision.outcome.heard.contains(&host) {
		return (
			StatusCode::SERVICE_UNAVAILABLE,
			Json(json!({"error": "the last decision did not hear this host: keep what it runs", "decided_at_ms": decision.at_ms})),
		)
			.into_response();
	}
	// The names Metis manages, so the agent also removes one that was placed elsewhere.
	let manages: Vec<&str> = spec
		.services
		.iter()
		.filter(|s| s.run.is_some())
		.map(|s| s.name.as_str())
		.collect();
	// Every copy the decision places on this host, of every service, routed or not (the table has no
	// route for an unrouted one, `lepis`): what a fleet home reports it runs in its own pods, so a
	// draining copy host can let its copy go (packing's drain). Only this host's.
	let placed: Vec<&Assignment> = decision
		.outcome
		.placement
		.iter()
		.filter(|a| a.host == host)
		.collect();
	match host_services(&spec, &decision.outcome.placement, &host) {
		Ok(entries) => Json(json!({
			"decided_at_ms": decision.at_ms,
			"manages": manages,
			"hostServices": entries,
			"placed": placed,
		}))
		.into_response(),
		Err(e) => (StatusCode::CONFLICT, Json(json!({"error": e}))).into_response(),
	}
}

async fn table(State(member): State<Member>, headers: HeaderMap) -> Response {
	if !is_any_agent(&member, &headers).await && !is_operator(&member, &headers) {
		return refused();
	}
	match member.state.state().await.decision {
		Some(decision) => Json(super::push::table_body(&member.config, &decision)).into_response(),
		None => (
			StatusCode::SERVICE_UNAVAILABLE,
			Json(json!({"error": "nothing decided yet"})),
		)
			.into_response(),
	}
}
