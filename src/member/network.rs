//! How members talk to each other: openraft's three calls over HTTP, JSON, with the member token.

use super::types::{Node, NodeId, TypeConfig};
use openraft::error::{
	InstallSnapshotError, NetworkError, RPCError, RaftError, RemoteError, Unreachable,
};
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
	AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
	VoteRequest, VoteResponse,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

#[derive(Clone)]
pub struct Network {
	pub client: reqwest::Client,
	pub token: String,
}

impl RaftNetworkFactory<TypeConfig> for Network {
	type Network = Peer;

	async fn new_client(&mut self, target: NodeId, node: &Node) -> Self::Network {
		Peer {
			target,
			address: node.addr.clone(),
			client: self.client.clone(),
			token: self.token.clone(),
		}
	}
}

pub struct Peer {
	target: NodeId,
	address: String,
	client: reqwest::Client,
	token: String,
}

impl Peer {
	// The error type is openraft's, fixed by the trait this serves.
	#[allow(clippy::result_large_err)]
	async fn call<Req, Resp, E>(
		&self,
		path: &str,
		request: &Req,
		option: &RPCOption,
	) -> Result<Resp, RPCError<NodeId, Node, RaftError<NodeId, E>>>
	where
		Req: Serialize,
		Resp: DeserializeOwned,
		E: std::error::Error + DeserializeOwned,
	{
		let response = self
			.client
			.post(format!("{}{path}", self.address))
			.bearer_auth(&self.token)
			.timeout(option.hard_ttl())
			.json(request)
			.send()
			.await
			.map_err(|e| RPCError::Unreachable(Unreachable::new(&e)))?;
		if !response.status().is_success() {
			let status =
				std::io::Error::other(format!("{} answered {}", self.address, response.status()));
			return Err(RPCError::Network(NetworkError::new(&status)));
		}
		let answer: Result<Resp, RaftError<NodeId, E>> = response
			.json()
			.await
			.map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
		answer.map_err(|e| RPCError::RemoteError(RemoteError::new(self.target, e)))
	}
}

impl RaftNetwork<TypeConfig> for Peer {
	async fn append_entries(
		&mut self,
		rpc: AppendEntriesRequest<TypeConfig>,
		option: RPCOption,
	) -> Result<AppendEntriesResponse<NodeId>, RPCError<NodeId, Node, RaftError<NodeId>>> {
		self.call("/raft/append", &rpc, &option).await
	}

	async fn install_snapshot(
		&mut self,
		rpc: InstallSnapshotRequest<TypeConfig>,
		option: RPCOption,
	) -> Result<
		InstallSnapshotResponse<NodeId>,
		RPCError<NodeId, Node, RaftError<NodeId, InstallSnapshotError>>,
	> {
		self.call("/raft/snapshot", &rpc, &option).await
	}

	async fn vote(
		&mut self,
		rpc: VoteRequest<NodeId>,
		option: RPCOption,
	) -> Result<VoteResponse<NodeId>, RPCError<NodeId, Node, RaftError<NodeId>>> {
		self.call("/raft/vote", &rpc, &option).await
	}
}
