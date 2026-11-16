# snout-metis

Placement and routing for the stack's services. Metis decides which copies of which services
(auth, rest, storage, realtime, functions, psyche, a database router) run on which hosts, and keeps
the routing table every proxy balances calls from. It is never in the path of a call and never
touches the database.

- [How it works](#how-it-works)
- [Design rules](#design-rules)
- [Running it](#running-it)
- [A member's configuration](#a-members-configuration)
- [The spec](#the-spec)
- [HTTP API](#http-api)
- [Operations](#operations)
- [Threat model](#threat-model)
- [Working on it](#working-on-it)

## How it works

Three pieces, each replaceable:

- **The pure core** (`src/decide.rs`, `src/compile.rs`, `src/database.rs`): a spec (what should
  run) and the facts (what the agents report) in; a placement, a routing table and notes on what
  moved out. No I/O and no clock: the time is a fact, so the same input always gives the same
  decision and a logged one can be replayed.
- **The members** (`src/member/`): three (or five) processes on hosts in different failure domains
  that agree through Raft ([openraft](https://github.com/databendlabs/openraft)) on the spec and on
  each decision. The leader decides every `tick_ms`; every member serves the result.
- **The agents and routers** around it: each host's agent reports what runs there and reads what it
  should run; each router (or proxy) is pushed the table and balances calls from it.

The database primary's region, which the table prefers, is a fact the agents report: an agent on a
host with a guarded database copy posts its guard's role and epoch to `/report/{host}/database`,
and the leader takes the region of the copy that freshly says `primary` with the highest epoch,
keeps the last one while nobody does (a failover in progress), and falls back to the member
config's `database_primary_region` only with no report at all. The spec's optional
`database_cluster` names the guard cluster that counts; without it, the one cluster reported is
taken. Each project's own primary, guarded or not, comes the same way from
`/report/{host}/projects`.

## Design rules

The code cites these by number.

- **MT1: Metis places and routes; it never touches the database.** When the database moves, Metis
  follows it (the table prefers copies near the new primary); it never moves it.
- **MT2: Metis is never in the path of a call.** Routers keep the last table and agents the last
  desired state, so with every member down calls still flow and copies still restart; only changes
  wait.
- **MT3: The agent first, Metis second.** The agent restarts what dies on its host. Metis acts only
  on what the agent cannot fix: a host gone, or a copy the agent gave up on. It never touches a
  container; it changes what agents are told to run.
- **MT4: The router balances, per call, from the table.** Healthy copies only; same host, then the
  same region (or the database primary's), then least busy; split by version shares.
- **MT5: The spec is a file; secrets are never in it,** only the names of files each host holds.
  An operator applies it (`/spec`), or the leader reads it from a URL (`spec_from`).
- **MT6: What Metis decided is a log.** Every decision with the facts it used and its outcome;
  replaying the facts through the core gives the same decision (`snout-metis replay`).
- **MT7: The decision is a pure function,** `decide(spec, facts)`, with table-driven tests for every
  case: a copy dies or gives up, a host dies, load rises, a version rolls, a dedicated host is full.
- **MT8: An odd number of members, at least three, one per host,** each with its own copy of the
  state. Two cannot tell "the other is dead" from "the link is down"; three can.
- **MT11: Every automatic step is told.** A decision that moved something carries notes saying what
  and why, in the log and the member's events.
- **MT12: Metis holds no cloud credentials.** It never creates or stops a machine. Adding a host is
  an operator's step; Metis places onto the hosts the spec names.
- **MT14: Spreading and splitting are separate dials.** Metis spreads a project's services over
  hosts; splitting its data over database nodes is the database layer's. Neither turns the other
  on. A spreading service lives apart from its database, off the database's host where another has
  room, and reaches the database through a router that follows the primary.

## Running it

One binary, `snout-metis`:

    snout-metis plan   --spec <spec.json> --facts <facts.json>
    snout-metis merge  --spec <spec.json> --facts <facts.json> --host <name> --current <file>
    snout-metis replay --log <decisions.jsonl>
    snout-metis member --config <member.json>

- `plan` prints the decision as JSON: notes, placement, table, and each host's `hostServices`. It
  reaches no host and changes nothing.
- `merge` prints the host's desired-state file with Metis's entries in place (stdout) and the
  change as JSON (stderr). Every key but `hostServices`, and every entry of a service the spec does
  not name, is kept exactly.
- `replay` runs every logged decision's facts through the core again and exits 1 when one differs.
- `member` runs a member until SIGTERM or Ctrl-C.

Exit codes: 0 done, 1 refused (a spec that does not check, a replay that differs), 2 usage.

To start a cluster: write each member's config (below), start every member, then `POST /init` on
one of them with the operator token. Apply a spec with `POST /spec`.

## A member's configuration

A JSON file on the member's host, never in a repository, since it holds the tokens. Every token is
at least 32 characters, or the member refuses to start; so does an even number of members or a
member missing from its own list.

| Key | Default | Secret | What it is |
| --- | --- | --- | --- |
| `id` | required | | This member's id, a key of `members`. |
| `listen` | required | | Where members and operators reach it, `10.0.0.1:7480`: a private address. |
| `data_dir` | required | | Raft's log, the state and the decision log. |
| `members` | required | | Every member by id, as the others reach it: `http://10.0.0.1:7480`. |
| `member_token` | required | yes | What members present to each other. |
| `operator_token` | required | yes | What an operator presents (`/init`, `/spec`, `/membership`). |
| `observer_token` | none | yes | Reads `/status`, `/log` and `/membership` and nothing else. |
| `agent_tokens` | required | yes | By host name: an agent reports for, and reads, its own host only. |
| `about` | `[]` | | Where each host is (cloud, region, addresses), for a watcher's picture. Nothing decides from it. |
| `routers_file` | none | | A file of `{"routers": [{name, region, url, cert_sha256}]}`, read again before each push. |
| `router_tokens` | `{}` | yes | By region: what this member presents to that region's routers. |
| `routes_from` | none | yes (its token) | `{url, token}`: what routers admit and route by, pushed with the table. |
| `spec_from` | none | yes (its token) | `{url, token, every_secs: 30, placement_url}`: the leader reads the spec there instead of an operator applying it, and posts each placement to `placement_url`. |
| `agent_tls` | none | key file | `{listen, cert, key}`: a second listener, over TLS, for agents alone (`/report`, `/desired`, `/table`). |
| `relay_reports` | `false` | | Pass each agent report to the other members, for agents that reach only some of them. |
| `database_primary_region` | none | | The region to prefer when no agent has reported a database role. |
| `tick_ms` | `5000` | | How often the leader decides. |
| `heartbeat_ms` | `500` | | Raft's heartbeat. |
| `election_min_ms`, `election_max_ms` | `2000`, `4000` | | Raft's election timeouts, roomy enough for members across regions. |

`spec_from` and `routes_from` must be `https://`, or plain HTTP to this machine only.

## The spec

What should run: `host_timeout_secs`, the `hosts` (name, region, fault domain, capacity, and
whether each serves, drains, is frozen or dedicated), the `services` (environment, copies,
versions with their shares, the resources per copy, a placement policy, where they may go, and
optionally the `run` block an agent runs them from), and the `agents` admitted by a token's sha256.
`Spec::check` refuses the whole spec rather than part of it: a service with no version share, more
copies than `MAX_INSTANCES`, a placement no host can take, a secret where only a file name may go.
The types and every rule are in `src/spec.rs`.

Four things taken from Kubernetes, each off unless the spec names it (`src/orchestration_tests.rs`
holds their cases):

- **Requests, limits and QoS.** `per_copy` is what a copy reserves (the request; placement packs by
  it), `limit` the most it may use. Together they class it: best effort (reserves nothing),
  burstable, guaranteed (limit = request). With `pressure_percent`, a host whose running copies use
  more than that share of its capacity (the agents' `load`) has copies moved off it: best effort
  first, then burstable by how far past its request it runs, the lowest priority first, never a
  guaranteed or one-clock copy, and nothing new placed there. Make before break, as a drain.
- **Priority and preemption.** A service's `priority` (0 to `MAX_PRIORITY`) is the order services
  keep and take room in. When no host has room for a copy of a priority above 0, it takes the room
  of the fewest, lowest-priority copies on one host (strictly lower only); they are placed elsewhere
  in their own turn if anywhere has room, and the note says who gave way to whom.
- **The rebalancer.** With `rebalance` (`high_percent`, `low_percent`), once every placed copy runs
  steady and nothing else moves, one environment's copies on the most reserved host past
  `high_percent` move to the least reserved host under `low_percent` that can take them all (each
  service's own rules, one copy per host, a fault domain no fuller than the one left, never the
  environment's database host), make before break. One move at a time: the decision names it in
  `moves`, the leader carries it into the next decision's facts, and it ends when the old copies go.
- **Rollouts.** A service's `rollout`: `max_unavailable` hosts change version at once (1 by default,
  as before), `max_surge` copies of the new version go to hosts holding none of the service beside
  the old ones, and with `rollback`, an agent giving up on a copy of a version while an older one
  still runs rolls the service back to it (`rollbacks`, carried by the leader like `moves`) until the
  spec changes that version. Without a `rollout`, a version change is one host at a time, no surge,
  no rollback.

**Why a copy is not placed.** A decision that places a version short of its target lists it in
`shortfalls`: every host of the spec and the first reason it took no more copies (not reporting,
another customer's, not on its list, draining, frozen, under pressure, already holding its copy,
full...), with its free room, its capacity and, when the spec gives a host's `server`, the whole
machine. Its note ends with the same reasons, counted. A host's `server` and `owner` are only said,
never placed by: on the fleet, `capacity` is what the databases leave, so a full host is told apart
from a small one.

## HTTP API

All calls take `Authorization: Bearer <token>`; a wrong or missing one is `401 {"error":"not allowed"}`.

| Route | Who | What |
| --- | --- | --- |
| `POST /init` | operator | Makes the cluster from the configured members, once, on one of them. |
| `GET /membership` | operator, observer | The voters and learners. |
| `POST /membership` | operator | The whole set of voters wanted (odd, at least three); each new one is a learner until it has caught up. |
| `POST /spec` | operator | Applies a spec; `409` while the member reads it from `spec_from`. |
| `GET /status` | operator, observer | Role, term, leader, membership, the spec, the last decision, the database primary, each host's last report, recent events. |
| `GET /log` | operator, observer | Recent decisions with their facts and notes. |
| `POST /report/{host}` | that host's agent | The copies it runs. `x-snoutpod-metis-mode: report` marks an agent that only reports. |
| `POST /report/{host}/database` | that host's agent | The roles of its guarded database copies. |
| `POST /report/{host}/projects` | that host's agent | Every copy of a project's database there, with its role. |
| `GET /desired/{host}` | that host's agent | What it should run, from the last decision; any member answers. |
| `GET /table` | any agent, operator | The routing table. |
| `POST /relay/...` | member | A report another member received (`relay_reports`). |
| `POST /raft/*` | member | Raft between members. |

## Operations

- **Health:** `GET /status` on each member: one leader, the others following at the same term, and a
  recent report from every host that should be up.
- **Logs:** each member writes its events to stderr; `/status` carries the recent ones. The
  decisions are `decisions.jsonl` in `data_dir` (the previous file kept as `decisions.1.jsonl`), and
  `snout-metis replay` checks them.
- **Upgrading:** one member at a time, waiting for it to rejoin at the leader's term before the
  next. A newer member reads what an older one wrote; fields added later have defaults.
- **Replacing a member:** start the new one under a new id, `POST /membership` with the set that
  includes it and leaves the old one out, then stop the old one.
- **Losing members:** with a majority up, nothing changes for anyone. Below a majority no new
  decision is made, and routers and agents go on from the last table and desired state (MT2).
- **Rotating a token:** change it in every file that holds it and restart the members one at a time.

## Threat model

**What it trusts.** The operator token, the member token, and each agent's own token. The spec,
whether applied or read from `spec_from`. The routers' pinned certificates.

**What it holds.** Tokens (in its config file, never in the spec or the log), the spec, the agents'
reports, and the decisions. No database credential, no service secret (a host's secrets are files
on that host, named and never carried), no cloud credential (MT12).

**What it faces.**
- **The members' listener** is meant for a private network. Every route checks a bearer token,
  compared in constant time; tokens under 32 characters are refused at start.
- **The agents' TLS listener** (`agent_tls`) can face the internet. It serves the agents' routes and
  nothing else, so no member, operator or Raft route is reachable there. A handshake that does not
  finish in 10 seconds is dropped.
- **An agent** may report only for its own host and read only its own desired state. An agent
  admitted by the spec is matched by its token's sha256.
- **Routers** are reached over TLS with their certificate pinned by sha256 (no CA, no name), and
  never call a member.

**What a compromise gets.**
- **An agent token:** false reports for that one host. The worst is that host taken for lost or
  full, and its copies placed elsewhere; another host's facts are out of reach.
- **The observer token:** a read of the status and the log, which carry host names, regions and
  addresses but no secret.
- **The operator or member token:** control of placement and routing: any spec, so any routing
  table. Still no secret and no database: a service's secrets stay on its hosts, and Metis cannot
  start a machine or reach a database.
- **A member's host:** all of the above, and its Raft log.

**Untrusted input is fuzzed:** `metis_decide` (a spec and facts through `check`, `decide` and the
compile step) and `metis_report` (each report an agent posts, into the facts and the primaries).
`unsafe` is forbidden.

## Working on it

Everything runs in the stack's dev container; Docker is the only requirement.

    bash scripts/dev.sh cargo test -p snout-metis    # the core's table-driven tests and three-member clusters
    bash scripts/dev.sh cargo deny check             # licences, advisories and sources of every dependency
    bash scripts/fuzz.sh 60 metis_decide metis_report

## Security

Report vulnerabilities as `SECURITY.md` says. The threat model above is what a report is weighed
against.

## License

Apache-2.0 (`LICENSE`).
