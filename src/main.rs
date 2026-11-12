//! `snout-metis`, the operator's command over the pure core. Reads files, decides, prints; it
//! reaches no host and changes nothing, so the operator's own tooling gathers the facts and is the
//! only thing that writes a host's file.
//!
//!   snout-metis plan  --spec <spec.json> --facts <facts.json>
//!       the decision as JSON: notes, placement, table, and each host's `hostServices`
//!   snout-metis merge --spec <spec.json> --facts <facts.json> --host <name> --current <file>
//!       the host's desired-state file with Metis's entries in place, on stdout; the change, as
//!       JSON, on stderr
//!   snout-metis replay --log <decisions.jsonl>
//!       every logged decision's facts through the core again (MT6); exits 1 when one differs
//!   snout-metis member --config <member.json>
//!       run a member until SIGTERM or Ctrl-C

use metis::compile::{host_services, merge};
use metis::replay::replay;
use metis::{Facts, Spec, decide};
use std::collections::BTreeMap;
use std::process::ExitCode;

fn main() -> ExitCode {
	let args: Vec<String> = std::env::args().skip(1).collect();
	match run(&args) {
		Ok(()) => ExitCode::SUCCESS,
		Err(Failure::Usage(message)) => {
			eprintln!(
				"{message}\nusage: snout-metis plan --spec <file> --facts <file>\n       snout-metis merge --spec <file> --facts <file> --host <name> --current <file>\n       snout-metis replay --log <file>\n       snout-metis member --config <file>"
			);
			ExitCode::from(2)
		}
		Err(Failure::Refused(message)) => {
			eprintln!("snout-metis: {message}");
			ExitCode::from(1)
		}
	}
}

enum Failure {
	Usage(String),
	Refused(String),
}

fn run(args: &[String]) -> Result<(), Failure> {
	let Some(command) = args.first() else {
		return Err(Failure::Usage("no command".into()));
	};
	let flags = flags(&args[1..])?;
	if command == "member" {
		return run_member(read_json(flag(&flags, "config")?)?);
	}
	if command == "replay" {
		let path = flag(&flags, "log")?;
		let text =
			std::fs::read_to_string(path).map_err(|e| Failure::Refused(format!("{path}: {e}")))?;
		let out = replay(&text).map_err(|e| Failure::Refused(format!("{path}: {e}")))?;
		for line in &out.differ {
			println!("{line}");
		}
		println!(
			"{} decisions replayed, {} differ",
			out.decisions,
			out.differ.len()
		);
		return if out.differ.is_empty() {
			Ok(())
		} else {
			Err(Failure::Refused(format!(
				"{} of {} decisions differ",
				out.differ.len(),
				out.decisions
			)))
		};
	}
	let spec: Spec = read_json(flag(&flags, "spec")?)?;
	spec.check().map_err(Failure::Refused)?;
	let facts: Facts = read_json(flag(&flags, "facts")?)?;
	let outcome = decide(&spec, &facts);
	match command.as_str() {
		"plan" => {
			let mut hosts = BTreeMap::new();
			for host in &spec.hosts {
				hosts.insert(
					host.name.clone(),
					host_services(&spec, &outcome.placement, &host.name)
						.map_err(Failure::Refused)?,
				);
			}
			let out = serde_json::json!({
				"notes": outcome.notes,
				"placement": outcome.placement,
				"table": outcome.table,
				"hostServices": hosts,
			});
			println!(
				"{}",
				serde_json::to_string_pretty(&out).map_err(|e| Failure::Refused(e.to_string()))?
			);
			Ok(())
		}
		"merge" => {
			let host = flag(&flags, "host")?;
			if !spec.hosts.iter().any(|h| h.name == host) {
				return Err(Failure::Refused(format!("host {host} is not in the spec")));
			}
			let current = std::fs::read_to_string(flag(&flags, "current")?)
				.map_err(|e| Failure::Refused(e.to_string()))?;
			let entries =
				host_services(&spec, &outcome.placement, host).map_err(Failure::Refused)?;
			let (merged, change) = merge(&spec, &current, &entries).map_err(Failure::Refused)?;
			print!("{merged}");
			eprintln!(
				"{}",
				serde_json::to_string(&change).map_err(|e| Failure::Refused(e.to_string()))?
			);
			Ok(())
		}
		other => Err(Failure::Usage(format!("unknown command {other}"))),
	}
}

fn flags(args: &[String]) -> Result<BTreeMap<String, String>, Failure> {
	let mut out = BTreeMap::new();
	let mut rest = args.iter();
	while let Some(arg) = rest.next() {
		let Some(name) = arg.strip_prefix("--") else {
			return Err(Failure::Usage(format!("unexpected {arg}")));
		};
		let Some(value) = rest.next() else {
			return Err(Failure::Usage(format!("--{name} needs a value")));
		};
		out.insert(name.to_string(), value.clone());
	}
	Ok(out)
}

fn flag<'a>(flags: &'a BTreeMap<String, String>, name: &str) -> Result<&'a str, Failure> {
	flags
		.get(name)
		.map(String::as_str)
		.ok_or_else(|| Failure::Usage(format!("--{name} is required")))
}

fn read_json<T: serde::de::DeserializeOwned>(path: &str) -> Result<T, Failure> {
	let text =
		std::fs::read_to_string(path).map_err(|e| Failure::Refused(format!("{path}: {e}")))?;
	serde_json::from_str(&text).map_err(|e| Failure::Refused(format!("{path}: {e}")))
}

fn run_member(config: metis::member::MemberConfig) -> Result<(), Failure> {
	let runtime = tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()
		.map_err(|e| Failure::Refused(e.to_string()))?;
	runtime.block_on(async move {
		let listener = tokio::net::TcpListener::bind(&config.listen)
			.await
			.map_err(|e| Failure::Refused(format!("{}: {e}", config.listen)))?;
		let id = config.id;
		let (member, server) = metis::member::start(config, listener)
			.await
			.map_err(Failure::Refused)?;
		eprintln!("snout-metis: member {id} up");
		let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
			.map_err(|e| Failure::Refused(e.to_string()))?;
		tokio::select! {
			_ = term.recv() => {}
			_ = tokio::signal::ctrl_c() => {}
		}
		member.stop().await;
		server.abort();
		eprintln!("snout-metis: member {id} stopped");
		Ok(())
	})
}
