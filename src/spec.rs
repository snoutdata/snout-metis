//! What should run (the [`Spec`], a file in the repo for our control plane, MT5) and what is
//! running (the [`Facts`] the agents report, MT3).

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// The desired environments: the hosts there are and the services to place on them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spec {
	/// Seconds without a heartbeat after which a host is taken as gone, and its copies placed
	/// elsewhere. Long enough that a restarting agent is not a lost host.
	pub host_timeout_secs: u64,
	pub hosts: Vec<HostSpec>,
	pub services: Vec<ServiceSpec>,
	/// The database cluster whose primary the table prefers copies near (MT4), by its guard's
	/// cluster name (`guard.cluster` in the agents' standalone files). Optional: without it, the one
	/// cluster the agents report is taken, and with two reported, none is (`database.rs`).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub database_cluster: Option<String>,
	/// Agents the members admit by a token the control plane issued, beside the ones in their own
	/// files (`agent_tokens`): a fleet copy host's, given at its enrolment. The control plane keeps only each token's sha256, and so does this.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub agents: Vec<SpecAgent>,
	/// Eviction under pressure (QoS): a host whose running copies USE more than this percent of its
	/// capacity (CPU or memory, as the agents measure it) has copies moved off it, best-effort ones
	/// first, then burstable ones by how far past their request they run, the lowest priority first
	/// within a class, never a guaranteed one ([`Qos`]). Make before break, as a drain. None: off.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub pressure_percent: Option<u32>,
	/// The rebalancer (a descheduler): with nothing on its way up, an environment's copies on the most
	/// reserved host past `high_percent` move to the least reserved one under `low_percent`, one move at
	/// a time, make before break. None: off, and a running copy only moves when it must.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub rebalance: Option<Rebalance>,
}

/// The rebalancer's band, in percent of a host's capacity its placed copies reserve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rebalance {
	#[serde(default = "default_high_percent")]
	pub high_percent: u32,
	#[serde(default = "default_low_percent")]
	pub low_percent: u32,
}

fn default_high_percent() -> u32 {
	80
}

fn default_low_percent() -> u32 {
	50
}

/// How a copy is treated when its host runs short, from what it reserves (`per_copy`, the request)
/// and the most it may use (`limit`), as Kubernetes classes pods.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Qos {
	/// Reserves nothing: the first moved off a host under pressure.
	BestEffort,
	/// Reserves something, and may use more (no limit, or a limit above it).
	Burstable,
	/// Its limit is what it reserves: never moved for pressure.
	Guaranteed,
}

/// How a version change rolls through a service's copies. None is one host at a time, no extra
/// copy, no rollback: what every spec before this field had.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rollout {
	/// Copies of the new version placed on hosts that hold none of the service, beside the old ones,
	/// before an old one is let go.
	#[serde(default)]
	pub max_surge: u32,
	/// Hosts whose old copy may be let go in one decision, while nothing placed is on its way up.
	#[serde(default = "one")]
	pub max_unavailable: u32,
	/// When an agent gives up on a copy of a new version while another version still runs, the
	/// service rolls back to that one until the spec changes the version ([`Rollback`]).
	#[serde(default, skip_serializing_if = "is_false")]
	pub rollback: bool,
}

/// A rollout that was rolled back: `from` gave up, its share goes to `to`. Carried by the members
/// from one decision to the next (`Facts::rollbacks`) while the spec still names `from`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Rollback {
	pub from: String,
	pub to: String,
}

/// A rebalancer's move in flight: every copy of `environment` on `from` goes to `to`, and the one on
/// `from` stays until its replacement runs steady. Carried by the members from one decision to the
/// next (`Facts::moves`) until it is done or no longer possible.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Move {
	pub environment: String,
	pub from: String,
	pub to: String,
}

