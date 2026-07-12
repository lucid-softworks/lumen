use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_DIR: AtomicU64 = AtomicU64::new(1);

#[test]
fn cluster_fork_runs_real_worker_process() {
    let dir = std::env::temp_dir().join(format!(
        "lumen-cluster-test-{}-{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("cluster.js");
    std::fs::write(
        &script,
        r#"
          const cluster = require("node:cluster");
          if (cluster.isPrimary) {
            cluster.setupPrimary({ exec: __filename, args: [], silent: true });
            const worker = cluster.fork({ TEST_VALUE: "41" });
            let stdout = "";
            worker.process.stdout.on("data", chunk => { stdout += chunk; });
            worker.on("online", () => worker.send({ value: Number(process.env.START || 1) }));
            worker.on("message", message => {
              console.log("message", worker.id, message.value, !!cluster.workers[worker.id]);
              worker.disconnect();
            });
            worker.on("exit", code => console.log("exit", code, stdout.trim()));
          } else {
            console.log("worker", cluster.worker.id, cluster.isWorker, process.env.TEST_VALUE);
            cluster.worker.on("message", message => cluster.worker.send({ value: message.value + 41 }));
          }
        "#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_lumen-cli"))
        .arg(&script)
        .output()
        .expect("run cluster script");
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().lines().collect::<Vec<_>>(),
        ["message 1 42 true", "exit 0 worker 1 true 41"]
    );
}
