//! chloedb-server's integration tests, as one test binary.
//!
//! Each file in this directory was a test binary of its own, and each
//! binary linked all of chloedb-server and Strata: on 8 November, building
//! the 68 took 25.8 minutes of a 4 vCPU machine, and 13.0 as this one and
//! ten others. The files are modules of this binary instead; `cargo test
//! --test all sim::` runs one file's tests (`sim.rs`).
//!
//! A file that needs the process to itself keeps a binary of its own,
//! named in Cargo.toml (`[[test]]`): a global allocator, the process's log
//! subscriber or CPU profiler, or a quiet machine for its timings, which
//! the other tests' load would skew. A new file is a module here, or a
//! `[[test]]` there: Cargo.toml turns `autotests` off, so a file in
//! neither would not be built, and `every_file_is_built` below fails.

mod access;
mod agent_admin;
mod alerts;
mod apps;
mod autopilot;
mod arrow_answers;
mod arrow_leaves;
mod autoscale;
mod budgets;
mod aws;
mod chaos;
mod cli;
mod cloud_clusters;
mod cluster;
mod cluster_keys;
mod cluster_writes;
mod command_center;
mod connectors;
mod correlated;
mod date_cluster;
mod deep_statements;
mod digitalocean;
mod edge_tables;
mod empty_database;
mod encode;
mod failover;
mod feed;
mod feedback;
mod fleet;
mod follow;
mod follow_retries;
mod hash_cluster;
mod hosted_models;
mod hub;
mod index_inspector;
mod json_cluster;
mod key_value;
mod limits;
mod llm_calls;
mod local_shard;
mod map_lease;
mod mcp;
mod model_manager;
mod oncall;
mod pack_cluster;
mod opensearch;
mod operator_kube;
mod otlp;
mod partial_writes;
mod percentile_cluster;
mod plan_job;
mod plan_pb;
mod postgres;
mod answers;
mod profile;
mod rank_cross;
mod rank_log;
mod rank_models;
mod rank_profiles;
mod recommender;
mod replication;
mod safety;
mod schema_race;
mod scoped_keys;
mod limited_keys;
mod scroll_pages;
mod server;
mod sim;
mod sim_cluster;
mod simulator;
mod stored_functions;
mod stream;
mod stream_moves;
mod tables;
mod telemetry;
mod tenants;
mod ui;
mod upgrade;
mod write_stall;

#[test]
fn every_file_is_built() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let here = std::fs::read_to_string(dir.join("tests/all.rs")).unwrap();
    let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
    let mut lost = Vec::new();
    for e in std::fs::read_dir(dir.join("tests")).unwrap() {
        let name = e.unwrap().file_name().into_string().unwrap();
        let Some(stem) = name.strip_suffix(".rs") else { continue };
        if stem != "all" && !here.contains(&format!("\nmod {stem};\n")) && !manifest.contains(&format!("path = \"tests/{name}\"")) {
            lost.push(name);
        }
    }
    assert!(lost.is_empty(), "tests/{lost:?} would not be built: add `mod <name>;` to tests/all.rs, or a [[test]] to Cargo.toml");
}