/// One agent the spec admits: its host, the sha256 of its token (lowercase hex) and the public
/// address it calls from (for the members' firewall, which the spec's reader keeps in step).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecAgent {
	pub host: String,
	pub token_sha256: String,
	pub address: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSpec {
	pub name: String,
	pub region: String,
	pub fault_domain: String,
	/// False for a host that only holds a Metis member (a control plane's witness host): nothing is
	/// ever placed on it.
	#[serde(default = "serves_by_default")]
	pub serves: bool,
	/// Being emptied so it can retire (packing): nothing new is placed on it, and a copy that
	/// runs there stays placed and routed until the copies placed elsewhere for it run steady
	/// ([`crate::decide::STEADY_MS`]), so a move never takes a service below its copies. Comes with
	/// `serves` false.
	#[serde(default, skip_serializing_if = "is_false")]
	pub draining: bool,
	/// Takes the copies of the environments whose database it holds (`Facts::database_hosts`) even
	/// while `serves` is false: a closed home on the fleet's Metis keeps its own spread projects' share
	///. Nothing else is placed on it, so a host closed by hand or for maintenance takes no
	/// other project's copy. A draining host takes nothing.
	#[serde(default, skip_serializing_if = "is_false")]
	pub serves_home: bool,
	/// Full, and must not change: what runs there stays placed and routed while it is heard,
	/// nothing new is placed there and nothing is moved off it. Comes with `serves` and `serves_home`
	/// false, so a member before this field reads it as a host that serves nothing.
	#[serde(default, skip_serializing_if = "is_false")]
	pub frozen: bool,
	/// Held because its drain was withdrawn after it began (moves in flight finish,
	/// nothing moves back). Comes with `frozen`: nothing new is placed there and nothing more is moved
	/// off it, but a copy whose replacement already runs or starts on another host is let go once that
	/// one runs steady, as a draining host's is. A member before this field holds it as frozen.
	#[serde(default, skip_serializing_if = "is_false")]
	pub finishing: bool,
	/// The one customer whose services may run here (a dedicated host). `None` is a shared host,
	/// which a dedicated service never uses either.
	#[serde(default)]
	pub dedicated_to: Option<String>,
	/// What the host has for copies, after what it keeps for itself.
	pub capacity: Resources,
	/// The whole server, before what the host and its databases keep: only said (why a host is
	/// full, `decide.rs`'s shortfalls), never placed by. None from a spec that does not say.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub server: Option<Resources>,
	/// The customer whose server it is when a list keeps it for them rather than `dedicated_to` (the
	/// fleet's pools, sql/337): only said, never placed by.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub owner: Option<String>,
	/// Which measure `capacity` came from when it is not the reservations: `measured` for a server of
	/// a pool the control plane measures (sql/396, a recommender beside Metis as Kubernetes' VPA).
	/// Only said (a shortfall names it), never placed by: placing reads `capacity` alone.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub room: Option<String>,
}

fn serves_by_default() -> bool {
	true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Resources {
	pub millicpu: u32,
	pub memory_mb: u32,
}

impl Resources {
	pub fn fits(self, need: Resources) -> bool {
		need.millicpu <= self.millicpu && need.memory_mb <= self.memory_mb
	}

	pub fn minus(self, used: Resources) -> Resources {
		Resources {
			millicpu: self.millicpu.saturating_sub(used.millicpu),
			memory_mb: self.memory_mb.saturating_sub(used.memory_mb),
		}
	}

	pub fn plus(self, freed: Resources) -> Resources {
		Resources {
			millicpu: self.millicpu.saturating_add(freed.millicpu),
			memory_mb: self.memory_mb.saturating_add(freed.memory_mb),
		}
	}
}

/// One service of one environment: `auth` of the control plane, `functions` of project X.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceSpec {
	pub name: String,
	/// The environment it belongs to: a project's ref, or `control-plane`.
	pub environment: String,
	/// How many copies, in all, over every host.
	pub copies: u32,
	/// The versions to run and their relative shares of the copies (and of the calls): `0.2.1` at
	/// 9 and `0.2.2` at 1 is a new version on a tenth.
	pub versions: Vec<VersionShare>,
	pub per_copy: Resources,
	/// Exactly one copy anywhere, kept where it is while its host lives: the sweeps, one clock.
	#[serde(default)]
	pub one_clock: bool,
	/// Run only on hosts dedicated to this customer.
	#[serde(default)]
	pub dedicated_to: Option<String>,
	/// How a host's agent runs a copy, for the services placed through the agent's
	/// `hostServices` (`compile.rs`). `None` for a service Metis places but does not yet hand to an
	/// agent.
	#[serde(default)]
	pub run: Option<RunSpec>,
	/// Where its copies may go, beside `serves` and `dedicated_to`: a control plane's auth, rest
	/// and psyche only on the hosts that reach the database's copies. Empty is anywhere.
	#[serde(default, skip_serializing_if = "Placement::is_anywhere")]
	pub placement: Placement,
	/// Whether the proxies may send calls to its copies. False keeps them placed and running and out
	/// of the routing table, so copies can be started and checked before any call reaches them, and
	/// taken out of the path again without being stopped.
	#[serde(default = "routed_by_default", skip_serializing_if = "is_true")]
	pub routed: bool,
	/// Copies by load, between `copies` and `max_copies` ([`Scale`]). None is always `copies`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub scale: Option<Scale>,
	/// How its copies share hosts: config the control plane
	/// sends per plan, never code. Absent is [`Policy::SPREAD`], today's rule and the only one the
	/// control plane's own environment may have ([`Spec::check`]).
	#[serde(default, skip_serializing_if = "Policy::is_spread")]
	pub policy: Policy,
	/// The most one copy may use; `per_copy` is what it reserves (the request). Together they give its
	/// [`Qos`]. None: it may use more than it reserves (burstable), or reserves nothing (best effort).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub limit: Option<Resources>,
	/// Higher keeps and takes room first, when room is short. It never takes a running copy's room:
	/// no copy is evicted for a higher priority (Joel, 2026-11-17: a free project is lower, never
	/// dispensable). 0 is the default.
	#[serde(default, skip_serializing_if = "is_zero_u32")]
	pub priority: u32,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub rollout: Option<Rollout>,
}

