//! A Metis member: one of three (later five), on hosts in
//! different failure domains, agreeing through openraft on what Metis has decided.
//!
//! What is replicated is small and changes rarely: the spec an operator applied, and each decision
//! the leader took from it (placement, table, notes). What is NOT replicated is the agents' reports:
//! they arrive every few seconds and only the leader decides from them, so they stay in the leader's
//! memory. A new leader therefore decides nothing until it has listened for a full
//! `host_timeout_secs`, or it would take every host it has not heard from yet for a dead one.
//! The same holds for the agents' database reports (`database.rs`); what the leader concluded from
//! them, the database primary, IS replicated with each decision, so a new leader keeps it.
//!
//! Every member answers an agent asking what it should run and a proxy asking for the table, from
//! its own copy of the last decision, so a lost leader does not stop either (MT2). And every member
//! pushes the table to the routers (`push.rs`), which never ask.

pub mod config;
pub mod decider;
pub mod events;
pub mod network;
pub mod push;
pub mod record;
pub mod server;
pub mod spec_source;
pub mod store;
pub mod tls;
pub mod types;

pub use config::MemberConfig;
pub use server::{Member, start};
