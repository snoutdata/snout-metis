//! snout-metis: placement and routing for the stack's services.
//!
//! Metis decides which copies of which services run on which hosts, and the routing table every
//! proxy balances calls from. It is never in the path of a call (MT2) and never touches the
//! database (MT1). The host agent restarts what dies on its host; Metis acts only on what the agent
//! cannot fix, a host gone or a copy the agent gave up on (MT3).
//!
//! This crate is the pure core (MT7): a [`Spec`] (what should run, from the repo) and [`Facts`]
//! (what the agents report) in, an [`Outcome`] (placement, table, notes) out. No I/O, no clock: the
//! time is a fact, so the same input always gives the same decision and a logged decision can be
//! replayed (MT6). The members, the consensus and the transports come later and call this.

pub mod compile;
pub mod database;
pub mod decide;
#[cfg(test)]
mod decide_before;
#[cfg(test)]
mod differential_tests;
pub mod member;
#[cfg(test)]
mod orchestration_tests;
pub mod replay;
#[cfg(test)]
mod shortfall_tests;
pub mod spec;

pub use database::{DatabasePrimary, DatabaseReport, DatabaseRole, database_primary};
pub use decide::{Assignment, Outcome, Route, decide};
pub use spec::{
	CopyFacts, CopyState, Facts, HostFacts, HostSpec, Resources, RunSpec, ServiceSpec, Spec,
	VersionShare,
};