/// The highest priority a spec may give, a bound in the binary.
pub const MAX_PRIORITY: u32 = 1_000_000;

fn is_zero_u32(n: &u32) -> bool {
	*n == 0
}

/// The environment of the control plane's own services, whose copies ARE its failover: always spread.
pub const CONTROL_PLANE: &str = "control-plane";

/// The most copies of a customer's service, and the most of them one host may hold: bounds in the
/// binary, so no config can multiply instances. The plans give 2 and 3.
pub const MAX_INSTANCES: u32 = 3;

/// How a service's copies share hosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
	pub mode: PlaceMode,
	/// The most copies one host may hold: 1 for `spread`, up to [`MAX_INSTANCES`] for `pack`.
	#[serde(default = "one")]
	pub max_per_host: u32,
}

/// `spread`: one copy per host, off the database's own host where another has room (MT14). `pack`:
/// the database's host first, up to `max_per_host` copies on one host, which the control plane runs
/// as one bigger services pod there. Any other word refuses the whole spec read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlaceMode {
	Spread,
	Pack,
}

impl Policy {
	pub const SPREAD: Policy = Policy {
		mode: PlaceMode::Spread,
		max_per_host: 1,
	};

	fn is_spread(&self) -> bool {
		*self == Self::SPREAD
	}
}

impl Default for Policy {
	fn default() -> Self {
		Self::SPREAD
	}
}

fn one() -> u32 {
	1
}

/// Scaling on load: one copy more when the running copies use more than `up_percent` of what they
/// reserve (`per_copy`, the busier of CPU and memory), one fewer when one fewer would still use less
/// than `down_percent`, never below `copies` nor above `max_copies`. `down_percent` below
/// `up_percent` is what keeps it from flapping: a step either way lands inside the band.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scale {
	pub max_copies: u32,
	#[serde(default = "default_up_percent")]
	pub up_percent: u32,
	#[serde(default = "default_down_percent")]
	pub down_percent: u32,
}

fn default_up_percent() -> u32 {
	80
}

fn default_down_percent() -> u32 {
	40
}

fn routed_by_default() -> bool {
	true
}

fn is_true(value: &bool) -> bool {
	*value
}

fn is_false(value: &bool) -> bool {
	!*value
}

/// A service's own limit on where its copies run. A host must pass both lists that are not empty.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Placement {
	/// Only these hosts, by name.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub hosts: Vec<String>,
	/// Only hosts in these regions.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub regions: Vec<String>,
}

impl Placement {
	pub fn is_anywhere(&self) -> bool {
		self.hosts.is_empty() && self.regions.is_empty()
	}

	pub fn allows(&self, host: &HostSpec) -> bool {
		(self.hosts.is_empty() || self.hosts.contains(&host.name))
			&& (self.regions.is_empty() || self.regions.contains(&host.region))
	}
}

