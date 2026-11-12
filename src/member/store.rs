//! A member's durable state on its own disk (MT8): the vote, the log and a snapshot of the state
//! machine, in one directory.
//!
//! Small by design (a spec and a few hundred decisions), so it is plain files: `vote.json` and
//! `snapshot.json` are replaced whole (write, fsync, rename, fsync the directory), and the log is
//! one `log.jsonl`, appended and fsynced before the append is acknowledged, and rewritten whole on
//! a truncate or a purge. Nothing is acknowledged that is not on disk, which is the one rule Raft
//! asks of storage.

use super::types::{Node, NodeId, State, TypeConfig};
use openraft::storage::{LogFlushed, LogState, RaftLogStorage, RaftStateMachine, Snapshot};
use openraft::{
	AnyError, Entry, EntryPayload, LogId, RaftLogReader, RaftSnapshotBuilder, SnapshotMeta,
	StorageError, StorageIOError, StoredMembership, Vote,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::io::{Cursor, Write};
use std::ops::RangeBounds;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

type Error = StorageError<NodeId>;

fn io(e: impl std::error::Error + 'static) -> AnyError {
	AnyError::new(&e)
}

/// Write `bytes` to `path` so that either the old file or the whole new one survives a crash.
fn replace(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
	let tmp = path.with_extension("tmp");
	{
		let mut file = std::fs::File::create(&tmp)?;
		file.write_all(bytes)?;
		file.sync_all()?;
	}
	std::fs::rename(&tmp, path)?;
	if let Some(dir) = path.parent() {
		std::fs::File::open(dir)?.sync_all()?;
	}
	Ok(())
}

#[derive(Debug, Default)]
struct LogInner {
	dir: PathBuf,
	vote: Option<Vote<NodeId>>,
	committed: Option<LogId<NodeId>>,
	last_purged: Option<LogId<NodeId>>,
	entries: BTreeMap<u64, Entry<TypeConfig>>,
}

#[derive(Serialize, Deserialize, Default)]
struct LogMeta {
	last_purged: Option<LogId<NodeId>>,
	committed: Option<LogId<NodeId>>,
}

impl LogInner {
	fn log_path(&self) -> PathBuf {
		self.dir.join("log.jsonl")
	}

	fn write_meta(&self) -> std::io::Result<()> {
		let meta = LogMeta {
			last_purged: self.last_purged,
			committed: self.committed,
		};
		replace(
			&self.dir.join("log-meta.json"),
			&serde_json::to_vec(&meta).map_err(std::io::Error::other)?,
		)
	}

	fn rewrite_log(&self) -> std::io::Result<()> {
		let mut text = Vec::new();
		for entry in self.entries.values() {
			serde_json::to_writer(&mut text, entry).map_err(std::io::Error::other)?;
			text.push(b'\n');
		}
		replace(&self.log_path(), &text)
	}
}

/// The log and the vote. Cloned into the reader openraft asks for; both share one lock.
#[derive(Debug, Clone)]
pub struct LogStore {
	inner: Arc<Mutex<LogInner>>,
}

impl LogStore {
	/// Opens the store in `dir`, reading back what a previous run left.
	pub fn open(dir: &Path) -> std::io::Result<Self> {
		std::fs::create_dir_all(dir)?;
		let mut inner = LogInner {
			dir: dir.to_path_buf(),
			..Default::default()
		};
		if let Ok(bytes) = std::fs::read(dir.join("vote.json")) {
			inner.vote = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
		}
		if let Ok(bytes) = std::fs::read(dir.join("log-meta.json")) {
			let meta: LogMeta = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
			inner.last_purged = meta.last_purged;
			inner.committed = meta.committed;
		}
		if let Ok(text) = std::fs::read_to_string(inner.log_path()) {
			for line in text.lines() {
				// A torn last line is a write that was never acknowledged: stop there.
				let Ok(entry) = serde_json::from_str::<Entry<TypeConfig>>(line) else {
					break;
				};
				inner.entries.insert(entry.log_id.index, entry);
			}
		}
		Ok(Self {
			inner: Arc::new(Mutex::new(inner)),
		})
	}
}

impl RaftLogReader<TypeConfig> for LogStore {
	async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send>(
		&mut self,
		range: RB,
	) -> Result<Vec<Entry<TypeConfig>>, Error> {
		let inner = self.inner.lock().await;
		Ok(inner.entries.range(range).map(|(_, e)| e.clone()).collect())
	}
}

impl RaftLogStorage<TypeConfig> for LogStore {
	type LogReader = Self;

	async fn get_log_state(&mut self) -> Result<LogState<TypeConfig>, Error> {
		let inner = self.inner.lock().await;
		let last = inner
			.entries
			.values()
			.next_back()
			.map(|e| e.log_id)
			.or(inner.last_purged);
		Ok(LogState {
			last_purged_log_id: inner.last_purged,
			last_log_id: last,
		})
	}

	async fn get_log_reader(&mut self) -> Self::LogReader {
		self.clone()
	}

	async fn save_vote(&mut self, vote: &Vote<NodeId>) -> Result<(), Error> {
		let mut inner = self.inner.lock().await;
		let bytes = serde_json::to_vec(vote).map_err(|e| StorageIOError::write_vote(io(e)))?;
		replace(&inner.dir.join("vote.json"), &bytes)
			.map_err(|e| StorageIOError::write_vote(io(e)))?;
		inner.vote = Some(*vote);
		Ok(())
	}

	async fn read_vote(&mut self) -> Result<Option<Vote<NodeId>>, Error> {
		Ok(self.inner.lock().await.vote)
	}

	async fn save_committed(&mut self, committed: Option<LogId<NodeId>>) -> Result<(), Error> {
		let mut inner = self.inner.lock().await;
		inner.committed = committed;
		inner
			.write_meta()
			.map_err(|e| StorageIOError::write(io(e)).into())
	}

	async fn read_committed(&mut self) -> Result<Option<LogId<NodeId>>, Error> {
		Ok(self.inner.lock().await.committed)
	}

	async fn append<I>(&mut self, entries: I, callback: LogFlushed<TypeConfig>) -> Result<(), Error>
	where
		I: IntoIterator<Item = Entry<TypeConfig>> + Send,
		I::IntoIter: Send,
	{
		let mut inner = self.inner.lock().await;
		let mut text = Vec::new();
		let mut added = Vec::new();
		for entry in entries {
			serde_json::to_writer(&mut text, &entry)
				.map_err(|e| StorageIOError::write_logs(io(e)))?;
			text.push(b'\n');
			added.push(entry);
		}
		let result = (|| -> std::io::Result<()> {
			let mut file = std::fs::OpenOptions::new()
				.create(true)
				.append(true)
				.open(inner.log_path())?;
			file.write_all(&text)?;
			file.sync_all()
		})();
		match result {
			Ok(()) => {
				for entry in added {
					inner.entries.insert(entry.log_id.index, entry);
				}
				callback.log_io_completed(Ok(()));
				Ok(())
			}
			Err(e) => {
				let message = e.to_string();
				callback.log_io_completed(Err(std::io::Error::other(message)));
				Err(StorageIOError::write_logs(io(e)).into())
			}
		}
	}

	async fn truncate(&mut self, log_id: LogId<NodeId>) -> Result<(), Error> {
		let mut inner = self.inner.lock().await;
		inner.entries.split_off(&log_id.index);
		inner
			.rewrite_log()
			.map_err(|e| StorageIOError::write_logs(io(e)).into())
	}

	async fn purge(&mut self, log_id: LogId<NodeId>) -> Result<(), Error> {
		let mut inner = self.inner.lock().await;
		inner.entries = inner.entries.split_off(&(log_id.index + 1));
		inner.last_purged = Some(log_id);
		inner
			.write_meta()
			.map_err(|e| StorageIOError::write_logs(io(e)))?;
		inner
			.rewrite_log()
			.map_err(|e| StorageIOError::write_logs(io(e)).into())
	}
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Machine {
	last_applied: Option<LogId<NodeId>>,
	membership: StoredMembership<NodeId, Node>,
	state: State,
}

#[derive(Serialize, Deserialize)]
struct StoredSnapshot {
	meta: SnapshotMeta<NodeId, Node>,
	data: Vec<u8>,
}

/// The replicated state, readable by the server through [`StateMachineStore::state`].
#[derive(Debug, Clone)]
pub struct StateMachineStore {
	dir: PathBuf,
	machine: Arc<Mutex<Machine>>,
	snapshots: Arc<Mutex<u64>>,
}

impl StateMachineStore {
	/// Opens it in `dir`, restoring the last snapshot; the log replays the rest.
	pub fn open(dir: &Path) -> std::io::Result<Self> {
		std::fs::create_dir_all(dir)?;
		let mut machine = Machine::default();
		if let Ok(bytes) = std::fs::read(dir.join("snapshot.json")) {
			let stored: StoredSnapshot =
				serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
			machine = serde_json::from_slice(&stored.data).map_err(std::io::Error::other)?;
		}
		Ok(Self {
			dir: dir.to_path_buf(),
			machine: Arc::new(Mutex::new(machine)),
			snapshots: Arc::new(Mutex::new(0)),
		})
	}

	/// A copy of the replicated state as this member has applied it.
	pub async fn state(&self) -> State {
		self.machine.lock().await.state.clone()
	}

	/// The spec's agents (host to token sha256), without copying the rest of the state: asked on
	/// every agent report.
	pub async fn spec_agents(&self) -> Vec<(String, String)> {
		self.machine
			.lock()
			.await
			.state
			.spec
			.as_ref()
			.map(|spec| {
				spec.agents
					.iter()
					.map(|a| (a.host.clone(), a.token_sha256.clone()))
					.collect()
			})
			.unwrap_or_default()
	}
}

impl RaftSnapshotBuilder<TypeConfig> for StateMachineStore {
	async fn build_snapshot(&mut self) -> Result<Snapshot<TypeConfig>, Error> {
		let machine = self.machine.lock().await.clone();
		let data =
			serde_json::to_vec(&machine).map_err(|e| StorageIOError::read_state_machine(io(e)))?;
		let mut count = self.snapshots.lock().await;
		*count += 1;
		let id = match machine.last_applied {
			Some(last) => format!("{}-{}-{}", last.leader_id, last.index, *count),
			None => format!("--{}", *count),
		};
		let meta = SnapshotMeta {
			last_log_id: machine.last_applied,
			last_membership: machine.membership.clone(),
			snapshot_id: id,
		};
		let stored = serde_json::to_vec(&StoredSnapshot {
			meta: meta.clone(),
			data: data.clone(),
		})
		.map_err(|e| StorageIOError::write_snapshot(None, io(e)))?;
		replace(&self.dir.join("snapshot.json"), &stored)
			.map_err(|e| StorageIOError::write_snapshot(None, io(e)))?;
		Ok(Snapshot {
			meta,
			snapshot: Box::new(Cursor::new(data)),
		})
	}
}

impl RaftStateMachine<TypeConfig> for StateMachineStore {
	type SnapshotBuilder = Self;

	async fn applied_state(
		&mut self,
	) -> Result<(Option<LogId<NodeId>>, StoredMembership<NodeId, Node>), Error> {
		let machine = self.machine.lock().await;
		Ok((machine.last_applied, machine.membership.clone()))
	}

	async fn apply<I>(&mut self, entries: I) -> Result<Vec<super::types::Applied>, Error>
	where
		I: IntoIterator<Item = Entry<TypeConfig>> + Send,
		I::IntoIter: Send,
	{
		let mut machine = self.machine.lock().await;
		let mut out = Vec::new();
		for entry in entries {
			machine.last_applied = Some(entry.log_id);
			match entry.payload {
				EntryPayload::Blank => {}
				EntryPayload::Normal(command) => machine.state.apply(command),
				EntryPayload::Membership(membership) => {
					machine.membership = StoredMembership::new(Some(entry.log_id), membership);
				}
			}
			out.push(super::types::Applied);
		}
		Ok(out)
	}

	async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
		self.clone()
	}

	async fn begin_receiving_snapshot(&mut self) -> Result<Box<Cursor<Vec<u8>>>, Error> {
		Ok(Box::new(Cursor::new(Vec::new())))
	}

	async fn install_snapshot(
		&mut self,
		meta: &SnapshotMeta<NodeId, Node>,
		snapshot: Box<Cursor<Vec<u8>>>,
	) -> Result<(), Error> {
		let data = snapshot.into_inner();
		let mut restored: Machine = serde_json::from_slice(&data)
			.map_err(|e| StorageIOError::read_snapshot(Some(meta.signature()), io(e)))?;
		restored.last_applied = meta.last_log_id;
		restored.membership = meta.last_membership.clone();
		let stored = serde_json::to_vec(&StoredSnapshot {
			meta: meta.clone(),
			data,
		})
		.map_err(|e| StorageIOError::write_snapshot(Some(meta.signature()), io(e)))?;
		replace(&self.dir.join("snapshot.json"), &stored)
			.map_err(|e| StorageIOError::write_snapshot(Some(meta.signature()), io(e)))?;
		*self.machine.lock().await = restored;
		Ok(())
	}

	async fn get_current_snapshot(&mut self) -> Result<Option<Snapshot<TypeConfig>>, Error> {
		let Ok(bytes) = std::fs::read(self.dir.join("snapshot.json")) else {
			return Ok(None);
		};
		let stored: StoredSnapshot = serde_json::from_slice(&bytes)
			.map_err(|e| StorageIOError::read_snapshot(None, io(e)))?;
		Ok(Some(Snapshot {
			meta: stored.meta,
			snapshot: Box::new(Cursor::new(stored.data)),
		}))
	}
}
