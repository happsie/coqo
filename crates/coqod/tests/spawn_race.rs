// SPDX-License-Identifier: Apache-2.0

//! Many clients starting `coqod` at once, round after round (#262): every
//! round ends with one daemon that every client reached, and no spawned
//! daemon fails (the losers exit 3 and their clients attach to the winner).
//!
//! CI runs a few rounds; set `COQO_SPAWN_RACE_ROUNDS` for a longer run.

use std::ffi::OsString;
use std::time::Duration;

use coqo_client::{ConnectOptions, DATA_DIR_ENV, DaemonPaths, SpawnOptions, connect_or_spawn};
use coqo_protocol::ClientConfig;
use coqo_protocol::daemon::{Shutdown, ShutdownParams, Status};

const COQOD: &str = env!("CARGO_BIN_EXE_coqod");
const SPAWNS: usize = 16;
const DEFAULT_ROUNDS: usize = 3;

fn rounds() -> usize {
    std::env::var("COQO_SPAWN_RACE_ROUNDS")
        .ok()
        .and_then(|r| r.parse().ok())
        .unwrap_or(DEFAULT_ROUNDS)
}

#[tokio::test(flavor = "multi_thread")]
async fn sixteen_concurrent_spawns_start_one_daemon_every_round() {
    for round in 0..rounds() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = DaemonPaths::at(tmp.path().join("run"));
        let data = tmp.path().join("data");
        let mut options = ConnectOptions::new(ClientConfig::new("coqod-tests", "test"));
        let mut spawn = SpawnOptions::new(COQOD);
        spawn.args = vec!["--idle-exit".into(), "30s".into()];
        spawn.env = vec![(OsString::from(DATA_DIR_ENV), data.clone().into())];
        options.spawn = spawn;
        options.ready_timeout = Duration::from_secs(30);

        let tasks: Vec<_> = (0..SPAWNS)
            .map(|_| {
                let (paths, options) = (paths.clone(), options.clone());
                tokio::spawn(async move { connect_or_spawn(&paths, &options).await })
            })
            .collect();
        let mut conns = Vec::new();
        for task in tasks {
            let conn = task.await.unwrap().unwrap_or_else(|e| {
                let log = std::fs::read_to_string(data.join("logs/coqod.log"))
                    .unwrap_or_else(|e| format!("(no log: {e})"));
                panic!("round {round}: connect_or_spawn: {e:#}\ncoqod log:\n{log}")
            });
            conns.push(conn);
        }
        let mut pids = Vec::new();
        for conn in &conns {
            pids.push(conn.client.request::<Status>(()).await.unwrap().pid);
        }
        pids.dedup();
        assert_eq!(pids.len(), 1, "round {round}: one daemon: {pids:?}");
        let stopped = conns[0]
            .client
            .request::<Shutdown>(ShutdownParams { force: true })
            .await
            .unwrap();
        assert!(stopped.stopping);
        for conn in &conns {
            conn.client.closed().await;
        }
    }
}