/// One copy as the host agent runs it: its `HostService` entry, less the image tag, which is the placed version. Never a
/// secret: a host's secrets for a service are the host's own, never in the spec (MT5). What a copy
/// needs that is secret is NAMED here (`secret_env`, `files`, `database_url`, `env_from`) and read
/// by the agent on the host (`hostServiceCompose.ts`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RunSpec {
	/// The repository; the copy's image is `<image_repo>:<version>`.
	pub image_repo: String,
	pub port: u16,
	/// Where `port` is published on the host. 0: not published (the Lepis router, reached only by
	/// the services that share its network namespace).
	pub host_port: u16,
	#[serde(default)]
	pub health_path: Option<String>,
	pub memory_mb: u32,
	#[serde(default)]
	pub env: BTreeMap<String, String>,
	#[serde(default)]
	pub command: Vec<String>,
	/// Variables whose values are host secret files, by variable name: `LEPIS_SERVICE_PASSWORD` →
	/// `lepis-router-admin` is the file of that name in the host's service-secrets directory, read
	/// by the agent when it makes the container and handed over in its environment, never argv.
	#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
	pub secret_env: BTreeMap<String, String>,
	/// Files from the same directory, mounted read-only, by the path in the container.
	#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
	pub files: BTreeMap<String, String>,
	/// Another service of the same environment whose network namespace this one shares, as a pod's
	/// sidecars share the database's: auth, rest and psyche join `lepis-router`, so the router they
	/// reach on 127.0.0.1 is the one on their own host and is published nowhere. Placed together.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub joins: Option<String>,
	/// The environment of one of a project's pod services on the same host, as the host's entry
	/// for that project holds it (JWT secret, OAuth secrets and all): what makes a host copy of the
	/// control plane's auth the same auth as its pod sidecar without a secret in the spec.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub env_from: Option<EnvFrom>,
	/// The database URL the agent composes into `env`, with the role's password from a host secret
	/// file. The variable and role replace the pod sidecar's `dbUriEnv`/`dbUriRole`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub database_url: Option<DatabaseUrl>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvFrom {
	/// The project's ref.
	pub project: String,
	/// The name of the service in that project's `spec.services`.
	pub service: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatabaseUrl {
	/// The variable the image reads it from: `PGRST_DB_URI`.
	pub env: String,
	pub role: String,
	/// The host secret file holding the role's password: `role-snout_gate`.
	pub password_secret: String,
	/// Where the database is, from inside the container: the Lepis router, `127.0.0.1`.
	pub host: String,
	pub port: u16,
	pub database: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionShare {
	pub version: String,
	pub share: u32,
}

impl Spec {
	/// Refuses a spec that cannot mean one thing, before anything is decided from it.
	pub fn check(&self) -> Result<(), String> {
		let mut hosts = BTreeSet::new();
		for host in &self.hosts {
			if !hosts.insert(host.name.as_str()) {
				return Err(format!("host {} is listed twice", host.name));
			}
		}
		let mut agents = BTreeSet::new();
		for agent in &self.agents {
			if !agents.insert(agent.host.as_str()) {
				return Err(format!("agent {} is listed twice", agent.host));
			}
			if agent.token_sha256.len() != 64
				|| !agent
					.token_sha256
					.bytes()
					.all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
			{
				return Err(format!("agent {}: its token is not a sha256", agent.host));
			}
			if agent.address.parse::<std::net::Ipv4Addr>().is_err() {
				return Err(format!(
					"agent {}: {} is not an address",
					agent.host, agent.address
				));
			}
		}
		if let Some(percent) = self.pressure_percent
			&& (percent == 0 || percent > 100)
		{
			return Err(format!(
				"pressure_percent {percent} is a percent of a host's capacity, 1 to 100"
			));
		}
		if let Some(band) = self.rebalance
			&& (band.high_percent == 0
				|| band.high_percent > 100
				|| band.low_percent >= band.high_percent)
		{
			return Err(format!(
				"rebalance: low_percent {} must be below high_percent {}, which is 1 to 100",
				band.low_percent, band.high_percent
			));
		}
		let mut services = BTreeSet::new();
		for service in &self.services {
			let id = service.id();
			if !services.insert(id.clone()) {
				return Err(format!("service {id} is listed twice"));
			}
			if service.copies == 0 {
				return Err(format!("{id}: copies must be at least 1"));
			}
			if service.one_clock && service.copies != 1 {
				return Err(format!("{id}: a one-clock service has exactly one copy"));
			}
			if service.versions.iter().map(|v| v.share).sum::<u32>() == 0 {
				return Err(format!("{id}: no version has a share"));
			}
			let mut versions = BTreeSet::new();
			for version in &service.versions {
				if !versions.insert(version.version.as_str()) {
					return Err(format!("{id}: version {} is listed twice", version.version));
				}
			}
			if let Some(owner) = &service.dedicated_to
				&& !self
					.hosts
					.iter()
					.any(|h| h.dedicated_to.as_ref() == Some(owner))
			{
				return Err(format!("{id}: dedicated to {owner}, and no host is"));
			}
			if let Some(scale) = &service.scale {
				check_scale(service, scale)?;
			}
			check_policy(service)?;
			check_qos(service)?;
			self.check_placement(service)?;
			if let Some(run) = &service.run {
				self.check_run(service, run)?;
			}
		}
		Ok(())
	}

	fn check_placement(&self, service: &ServiceSpec) -> Result<(), String> {
		let id = service.id();
		let placement = &service.placement;
		for name in &placement.hosts {
			match self.hosts.iter().find(|h| &h.name == name) {
				None => return Err(format!("{id}: placed on {name}, which is not a host")),
				Some(host) if !host.serves && !host.serves_home => {
					return Err(format!("{id}: placed on {name}, which serves nothing"));
				}
				Some(_) => {}
			}
		}
		for region in &placement.regions {
			// A closed home (`serves_home`) may hold its own projects' copies there.
			if !self
				.hosts
				.iter()
				.any(|h| (h.serves || h.serves_home) && &h.region == region)
			{
				return Err(format!("{id}: placed in {region}, where no host serves"));
			}
		}
		let room = self
			.hosts
			.iter()
			.filter(|h| (h.serves || h.serves_home) && placement.allows(h))
			.count();
		if room == 0 {
			return Err(format!(
				"{id}: no host is in both its hosts and its regions"
			));
		}
		// One copy per host for a service the agents run (`compile.rs`), so more copies than hosts
		// could never all be placed.
		let most = service.scale.map_or(service.copies, |s| s.max_copies);
		if service.run.is_some() && most as usize > room {
			return Err(format!(
				"{id}: {most} copies, and only {room} hosts may run one",
			));
		}
		Ok(())
	}

	fn check_run(&self, service: &ServiceSpec, run: &RunSpec) -> Result<(), String> {
		let id = service.id();
		if run.host_port == 0 && run.health_path.is_some() {
			return Err(format!(
				"{id}: a health path needs a published port to be asked on"
			));
		}
		let mut names: BTreeSet<&str> = run.env.keys().map(String::as_str).collect();
		for (variable, file) in &run.secret_env {
			if !is_env_name(variable) {
				return Err(format!("{id}: {variable:?} is not a variable name"));
			}
			if !names.insert(variable) {
				return Err(format!("{id}: {variable} is set twice"));
			}
			if !is_secret_name(file) {
				return Err(format!("{id}: {file:?} is not a host secret file name"));
			}
		}
		for (path, file) in &run.files {
			if !path.starts_with('/')
				|| path.contains("..")
				|| path.contains(':')
				|| path.contains(',')
			{
				return Err(format!("{id}: {path:?} is not a path to mount a file at"));
			}
			if !is_secret_name(file) {
				return Err(format!("{id}: {file:?} is not a host secret file name"));
			}
		}
		if let Some(url) = &run.database_url {
			if !is_env_name(&url.env) || !names.insert(&url.env) {
				return Err(format!(
					"{id}: the database URL's variable {:?} is not a free variable name",
					url.env
				));
			}
			if !is_db_name(&url.role) || !is_db_name(&url.database) {
				return Err(format!(
					"{id}: the database URL names a role or database that is not one"
				));
			}
			if !is_secret_name(&url.password_secret) {
				return Err(format!(
					"{id}: {:?} is not a host secret file name",
					url.password_secret
				));
			}
			if url.port == 0
				|| url.host.is_empty()
				|| !url
					.host
					.chars()
					.all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
			{
				return Err(format!("{id}: the database URL's host or port is not one"));
			}
		}
		if let Some(from) = &run.env_from
			&& (from.project.is_empty()
				|| !from
					.project
					.chars()
					.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
				|| !is_service_name(&from.service))
		{
			return Err(format!("{id}: env_from names no project service"));
		}
		if let Some(owner) = &run.joins {
			let Some(joined) = self
				.services
				.iter()
				.find(|s| s.environment == service.environment && &s.name == owner)
			else {
				return Err(format!(
					"{id}: joins {owner}, which is not a service of {}",
					service.environment
				));
			};
			let Some(joined_run) = &joined.run else {
				return Err(format!("{id}: joins {owner}, which no agent runs"));
			};
			if joined.name == service.name || joined_run.joins.is_some() {
				return Err(format!("{id}: joins {owner}, which joins another itself"));
			}
			// Placed together: one copy of each on the same hosts, and a decision that cannot
			// split them (`compile.rs` refuses a host that has one without the other).
			if joined.copies != service.copies || joined.placement != service.placement {
				return Err(format!(
					"{id}: joins {owner}, so it needs the same copies and placement"
				));
			}
		}
		Ok(())
	}
}

fn is_env_name(name: &str) -> bool {
	let mut chars = name.chars();
	name.len() <= 64
		&& chars.next().is_some_and(|c| c.is_ascii_uppercase())
		&& chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// A file in the host's service-secrets directory: a plain name, never a path.
fn is_secret_name(name: &str) -> bool {
	let mut chars = name.chars();
	name.len() <= 64
		&& chars
			.next()
			.is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
		&& chars
			.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

fn is_db_name(name: &str) -> bool {
	let mut chars = name.chars();
	name.len() <= 63
		&& chars.next().is_some_and(|c| c.is_ascii_lowercase())
		&& chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// A placement policy the members take: `spread` holds one copy per host; `pack` at most
/// [`MAX_INSTANCES`]; the control plane's own services spread, always, whatever a config says; and a
/// customer's service never has more than [`MAX_INSTANCES`] copies.
fn check_policy(service: &ServiceSpec) -> Result<(), String> {
	let id = service.id();
	let policy = service.policy;
	if policy.max_per_host == 0 || policy.max_per_host > MAX_INSTANCES {
		return Err(format!(
			"{id}: max_per_host {} is outside 1 to {MAX_INSTANCES}",
			policy.max_per_host
		));
	}
	if policy.mode == PlaceMode::Spread && policy.max_per_host != 1 {
		return Err(format!(
			"{id}: spread holds one copy per host, not {}",
			policy.max_per_host
		));
	}
	if service.environment == CONTROL_PLANE {
		if policy != Policy::SPREAD {
			return Err(format!("{id}: the control plane's services always spread"));
		}
	} else if service.copies > MAX_INSTANCES {
		return Err(format!(
			"{id}: {} copies, more than the {MAX_INSTANCES} a project may have",
			service.copies
		));
	}
	Ok(())
}

fn check_scale(service: &ServiceSpec, scale: &Scale) -> Result<(), String> {
	let id = service.id();
	if service.one_clock {
		return Err(format!(
			"{id}: a one-clock service has exactly one copy, never scaled"
		));
	}
	if scale.max_copies < service.copies {
		return Err(format!(
			"{id}: max_copies {} is below copies {}",
			scale.max_copies, service.copies
		));
	}
	if scale.up_percent == 0 || scale.up_percent > 100 {
		return Err(format!(
			"{id}: up_percent is a percent of what a copy reserves, 1 to 100"
		));
	}
	if scale.down_percent >= scale.up_percent {
		return Err(format!(
			"{id}: down_percent {} must be below up_percent {}, or it scales back and forth",
			scale.down_percent, scale.up_percent
		));
	}
	if service.per_copy.millicpu == 0 && service.per_copy.memory_mb == 0 {
		return Err(format!(
			"{id}: scaled on load, and a copy reserves nothing to measure it against"
		));
	}
	Ok(())
}

/// A limit is at least what a copy reserves, a priority within its bound, and a rollout that can
/// move: at least one host at a time, a surge within what a service may have.
fn check_qos(service: &ServiceSpec) -> Result<(), String> {
	let id = service.id();
	if let Some(limit) = service.limit {
		if !limit.fits(service.per_copy) {
			return Err(format!(
				"{id}: its limit is below what a copy reserves (per_copy)"
			));
		}
		if let Some(run) = &service.run
			&& limit.memory_mb != run.memory_mb
		{
			return Err(format!(
				"{id}: its memory limit {} is not the {} MB its agent runs it with",
				limit.memory_mb, run.memory_mb
			));
		}
	}
	if service.priority > MAX_PRIORITY {
		return Err(format!(
			"{id}: priority {} is above {MAX_PRIORITY}",
			service.priority
		));
	}
	if let Some(rollout) = service.rollout {
		if rollout.max_unavailable == 0 {
			return Err(format!(
				"{id}: max_unavailable 0 would never let an old copy go"
			));
		}
		if rollout.max_surge > MAX_INSTANCES {
			return Err(format!(
				"{id}: max_surge {} is above {MAX_INSTANCES}",
				rollout.max_surge
			));
		}
		if service.one_clock && rollout.max_surge > 0 {
			return Err(format!(
				"{id}: a one-clock service never runs a second copy, so no surge"
			));
		}
	}
	Ok(())
}

/// The agent's `assertServiceName`.
fn is_service_name(name: &str) -> bool {
	let mut chars = name.chars();
	name.len() <= 31
		&& chars.next().is_some_and(|c| c.is_ascii_lowercase())
		&& chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

impl ServiceSpec {
	/// `environment/name`, the key a service is known by in notes and in the table.
	pub fn id(&self) -> String {
		format!("{}/{}", self.environment, self.name)
	}

	/// Its class when a host runs short ([`Qos`]).
	pub fn qos(&self) -> Qos {
		if self.per_copy == Resources::default() {
			Qos::BestEffort
		} else if self.limit == Some(self.per_copy) {
			Qos::Guaranteed
		} else {
			Qos::Burstable
		}
	}
}

/// What the agents last reported, and the time it is now.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Facts {
	pub now_ms: u64,
	pub hosts: Vec<HostFacts>,
	/// Where the database's primary is, so the table can prefer copies near it (MT4). Metis follows
	/// the database and never moves it (MT1).
	#[serde(default)]
	pub database_primary_region: Option<String>,
	/// Where each environment's own database primary is, by environment (a fleet project's ref): its
	/// copies are placed off that host where another has room, and the table prefers that host's
	/// region over `database_primary_region` (MT4, MT14). From the agents' project reports
	/// (`database.rs`, `project_primaries`); an environment without one uses the region above.
	#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
	pub database_hosts: BTreeMap<String, String>,
	/// The rebalancer's move in flight, from the last decision (`Outcome::moves`).
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub moves: Vec<Move>,
	/// Rollouts rolled back, by service id, from the last decision (`Outcome::rollbacks`).
	#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
	pub rollbacks: BTreeMap<String, Rollback>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostFacts {
	pub name: String,
	pub last_heartbeat_ms: u64,
	pub copies: Vec<CopyFacts>,
	/// Its agent only reports (`SNOUTPOD_METIS_MODE=report`) and never takes a decision: what runs
	/// there is kept as it is, and nothing is ever added or changed there (`decide.rs`). False from an
	/// agent that does not say, which is every older agent.
	#[serde(default, skip_serializing_if = "std::ops::Not::not")]
	pub report_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyFacts {
	pub environment: String,
	pub service: String,
	pub version: String,
	pub state: CopyState,
	/// What the copy uses now, as its agent measured it (CPU in millicores, memory in MB). None from
	/// an agent that does not measure: a service scaled on load holds its copies until every running
	/// one says (`decide.rs`, `wanted_copies`).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub load: Option<Resources>,
	/// Since when it has run and answered its health, as its agent saw it (Unix ms). None from an
	/// agent that does not say, or a copy not running: such a copy never counts as steady.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub running_since_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CopyState {
	/// Up and answering its health check: the table routes to it.
	Running,
	/// Being started or restarted by its agent: kept where it is, not routed to yet.
	Starting,
	/// The agent restarted it until its limit and stopped: Metis places it on another host.
	GaveUp,
}

#[cfg(test)]
mod tests {
	use super::*;

	fn with_agents(agents: serde_json::Value) -> Spec {
		serde_json::from_value(serde_json::json!({
			"host_timeout_secs": 60,
			"hosts": [],
			"services": [],
			"agents": agents,
		}))
		.expect("a spec")
	}

	#[test]
	fn the_specs_agents_are_checked_and_absent_by_default() {
		let ok = serde_json::json!({ "host": "pod-aaaaaaaa", "token_sha256": "a".repeat(64), "address": "203.0.113.4" });
		assert!(with_agents(serde_json::json!([ok])).check().is_ok());
		assert!(
			with_agents(serde_json::json!([ok, ok])).check().is_err(),
			"twice"
		);
		let short = serde_json::json!({ "host": "pod-aaaaaaaa", "token_sha256": "abc", "address": "203.0.113.4" });
		assert!(
			with_agents(serde_json::json!([short])).check().is_err(),
			"not a sha256"
		);
		let upper = serde_json::json!({ "host": "pod-aaaaaaaa", "token_sha256": "A".repeat(64), "address": "203.0.113.4" });
		assert!(
			with_agents(serde_json::json!([upper])).check().is_err(),
			"the control plane writes lowercase hex"
		);
		let nowhere = serde_json::json!({ "host": "pod-aaaaaaaa", "token_sha256": "a".repeat(64), "address": "a host" });
		assert!(
			with_agents(serde_json::json!([nowhere])).check().is_err(),
			"not an address"
		);
		// A spec from before 284 has no `agents`, and one written back has none either.
		let old: Spec = serde_json::from_value(
			serde_json::json!({ "host_timeout_secs": 60, "hosts": [], "services": [] }),
		)
		.expect("old");
		assert!(old.agents.is_empty());
		assert!(
			serde_json::to_value(&old)
				.expect("json")
				.get("agents")
				.is_none()
		);
	}

	fn with_service(
		environment: &str,
		copies: u32,
		policy: Option<serde_json::Value>,
	) -> Result<Spec, String> {
		let mut service = serde_json::json!({
			"name": "auth",
			"environment": environment,
			"copies": copies,
			"versions": [{ "version": "0.1", "share": 1 }],
			"per_copy": { "millicpu": 0, "memory_mb": 64 },
		});
		if let Some(policy) = policy {
			service["policy"] = policy;
		}
		let spec: Spec = serde_json::from_value(serde_json::json!({
			"host_timeout_secs": 60,
			"hosts": [{ "name": "h1", "region": "r", "fault_domain": "a", "serves": true, "capacity": { "millicpu": 0, "memory_mb": 1024 } }],
			"services": [service],
		}))
		.map_err(|e| e.to_string())?;
		spec.check().map(|()| spec)
	}

	#[test]
	fn a_placement_policy_is_config_the_members_check_before_taking_it() {
		// Absent is spread, today's rule, and is never written back.
		let old = with_service("x8catwwkz44dn", 2, None).expect("no policy");
		assert_eq!(old.services[0].policy, Policy::SPREAD);
		assert!(
			serde_json::to_value(&old).expect("json")["services"][0]
				.get("policy")
				.is_none()
		);
		// Pack, two to a host: a Pro project.
		let pro = with_service(
			"x8catwwkz44dn",
			2,
			Some(serde_json::json!({ "mode": "pack", "max_per_host": 2 })),
		)
		.expect("pack");
		assert_eq!(
			pro.services[0].policy,
			Policy {
				mode: PlaceMode::Pack,
				max_per_host: 2
			}
		);
		// An unknown mode, or an unknown field, refuses the whole read.
		assert!(
			with_service(
				"x8catwwkz44dn",
				2,
				Some(serde_json::json!({ "mode": "scatter" }))
			)
			.is_err()
		);
		assert!(
			with_service(
				"x8catwwkz44dn",
				2,
				Some(serde_json::json!({ "mode": "pack", "everywhere": true }))
			)
			.is_err()
		);
		// Bounds in the binary: 1 to 3 per host, spread is one per host, and no project above 3 copies.
		assert!(
			with_service(
				"x8catwwkz44dn",
				2,
				Some(serde_json::json!({ "mode": "pack", "max_per_host": 4 }))
			)
			.is_err()
		);
		assert!(
			with_service(
				"x8catwwkz44dn",
				2,
				Some(serde_json::json!({ "mode": "pack", "max_per_host": 0 }))
			)
			.is_err()
		);
		assert!(
			with_service(
				"x8catwwkz44dn",
				2,
				Some(serde_json::json!({ "mode": "spread", "max_per_host": 2 }))
			)
			.is_err()
		);
		assert!(with_service("x8catwwkz44dn", 4, None).is_err());
		// The control plane's own services spread, whatever a config says; their copies are its failover.
		assert!(with_service(CONTROL_PLANE, 3, None).is_ok());
		let err = with_service(
			CONTROL_PLANE,
			3,
			Some(serde_json::json!({ "mode": "pack", "max_per_host": 2 })),
		)
		.unwrap_err();
		assert!(err.contains("always spread"), "{err}");
	}
}
