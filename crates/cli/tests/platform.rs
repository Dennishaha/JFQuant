use analytics_workflow::{
    WorkflowRequest, Workspace,
    service::{Client, Rpc, TaskRequest},
};
use serde_json::{Value, json};
use std::{
    process::{Command, Output},
    time::{Duration, Instant},
};
use uuid::Uuid;
struct Sandbox {
    temp: tempfile::TempDir,
    client: Client,
}
impl Sandbox {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("工作区 with spaces");
        Workspace::open(&root).unwrap();
        let client = Client::connect(&root).unwrap();
        Self { temp, client }
    }
    fn cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_jfquant"))
            .arg("--workspace")
            .arg(&self.client.root)
            .args(args)
            .output()
            .unwrap()
    }
    fn json(&self, args: &[&str]) -> Value {
        let o = self.cli(args);
        assert!(
            o.status.success(),
            "{} {}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        serde_json::from_slice(&o.stdout).unwrap()
    }
    fn submit(&self, r: TaskRequest) -> Uuid {
        self.client
            .ensure(Some(std::path::Path::new(env!("CARGO_BIN_EXE_jfquant"))))
            .unwrap();
        let id = Uuid::new_v4();
        self.client
            .call(Rpc::Submit {
                id,
                request: Box::new(r),
            })
            .unwrap();
        id
    }
    fn pause(&self, ms: u64) -> Uuid {
        self.submit(TaskRequest::TestingPause { milliseconds: ms })
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Ok(status) = self.client.call(Rpc::Status) {
            if let Some(id) = status["active"]["id"].as_str() {
                let id = id.parse().unwrap();
                let _ = self.client.call(Rpc::Cancel { id });
                let _ = self.client.wait(id, Some(10));
            }
            let _ = self.client.call(Rpc::Stop);
        }
        let _ = &self.temp;
    }
}
#[test]
fn storage_cli_scan_preview_and_permanent_release_share_service_results() {
    let s = Sandbox::new();
    let cache = s
        .client
        .root
        .join(".jfquant/runs/.factor-shards-v1/fixture/2020");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("features.parquet"), vec![7u8; 16384]).unwrap();
    std::fs::write(
        cache.join("manifest.json"),
        json!({"schema_version":1,"features":["fixture"],"sha256":"fixture","signature":"fixture"})
            .to_string(),
    )
    .unwrap();
    std::fs::write(cache.join("user-note.txt"), b"keep").unwrap();
    let query = s.json(&[
        "query",
        "run",
        "--sql",
        "SELECT * FROM (VALUES (1),(2)) AS v(n)",
        "--page-size",
        "1",
    ]);
    let query_id = query["submission"]["query_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let cursor = serde_json::from_value(
        query["result"]["results"]["QueryPaged"]["page"]["next_cursor"].clone(),
    )
    .unwrap();
    let scan = s.json(&["storage", "scan", "--level", "resources"])["task_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let scanned = s.client.wait(scan.parse().unwrap(), Some(20)).unwrap();
    let inventory = s.json(&["storage", "list", "--category", "factor_cache"]);
    assert_eq!(inventory["resources"].as_array().unwrap().len(), 1);
    assert_eq!(inventory["total"], scanned["results"]["total"]);
    let resource = inventory["resources"][0]["id"].as_str().unwrap();
    let task = s.json(&["storage", "plan", "--resource", resource])["task_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let preview = s.client.wait(task.parse().unwrap(), Some(20)).unwrap()["results"].clone();
    assert_eq!(preview["blockers"], json!([]), "{preview}");
    let plan = preview["id"].as_str().unwrap();
    assert!(
        !s.cli(&["storage", "apply", "--plan", plan])
            .status
            .success()
    );
    let apply = s.json(&["storage", "apply", "--plan", plan, "--confirm-permanent"])["task_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let result = s.client.wait(apply.parse().unwrap(), Some(20)).unwrap();
    assert_eq!(result["results"]["status"], "completed", "{result}");
    let expired = s.submit(TaskRequest::QueryNext {
        query_id,
        after: cursor,
    });
    assert_eq!(
        s.client.wait(expired, Some(10)).unwrap_err().code,
        "query_expired"
    );
    assert!(!cache.join("features.parquet").exists());
    assert!(cache.join("user-note.txt").is_file());
    assert_eq!(s.json(&["storage", "history"])[0]["plan"], plan);
    let rescan = s.json(&["storage", "scan", "--level", "resources"])["task_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    s.client.wait(rescan, Some(20)).unwrap();
    assert_eq!(
        s.json(&["storage", "list", "--status", "released"])["resources"][0]["id"],
        resource
    );
    let again = s.json(&["storage", "apply", "--plan", plan, "--confirm-permanent"])["task_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        s.client.wait(again.parse().unwrap(), Some(20)).unwrap()["results"]["deleted"],
        result["results"]["deleted"]
    );
}
#[test]
fn three_factor_capture_cli_exposes_temporal_policy_and_validates_before_network() {
    let s = Sandbox::new();
    let help = s.cli(&["data", "three-factor-inputs", "--help"]);
    assert!(help.status.success());
    let text = String::from_utf8(help.stdout).unwrap();
    assert!(
        text.contains("observed-on")
            && text.contains("provider-dates")
            && text.contains("--resume")
    );
    let error = s.cli(&[
        "data",
        "three-factor-inputs",
        "--market",
        "missing-market",
        "--inputs",
        "missing-template.json",
    ]);
    assert!(!error.status.success());
    let response: Value = serde_json::from_slice(&error.stdout).unwrap();
    assert!(response["error"]["message"].is_string());
    assert!(
        !s.client
            .root
            .join(".jfquant/staging")
            .read_dir()
            .is_ok_and(|mut d| d.next().is_some())
    );
}
#[test]
fn three_factor_cli_builds_hand_calculated_returns_and_publishes_a_new_version() {
    // Deliberately synthetic prices/fundamentals, with independent hand-calculated expected returns.
    let s = Sandbox::new();
    let w = Workspace::open(&s.client.root).unwrap();
    let root = w.resolve("data/ff3-fixture/market");
    std::fs::create_dir_all(root.join("calendars")).unwrap();
    std::fs::write(
        root.join("calendars/day.txt"),
        "2022-12-30\n2023-06-30\n2023-07-03\n",
    )
    .unwrap();
    std::fs::write(root.parent().unwrap().join("jfquant_source.json"),
        serde_json::to_vec(&json!({"generator":"JFQuant analytics-workflow synthetic-demo-v1","synthetic_fixture":true})).unwrap()).unwrap();
    let mut intervals = serde_json::Map::new();
    let mut shares = vec![];
    let mut books = vec![];
    for i in 1..=12 {
        let id = format!("SH{:06}", 600000 + i);
        let dir = root.join("features").join(id.to_lowercase());
        std::fs::create_dir_all(&dir).unwrap();
        for (name, values) in [
            ("close", vec![100_f32, 100., 100. + i as f32]),
            ("factor", vec![10.; 3]),
        ] {
            std::fs::write(
                dir.join(format!("{name}.day.bin")),
                std::iter::once(0_f32)
                    .chain(values)
                    .flat_map(f32::to_le_bytes)
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        }
        intervals.insert(
            id.clone(),
            json!([{"start":"2022-12-30","end":"2023-07-03"}]),
        );
        shares.push(json!({"instrument":id,"effective_date":"2022-01-01","published_on":"2022-01-01","total_shares":i}));
        books.push(json!({"instrument":id,"period_end":"2022-12-31","published_on":"2023-04-01","book_equity_cny":10*i*(((i-1)%6)/2+1)}));
    }
    let base = w.register(&root).unwrap();
    let request = json!({"schema_version":analytics_storage::three_factor::build::SCHEMA_VERSION,"construction_version":analytics_storage::three_factor::build::VERSION,"provider_history":null,
        "sources":{"universe":"synthetic fixture","shares":"synthetic fixture","book_equity":"synthetic fixture"},
        "market":"synthetic twelve stocks","range":{"start":"2023-07-03","end":"2023-07-03"},
        "price_basis":"provider","market_basis":"market_return",
        "universe":{"coverage":{"start":"2022-12-30","end":"2023-07-03"},"intervals":intervals},
        "shares":shares,"book_equity":books,"risk_free":[]});
    std::fs::write(
        w.resolve("facts.json"),
        serde_json::to_vec(&request).unwrap(),
    )
    .unwrap();
    let task = s.json(&[
        "data",
        "three-factors",
        "--market",
        &base.id,
        "--inputs",
        "facts.json",
    ]);
    let result = s.json(&[
        "jobs",
        "wait",
        task["task_id"].as_str().unwrap(),
        "--timeout",
        "30",
    ]);
    assert_eq!(result["record"]["status"], "succeeded", "{result}");
    assert_eq!(result["results"]["details"]["ready"], true);
    assert_eq!(result["results"]["details"]["complete_observations"], 1);
    let derived = result["results"]["data"]["path"].as_str().unwrap();
    let dataset = analytics_storage::daily::DailyDataset::open(w.resolve(derived)).unwrap();
    assert!((dataset.field("SH600001", "mkt").unwrap().get(2).unwrap() - 1. / 12.).abs() < 1e-13);
    assert!(
        !root
            .join(analytics_storage::three_factor::FILE_NAME)
            .exists()
    );
}
#[test]
fn supported_older_actions_expose_unknown_structured_fields_through_public_inspection() {
    let s = Sandbox::new();
    let w = Workspace::open(&s.client.root).unwrap();
    let (_, actions, _) = analytics_workflow::testing::synthetic(&w);
    let submitted = s.json(&[
        "data",
        "inspect-actions",
        "--actions",
        &actions.id,
        "--instrument",
        "SH600000",
    ]);
    let result = s.json(&[
        "jobs",
        "wait",
        submitted["task_id"].as_str().unwrap(),
        "--timeout",
        "10",
    ]);
    assert_eq!(result["record"]["status"], "succeeded");
    assert!(
        result["results"]["details"]
            .get("structured_fields")
            .is_some()
    );
    assert_eq!(
        result["results"]["details"]["structured_fields"],
        Value::Null
    );
    assert_eq!(
        analytics_storage::corporate_actions::ActionBundle::read(&w.resolve(actions.path))
            .unwrap()
            .schema_version,
        5
    );
}

#[test]
fn clap_help_validation_json_and_exit_codes_require_no_workspace() {
    let output = Command::new(env!("CARGO_BIN_EXE_jfquant"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    for c in ["data", "query", "research", "jobs", "templates"] {
        assert!(text.contains(c))
    }
    let output = Command::new(env!("CARGO_BIN_EXE_jfquant"))
        .args(["query", "run", "--sql", "SELECT 1", "--page-size", "0"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let error: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["error"]["code"], "invalid_request");
    let output = Command::new(env!("CARGO_BIN_EXE_jfquant"))
        .args(["query", "run", "--sql", "SELECT 1", "--sql-file", "a.sql"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn quality_flags_match_public_api_freezing_and_reject_invalid_thresholds() {
    let s = Sandbox::new();
    let w = Workspace::open(&s.client.root).unwrap();
    let (m, a, t) = analytics_workflow::demo::create_canonical(&w).unwrap();
    t.write(&w.resolve("recipe.json")).unwrap();
    for (mode, strictness) in [
        (
            "strict",
            analytics_core::research_quality::ResearchStrictness::Strict,
        ),
        (
            "allow_missing",
            analytics_core::research_quality::ResearchStrictness::AllowMissing,
        ),
        (
            "qualified_only",
            analytics_core::research_quality::ResearchStrictness::QualifiedOnly,
        ),
    ] {
        let mut template = t.clone();
        template.quality_policy.strictness = strictness;
        let request = WorkflowRequest::Research {
            template: Box::new(template),
            market: m.id.clone(),
            actions: Some(a.id.clone()),
            audit_only: false,
        };
        let id = Uuid::new_v4();
        let mut record = w
            .begin(
                id,
                "direct quality",
                analytics_workflow::snapshot(&w, &request).unwrap(),
            )
            .unwrap();
        let api = analytics_workflow::execute(
            &w,
            &request,
            &record,
            &analytics_core::task::TaskContext::default(),
        )
        .unwrap()
        .details;
        w.finish(&mut record, analytics_workflow::RunStatus::Succeeded, None)
            .unwrap();
        let submitted = s.json(&[
            "research",
            "run",
            "--template",
            "recipe.json",
            "--market",
            &m.id,
            "--actions",
            &a.id,
            "--strictness",
            mode,
            "--min-valid-feature-ratio",
            "0",
            "--min-labeled-rows",
            "1",
        ]);
        let id = submitted["task_id"].as_str().unwrap();
        s.json(&["jobs", "wait", id, "--timeout", "30"]);
        let cli = s.json(&["jobs", "results", id]);
        assert_eq!(cli, api);
    }
    let failed = s.cli(&[
        "research",
        "run",
        "--template",
        "recipe.json",
        "--market",
        &m.id,
        "--actions",
        &a.id,
        "--strictness",
        "allow_missing",
        "--min-valid-feature-ratio",
        "2",
    ]);
    assert_eq!(failed.status.code(), Some(2));
    let help = s.cli(&["research", "check", "--help"]);
    assert!(
        String::from_utf8(help.stdout)
            .unwrap()
            .contains("qualified_only")
    );
}
#[test]
fn async_acceptance_busy_wait_timeout_cancel_and_client_exit_preserve_job() {
    let s = Sandbox::new();
    let id = s.pause(500);
    let busy = s
        .client
        .call(Rpc::Submit {
            id: Uuid::new_v4(),
            request: Box::new(TaskRequest::Workflow {
                request: Box::new(WorkflowRequest::CreateDemo {}),
            }),
        })
        .unwrap_err();
    assert_eq!(busy.code, "busy");
    assert_eq!(busy.task_id, Some(id));
    let restart = s.client.call(Rpc::Restart).unwrap_err();
    assert_eq!(restart.code, "busy");
    assert_eq!(restart.task_id, Some(id));
    let o = s.cli(&["jobs", "wait", &id.to_string(), "--timeout", "0"]);
    assert_eq!(o.status.code(), Some(124));
    assert_eq!(s.client.call(Rpc::Job { id }).unwrap()["status"], "running");
    assert_eq!(s.cli(&["research", "demo"]).status.code(), Some(4));
    s.client.wait(id, Some(10)).unwrap(); // waiter process exited, computation continued.
    let id = s.pause(5000);
    s.json(&["jobs", "cancel", &id.to_string()]);
    let o = s.cli(&["jobs", "wait", &id.to_string()]);
    assert_eq!(o.status.code(), Some(130));
    assert_eq!(
        s.client.call(Rpc::Job { id }).unwrap()["status"],
        "cancelled"
    );
    assert!(
        !s.json(&["jobs", "logs", &id.to_string()])["lines"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
#[test]
fn separate_query_sessions_keep_null_duplicate_pages_and_report_expiration() {
    let s = Sandbox::new();
    let a = s.json(&[
        "query",
        "run",
        "--sql",
        "SELECT * FROM (VALUES (NULL),(1),(1),(2)) AS v(n)",
        "--page-size",
        "2",
    ]);
    let b = s.json(&[
        "query",
        "run",
        "--sql",
        "SELECT * FROM (VALUES (10),(20),(30)) AS v(n)",
        "--page-size",
        "2",
    ]);
    let query_id = a["submission"]["query_id"].as_str().unwrap();
    assert_ne!(query_id, b["submission"]["query_id"].as_str().unwrap());
    let cursor = a["result"]["results"]["QueryPaged"]["page"]["next_cursor"].to_string();
    let next = s.json(&[
        "query",
        "next",
        "--query-id",
        query_id,
        "--cursor-json",
        &cursor,
    ]);
    let page = &next["result"]["results"]["QueryPaged"]["page"];
    assert_eq!(page["rows"][0][0], json!({"Int64":2}));
    assert_eq!(page["rows"][1][0], "Null");
    let id = s.submit(TaskRequest::QueryNext {
        query_id: Uuid::new_v4(),
        after: serde_json::from_str(&cursor).unwrap(),
    });
    let error = s.client.wait(id, Some(10)).unwrap_err();
    assert_eq!(error.code, "query_expired");
}
#[test]
fn cli_native_research_templates_results_and_incomplete_actions_share_strict_checks() {
    let s = Sandbox::new();
    let submitted = s.json(&["research", "demo"]);
    let id = submitted["task_id"].as_str().unwrap().parse().unwrap();
    s.client.wait(id, Some(10)).unwrap();
    let demo = s.client.call(Rpc::Results { id }).unwrap();
    let mut t = demo["details"]["demo_template"].clone();
    t["initial_cash_cny"] = json!(1050.0);
    t["market_rules"]["commission"]["minimum_cny"] = json!(5.0);
    serde_json::from_value::<analytics_workflow::ResearchTemplate>(t)
        .unwrap()
        .write(&s.client.root.join("recipe.json"))
        .unwrap();
    assert_eq!(
        s.json(&["templates", "validate", "recipe.json"])["valid"],
        true
    );
    s.json(&["templates", "save", "recipe.json"]);
    assert_eq!(s.json(&["templates", "list"]).as_array().unwrap().len(), 1);
    let market = demo["data"]["id"].as_str().unwrap();
    let actions = demo["details"]["demo_actions"].as_str().unwrap();
    let run = s.json(&[
        "research",
        "run",
        "--template",
        "recipe.json",
        "--market",
        market,
        "--actions",
        actions,
    ]);
    let id = run["task_id"].as_str().unwrap();
    let result = s.json(&["jobs", "wait", id, "--timeout", "20"]);
    assert_eq!(
        result["results"]["annual_reports"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(
        (result["results"]["aggregate"]["total_return"]
            .as_f64()
            .unwrap()
            + 5.0 / 1050.0)
            .abs()
            < 1e-6
    );
    let run: analytics_workflow::RunRecord =
        serde_json::from_value(result["record"].clone()).unwrap();
    let snapshots: Value = serde_json::from_slice(
        &std::fs::read(
            s.client
                .root
                .join(&run.output)
                .join("portfolio_snapshots.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        snapshots["2009-01-12"]["positions"],
        snapshots["2010-01-12"]["positions"]
    );
    let original = result["results"].clone();
    s.json(&["service", "stop"]);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(s.json(&["jobs", "results", id]), original);
    let w = Workspace::open(&s.client.root).unwrap();
    let action = w
        .datasets()
        .unwrap()
        .into_iter()
        .find(|a| a.id == actions)
        .unwrap();
    let path = w.resolve(action.path);
    let mut book = analytics_storage::corporate_actions::ActionBundle::read(&path).unwrap();
    book.actions[0].pay_date = None;
    std::fs::write(&path, serde_json::to_vec(&book).unwrap()).unwrap();
    let registration = s.json(&["data", "register", path.to_str().unwrap()]);
    let registered = registration["task_id"].as_str().unwrap().parse().unwrap();
    s.client.wait(registered, Some(10)).unwrap();
    let new = s.client.call(Rpc::Results { id: registered }).unwrap();
    let actions = new["data"]["id"].as_str().unwrap();
    let run = s.json(&[
        "research",
        "run",
        "--template",
        "recipe.json",
        "--market",
        market,
        "--actions",
        actions,
    ]);
    let id = run["task_id"].as_str().unwrap();
    assert_eq!(s.cli(&["jobs", "wait", id]).status.code(), Some(0));
    assert_eq!(
        s.json(&["jobs", "results", id])["execution"]["corporate_action_policy"],
        "ex_date"
    );
}
#[test]
fn startup_race_uses_one_owner_and_crash_recovery_marks_interrupted() {
    let s = Sandbox::new();
    let binary = std::path::Path::new(env!("CARGO_BIN_EXE_jfquant"));
    let clients: Vec<_> = (0..4)
        .map(|_| {
            let c = s.client.clone();
            std::thread::spawn(move || {
                c.ensure(Some(binary)).unwrap();
                c.call(Rpc::Status).unwrap()["pid"].as_u64().unwrap()
            })
        })
        .collect();
    let ids: Vec<_> = clients.into_iter().map(|c| c.join().unwrap()).collect();
    assert!(ids.iter().all(|id| *id == ids[0]));
    assert!(Workspace::open(&s.client.root).unwrap().lease().is_err());
    let id = s.pause(5000);
    assert!(
        Command::new("/bin/kill")
            .args(["-9", &ids[0].to_string()])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && s.client.call(Rpc::Status).is_ok() {
        std::thread::sleep(Duration::from_millis(20));
    }
    s.client.ensure(Some(binary)).unwrap();
    assert_eq!(
        s.client.call(Rpc::Job { id }).unwrap()["status"],
        "interrupted"
    );
}

#[test]
fn large_json_frames_are_not_truncated_at_socket_buffer_boundaries() {
    let s = Sandbox::new();
    let result = s.json(&[
        "query",
        "run",
        "--sql",
        "SELECT repeat('界', 30000) AS payload",
    ]);
    assert_eq!(
        result["result"]["results"]["QueryPaged"]["page"]["rows"][0][0]["Utf8"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        30000
    );
    let events = s.client.call(Rpc::Events { after: 0 }).unwrap();
    assert!(serde_json::to_vec(&events).unwrap().len() > 8192);
}

#[test]
fn sql_nonfinite_cells_round_trip_through_the_same_events_consumed_by_app() {
    let s = Sandbox::new();
    let result=s.json(&["query","run","--sql","SELECT CAST('NaN' AS DOUBLE), CAST('Infinity' AS DOUBLE), CAST('-Infinity' AS DOUBLE), NULL"]);
    let values = &result["result"]["results"]["QueryPaged"]["page"]["rows"][0];
    assert_eq!(values[0], json!({"Float64":"NaN"}));
    assert_eq!(values[1], json!({"Float64":"Infinity"}));
    assert_eq!(values[2], json!({"Float64":"-Infinity"}));
    assert_eq!(values[3], "Null");
    let events = s.client.call(Rpc::Events { after: 0 }).unwrap();
    for item in events["events"].as_array().unwrap() {
        let _: analytics_workflow::backend::WorkerEvent =
            serde_json::from_value(item[1].clone()).unwrap();
    }
}

#[test]
fn late_clients_can_read_events_after_large_query_pages() {
    let s = Sandbox::new();
    for _ in 0..3 {
        let id = s.submit(TaskRequest::Query {
            query_id: None,
            request: analytics_core::PageRequest {
                sql: "SELECT repeat('x', 12582912) AS large_cell".into(),
                limit: 1,
                sort: vec![],
                after: None,
            },
        });
        s.client.wait(id, Some(30)).unwrap();
    }
    // Replaying all prior result pages used to exceed the 64 MiB frame limit.
    let events = s.client.call(Rpc::Events { after: 0 }).unwrap();
    assert!(serde_json::to_vec(&events).unwrap().len() < 64 * 1024 * 1024);
    assert!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e[1].get("Finished").is_some())
    );
}

fn owned_archive(w: &Workspace, provider: &std::path::Path, owner: Uuid) -> std::path::PathBuf {
    use sha2::{Digest, Sha256};
    use std::{fs, io::Write};
    let stage = w.resolve(format!(".jfquant/staging/{owner}"));
    fs::create_dir_all(&stage).unwrap();
    let archive = stage.join("qlib_bin.tar.gz");
    let gz = flate2::write::GzEncoder::new(
        fs::File::create(&archive).unwrap(),
        flate2::Compression::default(),
    );
    let mut tar = tar::Builder::new(gz);
    tar.append_dir_all("qlib_bin", provider).unwrap();
    let members = fs::read(provider.join("instruments/csi500.txt")).unwrap();
    let mut header = tar::Header::new_gnu();
    header.set_size(members.len() as u64);
    header.set_mode(0o600);
    header.set_cksum();
    tar.append_data(
        &mut header,
        "qlib_bin/instruments/all.txt",
        members.as_slice(),
    )
    .unwrap();
    tar.into_inner().unwrap().finish().unwrap().flush().unwrap();
    let bytes = fs::read(&archive).unwrap();
    fs::write(stage.join("qlib_bin.manifest.json"),json!({"release_tag":"2010-01-12","target_trade_date":"2010-01-12","archive_size_bytes":bytes.len(),"archive_sha256":format!("sha256:{:x}",analytics_core::hash::Hex(Sha256::digest(bytes)))}).to_string()).unwrap();
    fs::write(
        stage.join("owner.json"),
        json!({"schema_version":1,"downloader":"JFQuant analytics-storage","tag":"2010-01-12"})
            .to_string(),
    )
    .unwrap();
    stage
}
#[test]
fn interrupting_initial_foreground_wait_does_not_signal_new_background_service() {
    use std::os::unix::process::CommandExt;
    let s = Sandbox::new();
    let pause = s.client.root.join("pause.json");
    std::fs::write(
        &pause,
        r#"{"operation":"testing_pause","milliseconds":1500}"#,
    )
    .unwrap();
    let id_file = s.client.root.join("accepted.json");
    // One foreground process group starts the daemon and waits, like --wait.
    // Only positional shell parameters hold paths: no shell interpolation of user input.
    let mut child = Command::new("/bin/sh")
        .args(["-c",r#""$1" --workspace "$2" run --request pause.json > "$3" || exit; task=$(/usr/bin/sed -E 's/.*"task_id":"([^"]+)".*/\1/' "$3"); "$1" --workspace "$2" jobs wait "$task""#,"foreground-wait-test"])
        .arg(env!("CARGO_BIN_EXE_jfquant")).arg(&s.client.root).arg(&id_file)
        .process_group(0).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
        .spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let task = loop {
        if let Ok(bytes) = std::fs::read(&id_file)
            && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
            && let Some(task) = value["task_id"].as_str()
        {
            break task.parse().unwrap();
        }
        assert!(
            Instant::now() < deadline,
            "initial foreground client did not submit"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let pgid = rustix::process::Pid::from_raw(child.id() as i32).unwrap();
    rustix::process::kill_process_group(pgid, rustix::process::Signal::INT).unwrap();
    let _ = child.wait().unwrap();
    assert!(
        s.client.call(Rpc::Status).is_ok(),
        "Ctrl-C killed the service"
    );
    assert_eq!(
        s.client.wait(task, Some(10)).unwrap()["record"]["status"],
        "succeeded"
    );
}

#[test]
fn removed_entrypoints_and_old_requests_are_rejected_without_starting_tasks() {
    let s = Sandbox::new();
    for args in [
        vec!["compat", "rolling-backtest"],
        vec!["market_data"],
        vec!["research", "run", "--legacy-adjusted"],
        vec!["research", "run", "--reprice-from", "old"],
    ] {
        let output = s.cli(&args);
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap()["error"]["code"],
            "invalid_request"
        );
    }
    for value in [
        json!({"operation":"create_demo"}),
        json!({"operation":"workflow","request":{"operation":"compatibility","request":{"operation":"market_rules","output":"fees.json"}}}),
        json!({"operation":"workflow","request":{"operation":"create_demo","legacy_adjusted":true}}),
    ] {
        std::fs::write(s.client.root.join("old-request.json"), value.to_string()).unwrap();
        assert_eq!(
            s.cli(&["run", "--request", "old-request.json"])
                .status
                .code(),
            Some(2)
        );
    }
    assert_eq!(s.json(&["jobs", "list"]), json!([]));
    assert!(!s.client.root.join("fees.json").exists());
    std::fs::write(
        s.client.root.join("current-request.json"),
        json!({"operation":"workflow","request":{"operation":"create_demo"}}).to_string(),
    )
    .unwrap();
    let value = s.json(&["run", "--request", "current-request.json"]);
    s.client
        .wait(
            value["task_id"].as_str().unwrap().parse().unwrap(),
            Some(10),
        )
        .unwrap();
}

#[test]
fn current_research_cli_and_api_share_results_and_reject_old_imports_without_modifying_files() {
    let s = Sandbox::new();
    let w = Workspace::open(&s.client.root).unwrap();
    let (m, a, mut template) = analytics_workflow::testing::synthetic(&w);
    template.initial_cash_cny = 1050.0;
    template.market_rules.commission.minimum_cny = 5.0;
    template.write(&w.resolve("研究 recipe.json")).unwrap();
    let id = s.submit(TaskRequest::Workflow {
        request: Box::new(WorkflowRequest::Research {
            template: Box::new(template),
            market: m.id.clone(),
            actions: Some(a.id.clone()),
            audit_only: false,
        }),
    });
    let expected = s.client.wait(id, Some(15)).unwrap()["results"].clone();
    let value = s.json(&[
        "research",
        "run",
        "--template",
        "研究 recipe.json",
        "--market",
        &m.id,
        "--actions",
        &a.id,
    ]);
    let cli_id = value["task_id"].as_str().unwrap().parse().unwrap();
    let actual = s.client.wait(cli_id, Some(15)).unwrap();
    let allocation_input = |task: Uuid| {
        serde_json::from_slice::<Value>(
            &std::fs::read(w.resolve(format!(".jfquant/runs/{task}/allocation-input.json")))
                .unwrap(),
        )
        .unwrap()
    };
    assert_eq!(allocation_input(cli_id), allocation_input(id));
    assert_eq!(actual["results"], expected);
    assert_eq!(
        expected["schema_version"],
        analytics_workflow::research::backtest::RESEARCH_SCHEMA_VERSION
    );
    assert!(
        expected["aggregate"]
            .get("terminal_valuation_fees_cny")
            .is_none()
    );
    assert_eq!(
        actual["record"]["request"]["request"]["template"]["config"]["factor_ids"],
        json!(["alpha_002"])
    );
    let snapshot = |id: &str| {
        serde_json::from_slice::<Value>(
            &std::fs::read(w.resolve(format!(".jfquant/runs/{id}/portfolio_snapshots.json")))
                .unwrap(),
        )
        .unwrap()
    };
    assert_eq!(snapshot(&id.to_string()), snapshot(&cli_id.to_string()));
    let verification = s.json(&["research", "verify", "--task", &cli_id.to_string()]);
    assert_eq!(verification["status"], "consistent");
    assert_eq!(
        verification["event_simulation_complete"],
        expected["event_simulation_complete"]
    );
    assert_eq!(verification["fills_on_valuation_only_rows"], 0);
    assert_eq!(
        verification,
        analytics_workflow::research::backtest::verification::report(&w, cli_id).unwrap()
    );
    let daily_path = w.resolve(format!(".jfquant/runs/{cli_id}/daily.json"));
    let daily_bytes = std::fs::read(&daily_path).unwrap();
    let mut bad_daily: Value = serde_json::from_slice(&daily_bytes).unwrap();
    bad_daily[0]["execution_fee_cny"] = json!(999.);
    std::fs::write(&daily_path, serde_json::to_vec(&bad_daily).unwrap()).unwrap();
    assert!(
        !s.cli(&["research", "verify", "--task", &cli_id.to_string()])
            .status
            .success()
    );
    std::fs::write(&daily_path, daily_bytes).unwrap();
    let imported = s.json(&["jobs", "import", &format!(".jfquant/runs/{id}")]);
    let import_id = imported["id"].as_str().unwrap();
    assert_eq!(s.json(&["jobs", "results", import_id]), expected);
    let old_path = w.resolve("old-results");
    std::fs::create_dir(&old_path).unwrap();
    let mut old = expected.clone();
    old["schema_version"] = json!(3);
    let bytes = serde_json::to_vec_pretty(&old).unwrap();
    std::fs::write(old_path.join("summary.json"), &bytes).unwrap();
    let count = w.runs().unwrap().len();
    let output = s.cli(&["jobs", "import", "old-results"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["error"]["code"],
        "unsupported_result_format"
    );
    assert_eq!(w.runs().unwrap().len(), count);
    assert_eq!(std::fs::read(old_path.join("summary.json")).unwrap(), bytes);
    let mut record = w
        .begin(
            Uuid::new_v4(),
            "existing old task",
            json!({"request":{"operation":"compatibility"}}),
        )
        .unwrap();
    std::fs::write(w.resolve(&record.output).join("summary.json"), &bytes).unwrap();
    w.finish(&mut record, analytics_workflow::RunStatus::Succeeded, None)
        .unwrap();
    let output = s.cli(&["jobs", "results", &record.id.to_string()]);
    let error: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["error"]["code"], "unsupported_result_format");
    assert_eq!(error["error"]["task_id"], record.id.to_string());
    assert!(
        s.cli(&["jobs", "status", &record.id.to_string()])
            .status
            .success()
    );
    assert!(
        s.cli(&["jobs", "logs", &record.id.to_string()])
            .status
            .success()
    );
    for field in ["action_policy", "schedule", "resolved_folds"] {
        let mut corrupt = expected.clone();
        corrupt["execution"].as_object_mut().unwrap().remove(field);
        assert!(analytics_workflow::research::backtest::validate_result(&corrupt).is_err());
    }
    for corrupt in [
        {
            let mut v = expected.clone();
            v["execution"]["mode"] = json!("legacy_adjusted_fractional");
            v
        },
        {
            let mut v = expected.clone();
            v.as_object_mut().unwrap().remove("annual_reports");
            v
        },
        {
            let mut v = expected.clone();
            v["aggregate"]["terminal_valuation_fees_cny"] = json!(0);
            v
        },
    ] {
        assert!(analytics_workflow::research::backtest::validate_result(&corrupt).is_err());
    }
    assert_eq!(s.json(&["jobs", "results", &id.to_string()]), expected);
}

#[test]
fn standard_download_resumes_verified_checkpoint_and_rejects_busy_or_wrong_owner_offline() {
    let s = Sandbox::new();
    let w = Workspace::open(&s.client.root).unwrap();
    let (market, _, _) = analytics_workflow::testing::synthetic(&w);
    let owner = Uuid::new_v4();
    let staging = owned_archive(&w, &w.resolve(&market.path), owner);
    let value = s.json(&[
        "data",
        "download",
        "--tag",
        "2010-01-12",
        "--resume",
        &owner.to_string(),
    ]);
    let id = value["task_id"].as_str().unwrap().parse().unwrap();
    let result = s.client.wait(id, Some(15)).unwrap();
    assert_eq!(result["record"]["status"], "succeeded");
    assert!(
        w.resolve(format!(
            "data/market-2010-01-12-{id}/qlib_bin/calendars/day.txt"
        ))
        .is_file()
    );
    assert!(!staging.exists());
    let owner = Uuid::new_v4();
    let staging = owned_archive(&w, &w.resolve(&market.path), owner);
    let busy = s.pause(5000);
    let output = s.cli(&[
        "data",
        "download",
        "--tag",
        "2010-01-12",
        "--resume",
        &owner.to_string(),
    ]);
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["error"]["task_id"],
        busy.to_string()
    );
    s.client.call(Rpc::Cancel { id: busy }).unwrap();
    assert_eq!(s.client.wait(busy, Some(15)).unwrap_err().code, "cancelled");
    std::fs::write(
        staging.join("owner.json"),
        json!({"schema_version":1,"downloader":"different","tag":"2010-01-12"}).to_string(),
    )
    .unwrap();
    let before = w.datasets().unwrap().len();
    let value = s.json(&[
        "data",
        "download",
        "--tag",
        "2010-01-12",
        "--resume",
        &owner.to_string(),
    ]);
    let id = value["task_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(s.client.wait(id, Some(15)).unwrap_err().code, "task_failed");
    assert!(!w.resolve(format!("data/market-2010-01-12-{id}")).exists());
    assert_eq!(w.datasets().unwrap().len(), before);
}

#[test]
fn standard_pipeline_json_preserves_feature_order_and_persistent_artifacts() {
    let s = Sandbox::new();
    let market = "SELECT 'SH600000' AS instrument_id, DATE '2020-01-01' + range::INTEGER AS trade_date, 10.0+range*.01 AS open, open+1 AS high, open-1 AS low, open+.2 AS close, 1000.0+range AS volume, volume*close AS amount, open AS adj_open, high AS adj_high, low AS adj_low, close AS adj_close, true AS is_trading, DATE '2000-01-01' AS listing_date FROM range(120)";
    let universe = "SELECT 'SH600000' AS instrument_id, 'csi500' AS universe_id, DATE '2000-01-01' AS trade_date, true AS is_member";
    s.json(&[
        "query",
        "export",
        "--sql",
        market,
        "--output",
        "market.parquet",
    ]);
    s.json(&[
        "query",
        "export",
        "--sql",
        universe,
        "--output",
        "universe.parquet",
    ]);
    let request = analytics_workflow::research::pipeline::PipelineRequest {
        factor_refs: Vec::new(),
        factor_set: Default::default(),
        market_path: "market.parquet".into(),
        universe_path: "universe.parquet".into(),
        universe_id: "csi500".into(),
        date_range: analytics_core::DateRange {
            start: "2020-01-01".parse().unwrap(),
            end: "2020-04-30".parse().unwrap(),
        },
        factor_ids: vec!["alpha_002".into(), "alpha_001".into()],
        output_dir: "unused-output".into(),
        training: Default::default(),
    };
    std::fs::write(
        s.client.root.join("pipeline.json"),
        serde_json::to_vec(&request).unwrap(),
    )
    .unwrap();
    let value = s.json(&["factors", "run", "--request", "pipeline.json"]);
    let id = value["task_id"].as_str().unwrap().parse().unwrap();
    let result = s.client.wait(id, Some(15)).unwrap();
    assert_eq!(
        result["record"]["request"]["request"]["request"]["factor_ids"],
        json!(["alpha_002", "alpha_001"])
    );
    assert!(
        s.client
            .root
            .join(format!(".jfquant/runs/{id}/factors.parquet"))
            .is_file()
    );
    assert!(!s.client.root.join("unused-output").exists());
    assert!(s.cli(&["jobs", "list"]).status.success());
}

#[test]
fn cli_pools_and_canonical_research_share_public_backend_and_schema() {
    let s = Sandbox::new();
    let submitted = s.json(&["research", "demo-parquet"]);
    let id = submitted["task_id"].as_str().unwrap();
    s.json(&["jobs", "wait", id, "--timeout", "30"]);
    let demo = s.json(&["jobs", "results", id]);
    let market = demo["data"]["id"].as_str().unwrap();
    let actions = demo["details"]["demo_actions"].as_str().unwrap();
    let definition = json!({"schema_version":3,"name":"CLI self-selected pool","coverage":{"start":"2007-01-02","end":"2010-01-12"},"expression":{"kind":"symbols","instruments":["SH600000","SH600001"]},"adjustments":[]});
    std::fs::write(
        s.client.root.join("pool.json"),
        serde_json::to_vec(&definition).unwrap(),
    )
    .unwrap();
    let version = s.json(&["pools", "create", "--definition", "pool.json"]);
    let pool_id = version["id"].as_str().unwrap();
    let public = Workspace::open(&s.client.root)
        .unwrap()
        .pool(pool_id)
        .unwrap();
    assert_eq!(version["signature"], public.signature);
    let preview = s.json(&[
        "pools",
        "preview",
        pool_id,
        "--market",
        market,
        "--date",
        "2009-01-03",
    ]);
    assert_eq!(preview["instruments"].as_array().unwrap().len(), 2);
    let mut template = demo["details"]["demo_template"].clone();
    template["pool_version"] = json!(pool_id);
    serde_json::from_value::<analytics_workflow::ResearchTemplate>(template)
        .unwrap()
        .write(&s.client.root.join("recipe.json"))
        .unwrap();
    let run = s.json(&[
        "research",
        "run",
        "--template",
        "recipe.json",
        "--market",
        market,
        "--actions",
        actions,
    ]);
    let id = run["task_id"].as_str().unwrap();
    s.json(&["jobs", "wait", id, "--timeout", "30"]);
    let result = s.json(&["jobs", "results", id]);
    assert_eq!(
        result["schema_version"],
        analytics_workflow::research::backtest::RESEARCH_SCHEMA_VERSION
    );
    assert_eq!(
        result["execution"]["benchmark"],
        analytics_workflow::demo::fixture_security_id("SH000905")
    );
    assert_eq!(result["execution"]["pool_exit_policy"], "next_tradable_day");
    assert!(
        result["execution"]["pool_signature"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    let bad = s.cli(&[
        "data",
        "collect",
        "--provider",
        "unknown",
        "--start",
        "2020-01-01",
        "--end",
        "2020-01-02",
        "--instruments",
        "SH600000",
    ]);
    assert_eq!(bad.status.code(), Some(2));
}

#[test]
fn date_overrides_preview_and_no_validation_run_share_the_public_schedule() {
    let s = Sandbox::new();
    let w = Workspace::open(&s.client.root).unwrap();
    let (m, a, t) = analytics_workflow::demo::create_canonical(&w).unwrap();
    t.write(&w.resolve("recipe.json")).unwrap();
    let submit = s.json(&[
        "research",
        "preview",
        "--template",
        "recipe.json",
        "--market",
        &m.id,
        "--actions",
        &a.id,
        "--train-start",
        "2007-01-02",
        "--train-end",
        "2008-01-12",
        "--test-start",
        "2009-01-05",
        "--test-end",
        "2009-01-12",
        "--no-validation",
    ]);
    let id = submit["task_id"].as_str().unwrap();
    let wait = s.json(&["jobs", "wait", id, "--timeout", "10"]);
    assert_eq!(
        wait["results"]["details"]["resolved_folds"][0]["actual"]["validation"],
        Value::Null
    );
    let run = s.json(&[
        "research",
        "run",
        "--template",
        "recipe.json",
        "--market",
        &m.id,
        "--actions",
        &a.id,
        "--train-start",
        "2007-01-02",
        "--train-end",
        "2008-01-12",
        "--test-start",
        "2009-01-05",
        "--test-end",
        "2009-01-12",
        "--no-validation",
    ]);
    let done = s.json(&[
        "jobs",
        "wait",
        run["task_id"].as_str().unwrap(),
        "--timeout",
        "30",
    ]);
    assert_eq!(
        done["results"]["schema_version"],
        analytics_workflow::research::backtest::RESEARCH_SCHEMA_VERSION
    );
    assert_eq!(done["results"]["fold_reports"][0]["validation_rows"], 0);
    let invalid = s.cli(&[
        "research",
        "run",
        "--template",
        "recipe.json",
        "--market",
        &m.id,
        "--actions",
        &a.id,
        "--train-start",
        "2007-01-02",
    ]);
    assert_eq!(invalid.status.code(), Some(2));
}

#[test]
fn stopping_idle_service_then_immediately_reconnecting_does_not_parse_empty_json() {
    let s = Sandbox::new();
    let initial = s.json(&["service", "status"]);
    assert!(initial["active"].is_null());
    s.json(&["service", "stop"]);
    let after = s.json(&["service", "status"]);
    assert!(after["active"].is_null());
    assert_eq!(
        after["protocol_version"],
        analytics_workflow::service::PROTOCOL_VERSION
    );
}

#[test]
fn audited_execution_data_rejections_exit_three_before_training_with_diagnostics() {
    let s = Sandbox::new();
    let w = Workspace::open(&s.client.root).unwrap();
    let (m, a, mut t) = analytics_workflow::demo::create(&w).unwrap();
    t.event_check_policy = analytics_core::execution::EventCheckPolicy::Audited;
    let path = w.resolve(a.path);
    let mut book = analytics_storage::corporate_actions::ActionBundle::read(&path).unwrap();
    book.execution_issues
        .push(analytics_storage::execution_data::ExecutionIssue {
            id: String::new(),
            event_category: String::new(),
            evidence: Vec::new(),
            instrument: book.actions[0].instrument.clone(),
            announced: None,
            dates: analytics_core::DateRange {
                start: "2009-01-02".parse().unwrap(),
                end: "2010-01-12".parse().unwrap(),
            },
            code: "source_conflict".into(),
            message: "fixture conflict".into(),
        });
    std::fs::write(&path, serde_json::to_vec(&book).unwrap()).unwrap();
    let a = w.register(&path).unwrap();
    t.write(&w.resolve("execution-recipe.json")).unwrap();
    let accepted = s.json(&[
        "research",
        "run",
        "--template",
        "execution-recipe.json",
        "--market",
        &m.id,
        "--actions",
        &a.id,
    ]);
    let id = accepted["task_id"].as_str().unwrap();
    let wait = s.cli(&["jobs", "wait", id, "--timeout", "30"]);
    assert_eq!(wait.status.code(), Some(3));
    let records = w.runs().unwrap();
    let run = records.iter().find(|r| r.id.to_string() == id).unwrap();
    let root = w.resolve(&run.output);
    assert!(!root.join("training.json").is_file());
    assert!(!root.join("summary.json").exists());
    let readiness: Value =
        serde_json::from_slice(&std::fs::read(root.join("execution_readiness.json")).unwrap())
            .unwrap();
    assert_eq!(readiness["training_complete"], false);
    assert_eq!(readiness["backtest_complete"], false);
}

#[test]
fn removed_identity_cli_is_rejected_and_research_stages_are_explicit() {
    let s = Sandbox::new();
    assert_eq!(s.cli(&["identities", "list"]).status.code(), Some(2));
    for cmd in ["train", "backtest"] {
        let out = s.cli(&["research", cmd, "--help"]);
        assert!(out.status.success());
    }
    let w = Workspace::open(&s.client.root).unwrap();
    let (m, a, t) = analytics_workflow::demo::create(&w).unwrap();
    t.write(&w.resolve("stage-recipe.json")).unwrap();
    let accepted = s.json(&[
        "research",
        "train",
        "--template",
        "stage-recipe.json",
        "--market",
        &m.id,
        "--actions",
        &a.id,
    ]);
    let id = accepted["task_id"].as_str().unwrap();
    assert!(s.cli(&["jobs", "wait", id]).status.success());
    assert_eq!(s.json(&["jobs", "results", id])["status"], "trained");
    let accepted = s.json(&["research", "backtest", "--training", id, "--actions", &a.id]);
    let id = accepted["task_id"].as_str().unwrap();
    assert!(s.cli(&["jobs", "wait", id]).status.success());
    assert_eq!(
        s.json(&["jobs", "results", id])["schema_version"],
        analytics_workflow::research::backtest::RESEARCH_SCHEMA_VERSION
    );
}

#[test]
fn split_template_profiles_and_price_basis_flags_share_native_training() {
    let s = Sandbox::new();
    let demo = s.json(&["research", "demo"]);
    let id = demo["task_id"].as_str().unwrap();
    s.json(&["jobs", "wait", id, "--timeout", "30"]);
    let demo = s.json(&["jobs", "results", id]);
    let mut t: analytics_workflow::ResearchTemplate =
        serde_json::from_value(demo["details"]["demo_template"].clone()).unwrap();
    t.write(&s.client.root.join("workflow.json")).unwrap();
    s.json(&[
        "templates",
        "factors",
        "--template",
        "workflow.json",
        "--output",
        "features.json",
    ]);
    s.json(&[
        "templates",
        "target",
        "--template",
        "workflow.json",
        "--output",
        "universe.json",
    ]);
    s.json(&[
        "templates",
        "compose",
        "--workflow",
        "workflow.json",
        "--factors",
        "features.json",
        "--target",
        "universe.json",
        "--output",
        "composed.json",
    ]);
    assert_eq!(
        analytics_workflow::signature(&t).unwrap(),
        analytics_workflow::signature(
            &analytics_workflow::ResearchTemplate::read(&s.client.root.join("composed.json"))
                .unwrap()
        )
        .unwrap()
    );
    t.select_factor_set(analytics_core::research_quality::FactorSet::Alpha158);
    t.config.factor_ids = vec!["alpha158_001_KMID".into(), "alpha158_014_ROC5".into()];
    analytics_workflow::profiles::FactorProfile::from_template(&t)
        .write(&s.client.root.join("alpha.json"))
        .unwrap();
    let submitted = s.json(&[
        "research",
        "train",
        "--template",
        "composed.json",
        "--market",
        demo["data"]["id"].as_str().unwrap(),
        "--factor-profile",
        "alpha.json",
        "--price-basis",
        "provider",
    ]);
    let id = submitted["task_id"].as_str().unwrap();
    s.json(&["jobs", "wait", id, "--timeout", "30"]);
    let artifact = s.json(&["jobs", "results", id]);
    assert_eq!(artifact["factor_set"], "alpha158");
    assert_eq!(artifact["price_basis"], "provider");
    assert!(
        artifact["source_warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["code"] == "provider_adjustment_unreconciled")
    );
    assert_eq!(artifact["feature_order"], json!(t.config.factor_ids));
}

#[test]
fn source_priorities_and_refresh_preview_share_the_public_backend_without_credentials() {
    let s = Sandbox::new();
    let sources = s.json(&["data", "sources", "list"]);
    for role in ["quotes", "actions", "execution", "members", "calendar"] {
        let routes = &sources["result"]["results"]["details"]["policy"]["priorities"][role];
        if role == "execution" {
            assert!(routes.as_array().unwrap().is_empty());
        } else {
            assert_eq!(routes[0], "hithink");
        }
    }
    let w = Workspace::open(&s.client.root).unwrap();
    let (market, actions, recipe) = analytics_workflow::demo::create_canonical(&w).unwrap();
    recipe.write(&w.resolve("recipe.json")).unwrap();
    let result = s.json(&[
        "data",
        "refresh",
        "--template",
        "recipe.json",
        "--market",
        &market.id,
        "--actions",
        &actions.id,
        "--preview",
        "--provider",
        "tdxaidata",
    ]);
    assert_eq!(
        result["result"]["results"]["details"]["plan"]["schema_version"],
        1
    );
    assert_eq!(
        result["result"]["results"]["details"]["account"]["status"],
        "not_run"
    );
    let plan = &result["result"]["results"]["details"]["plan"];
    for security in plan["securities"].as_array().unwrap() {
        assert_eq!(security["quote_sources"], json!(["tdxaidata"]));
    }
    let out = s.cli(&["data", "sources", "configure", "--provider", "tdxaidata"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn local_credentials_are_private_and_survive_separate_cli_processes_without_os_prompts() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Stdio;
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let secret = "fixture-only-tdx-private-key";
    let mut child = Command::new(env!("CARGO_BIN_EXE_jfquant"))
        .env("HOME", &home)
        .args([
            "data",
            "sources",
            "configure",
            "--provider",
            "tdxaidata",
            "--key-stdin",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(secret.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["storage"], "private_local_file");
    #[cfg(target_os = "macos")]
    let root = home.join("Library/Application Support/JFQuant/credentials");
    #[cfg(not(target_os = "macos"))]
    let root = home.join(".jfquant/credentials");
    assert_eq!(
        std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(root.join("tdxaidata.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let removed = Command::new(env!("CARGO_BIN_EXE_jfquant"))
        .env("HOME", home)
        .args(["data", "sources", "authorize", "--provider", "tdxaidata"])
        .output()
        .unwrap();
    assert_eq!(removed.status.code(), Some(2));
}

#[test]
fn retired_action_commands_are_not_public_cli_interfaces() {
    use clap::Parser;
    for command in ["supplement", "retry"] {
        assert!(analytics_cli::Cli::try_parse_from(["jfquant", "data", command]).is_err());
    }
}

#[test]
fn custom_formula_library_mixed_evaluation_training_and_revision_receipts() {
    use analytics_core::research_quality::FactorSet;
    use analytics_factor::selection::{CustomFactorDefinition, FactorRef};
    let s = Sandbox::new();
    let write = |name: &str, value: &Value| {
        std::fs::write(s.client.root.join(name), serde_json::to_vec(value).unwrap()).unwrap()
    };
    write(
        "factor.json",
        &json!({"name":"20日动量","description":"CLI acceptance","formula":"CLOSE / GTJA_DELAY(CLOSE,20) - 1","id":null,"expected_revision":null}),
    );
    assert!(
        !s.json(&["factors", "operators", "--query", "QLIB_MEAN"])["operators"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        s.json(&["factors", "validate", "--definition", "factor.json"])["compiled"]["lookback"],
        21
    );
    let first = s.json(&["factors", "save", "--definition", "factor.json"]);
    let saved: CustomFactorDefinition =
        serde_json::from_value(first["saved_factor"].clone()).unwrap();
    let preview = s.json(&["factors", "preview", "--definition", "factor.json"]);
    let id = preview["task_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        s.client.wait(id, Some(15)).unwrap()["record"]["status"],
        "succeeded"
    );
    let market = "SELECT 'SH' || (600000+s)::VARCHAR AS instrument_id, DATE '2020-01-01' + d::INTEGER AS trade_date, (10.0+s+d*.01)::DOUBLE AS open, (open+1)::DOUBLE AS high, (open-1)::DOUBLE AS low, (open+.1+s*.01)::DOUBLE AS close, (1000.0+d)::DOUBLE AS volume, (volume*close)::DOUBLE AS amount, open AS adj_open, high AS adj_high, low AS adj_low, close AS adj_close, true AS is_trading, DATE '2000-01-01' AS listing_date FROM range(6) a(s) CROSS JOIN range(120) b(d)";
    let universe = "SELECT 'SH' || (600000+s)::VARCHAR AS instrument_id,'csi500' AS universe_id,DATE '2000-01-01' AS trade_date,true AS is_member FROM range(6) a(s)";
    s.json(&[
        "query",
        "export",
        "--sql",
        market,
        "--output",
        "market.parquet",
    ]);
    s.json(&[
        "query",
        "export",
        "--sql",
        universe,
        "--output",
        "universe.parquet",
    ]);
    let refs = vec![
        FactorRef::Custom {
            definition: Box::new(saved.clone()),
        },
        FactorRef::builtin(FactorSet::Alpha158, "alpha158_001_KMID"),
        FactorRef::builtin(FactorSet::Gtja191, "alpha_002"),
    ];
    let ids: Vec<String> = refs.iter().map(|r| r.id().to_owned()).collect();
    let input = analytics_workflow::research::pipeline::PipelineRequest {
        factor_refs: refs.clone(),
        factor_set: FactorSet::Gtja191,
        market_path: "market.parquet".into(),
        universe_path: "universe.parquet".into(),
        universe_id: "csi500".into(),
        date_range: analytics_core::DateRange {
            start: "2020-01-01".parse().unwrap(),
            end: "2020-04-30".parse().unwrap(),
        },
        factor_ids: ids.clone(),
        output_dir: "unused".into(),
        training: Default::default(),
    };
    let check = analytics_workflow::input_check::CheckRequest {
        expected_input_hash: None,
        expected_source_stamp: None,
        input: input.clone().into(),
        horizon: 5,
        policy: Default::default(),
    };
    write("check.json", &serde_json::to_value(check).unwrap());
    let checked = s.json(&["factors", "check", "--request", "check.json"]);
    let check_id = checked["task_id"].as_str().unwrap().parse().unwrap();
    let checked = s.client.wait(check_id, Some(15)).unwrap();
    assert_eq!(checked["results"]["ready"], true);
    assert_eq!(checked["results"]["factors"].as_array().unwrap().len(), 3);
    assert_eq!(checked["results"]["artifact"]["cache_hit"], false);
    assert!(
        !s.client
            .root
            .join(format!(".jfquant/runs/{check_id}/model.txt"))
            .exists()
    );
    let evaluation = analytics_workflow::factor_evaluation::EvaluationRequest {
        policy: Default::default(),
        input: input.clone().into(),
        horizon: 5,
        quantiles: 3,
        min_samples: 3,
    };
    write(
        "evaluation.json",
        &serde_json::to_value(evaluation).unwrap(),
    );
    let eval = s.json(&["factors", "evaluate", "--request", "evaluation.json"]);
    let eval_id = eval["task_id"].as_str().unwrap().parse().unwrap();
    let result = s.client.wait(eval_id, Some(15)).unwrap();
    assert_eq!(result["record"]["status"], "succeeded");
    let report: Value = serde_json::from_slice(
        &std::fs::read(
            s.client
                .root
                .join(format!(".jfquant/runs/{eval_id}/factor-evaluation.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(report["factors"][0]["factor_id"], saved.id);
    assert_eq!(report["computation_receipt"]["cache_hit"], true);
    assert_eq!(report["factors"].as_array().unwrap().len(), 3);
    write("pipeline.json", &serde_json::to_value(input).unwrap());
    let train = s.json(&["factors", "run", "--request", "pipeline.json"]);
    let train_id = train["task_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        s.client.wait(train_id, Some(15)).unwrap()["record"]["status"],
        "succeeded"
    );
    let manifest = s
        .client
        .root
        .join(format!(".jfquant/runs/{train_id}/manifest.json"));
    let before = std::fs::read(&manifest).unwrap();
    let m: Value = serde_json::from_slice(&before).unwrap();
    assert_eq!(
        m["features"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap().to_string())
            .collect::<Vec<_>>(),
        ids
    );
    assert_eq!(m["factor_refs"][0]["definition"]["revision"], 1);
    analytics_workflow::research::load_manifest(&manifest).unwrap();
    write(
        "factor.json",
        &json!({"id":saved.id,"expected_revision":1,"name":"10日动量","description":"new revision","formula":"CLOSE / GTJA_DELAY(CLOSE,10) - 1"}),
    );
    assert_eq!(
        s.json(&["factors", "save", "--definition", "factor.json"])["saved_factor"]["revision"],
        2
    );
    s.json(&["factors", "archive", "--id", &saved.id]);
    analytics_workflow::research::load_manifest(&manifest).unwrap();
    assert_eq!(std::fs::read(manifest).unwrap(), before);
    let library = s.json(&["factors", "library"]);
    assert_eq!(library["factors"][0]["archived"], true);
}

#[test]
fn independent_fit_models_preview_and_new_pool_backtest_share_service() {
    use analytics_workflow::research::backtest::independent::{
        BacktestPlan, FitWindow, ModelSelection, TrainingPlan, TrainingSchedule,
    };
    let s = Sandbox::new();
    let w = Workspace::open(&s.client.root).unwrap();
    let (m, a, t) = analytics_workflow::testing::synthetic(&w);
    let mut fit = TrainingPlan::from_template(&t);
    fit.schedule = TrainingSchedule::Single {
        window: FitWindow {
            train: analytics_core::DateRange {
                start: "2008-01-02".parse().unwrap(),
                end: "2008-01-12".parse().unwrap(),
            },
            validation: None,
        },
    };
    let path = w.resolve("fit.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&fit).unwrap()).unwrap();
    let submitted = s.json(&[
        "research",
        "fit",
        "--plan",
        path.to_str().unwrap(),
        "--market",
        &m.id,
        "--actions",
        &a.id,
    ]);
    let training = submitted["task_id"].as_str().unwrap();
    let done = s.json(&["jobs", "wait", training, "--timeout", "30"]);
    assert_eq!(done["record"]["status"], "succeeded", "{done}");
    assert_eq!(done["results"]["schema_version"], 5);
    let models = s.json(&["research", "models", "--training", training]);
    let listed = s.json(&[
        "jobs",
        "wait",
        models["task_id"].as_str().unwrap(),
        "--timeout",
        "30",
    ]);
    assert_eq!(listed["record"]["status"], "succeeded", "{listed}");
    assert_eq!(
        listed["results"]["details"]["catalogue"]["models"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let pool = w
        .save_pool(analytics_core::pool::PoolDefinition {
            schema_version: 3,
            name: "B only".into(),
            coverage: analytics_core::DateRange {
                start: "2007-01-02".parse().unwrap(),
                end: "2010-01-12".parse().unwrap(),
            },
            expression: analytics_core::pool::PoolExpression::Symbols {
                instruments: ["SH600001".into()].into(),
            },
            adjustments: vec![],
        })
        .unwrap();
    let plan = BacktestPlan {
        schema_version: 4,
        training: training.parse().unwrap(),
        models: ModelSelection::Rolling,
        pool_version: pool.id,
        range: analytics_core::DateRange {
            start: "2010-01-02".parse().unwrap(),
            end: "2010-01-12".parse().unwrap(),
        },
        market: m.id,
        actions: a.id,
        benchmark: t.benchmark.clone(),
        quality_policy: t.quality_policy.clone(),
        execution: t.execution_settings(),
        event_check_policy: Default::default(),
    };
    let path = w.resolve("B.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&plan).unwrap()).unwrap();
    let preview = s.json(&[
        "research",
        "backtest-inputs",
        "--plan",
        path.to_str().unwrap(),
    ]);
    let preview = s.json(&[
        "jobs",
        "wait",
        preview["task_id"].as_str().unwrap(),
        "--timeout",
        "30",
    ]);
    assert_eq!(preview["record"]["status"], "succeeded", "{preview}");
    assert_eq!(
        preview["results"]["details"]["training_data_required"],
        false
    );
    let backtest = s.json(&[
        "research",
        "model-backtest",
        "--plan",
        path.to_str().unwrap(),
    ]);
    let backtest = s.json(&[
        "jobs",
        "wait",
        backtest["task_id"].as_str().unwrap(),
        "--timeout",
        "30",
    ]);
    assert_eq!(backtest["record"]["status"], "succeeded", "{backtest}");
    assert_eq!(
        backtest["results"]["schema_version"],
        analytics_workflow::research::backtest::RESEARCH_SCHEMA_VERSION
    );
}

#[test]
fn evidence_commands_share_status_and_persist_nonpublishing_scaffolds() {
    let s = Sandbox::new();
    let w = Workspace::open(&s.client.root).unwrap();
    let (_, actions, _) = analytics_workflow::demo::create_canonical(&w).unwrap();
    let sub = s.json(&[
        "data",
        "evidence",
        "status",
        "--actions",
        &actions.id,
        "--instrument",
        "SH600000",
    ]);
    s.json(&[
        "jobs",
        "wait",
        sub["task_id"].as_str().unwrap(),
        "--timeout",
        "30",
    ]);
    let result = s.json(&["jobs", "results", sub["task_id"].as_str().unwrap()]);
    assert_eq!(result["details"]["actions"], actions.id);
    assert!(result["details"]["entries"].is_array());
    assert_eq!(result["details"]["report_kind"], "evidence_inventory");
    assert!(result["details"]["current"].is_null());
    let mut owner = w
        .begin(Uuid::new_v4(), "terminal dependency fixture", json!({}))
        .unwrap();
    std::fs::write(w.resolve(&owner.output).join("execution-dependencies.json"),serde_json::to_vec(&json!({"schema_version":2,"policy":"audited","scope":{"SH600000":{"start":"2009-01-02","end":"2009-01-02"}}})).unwrap()).unwrap();
    w.finish(&mut owner, analytics_workflow::RunStatus::Succeeded, None)
        .unwrap();
    let task_status = s.json(&[
        "data",
        "evidence",
        "status",
        "--actions",
        &actions.id,
        "--task",
        &owner.id.to_string(),
    ]);
    s.json(&[
        "jobs",
        "wait",
        task_status["task_id"].as_str().unwrap(),
        "--timeout",
        "30",
    ]);
    let result = s.json(&["jobs", "results", task_status["task_id"].as_str().unwrap()]);
    assert_eq!(result["details"]["report_kind"], "task_dependencies");
    assert_eq!(result["details"]["current"]["ready"], true);
    assert_eq!(result["details"]["current"]["task"], owner.id.to_string());
    let root = w.resolve("selected evidence");
    std::fs::create_dir_all(root.join("raw")).unwrap();
    std::fs::write(
        root.join("announcements.json"),
        serde_json::to_vec(&json!({"schema_version":2,"documents":[],"receipts":{}})).unwrap(),
    )
    .unwrap();
    let before = w.datasets().unwrap().len();
    let sub = s.json(&[
        "data",
        "evidence",
        "draft",
        "--actions",
        &actions.id,
        "--instrument",
        "SH600000",
        "--evidence-manifest",
        "selected evidence/announcements.json",
    ]);
    s.json(&[
        "jobs",
        "wait",
        sub["task_id"].as_str().unwrap(),
        "--timeout",
        "30",
    ]);
    let result = s.json(&["jobs", "results", sub["task_id"].as_str().unwrap()]);
    assert_eq!(result["details"]["validation"]["ready"], false);
    assert!(std::path::Path::new(result["details"]["request_path"].as_str().unwrap()).is_file());
    assert_eq!(w.datasets().unwrap().len(), before);
    for command in ["capture", "recover", "retry", "status", "draft"] {
        assert!(
            s.cli(&["data", "evidence", command, "--help"])
                .status
                .success()
        );
    }
}

#[test]
fn index_catalogue_is_offline_and_exposes_missing_routes_without_opening_a_workspace() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("must-not-be-created");
    let output = Command::new(env!("CARGO_BIN_EXE_jfquant"))
        .arg("--workspace")
        .arg(&root)
        .arg("indices")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let catalogue: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(catalogue["schema_version"], 1);
    let rows = catalogue["indices"].as_array().unwrap();
    assert!(
        rows.iter()
            .any(|i| i["id"] == "csi1000" && i["symbol"] == "SH000852")
    );
    let msci = rows.iter().find(|i| i["id"] == "msci_a50").unwrap();
    assert_eq!(msci["official_code"], "746059");
    assert_eq!(msci["quote_route"], "unavailable");
    assert!(msci["symbol"].is_null());
    assert!(!root.exists());
}

#[test]
fn runtime_indices_roundtrip_without_compilation_and_queries_do_not_create_tasks() {
    let s = Sandbox::new();
    let mut config = analytics_core::indices::builtin_config("csi1000").unwrap();
    config.definition.id = "cli_index".into();
    config.definition.symbol = None;
    config.definition.official_code = "CLI001".into();
    config.data_key = "IDX_cli_index".into();
    config.mappings = std::collections::BTreeMap::from([("hithink".into(), "CLI001.CSI".into())]);
    std::fs::write(
        s.client.root.join("input.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    let valid = s.json(&["indices", "validate", "input.json"]);
    assert_eq!(valid["valid"], true);
    assert_eq!(valid["data_ready"], false);
    let imported = s.json(&["indices", "import", "input.json"]);
    let task = imported["submission"]["task_id"].as_str().unwrap();
    let result = s.json(&["jobs", "results", task]);
    let version = result["details"]["index_version"]["id"].as_str().unwrap();
    let w = Workspace::open(&s.client.root).unwrap();
    let before = w.runs().unwrap().len();
    let listed = s.json(&["indices", "list"]);
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["id"] == version)
    );
    assert_eq!(
        s.json(&["indices", "show", version])["config"]["data_key"],
        config.data_key
    );
    assert_eq!(w.runs().unwrap().len(), before);
    s.json(&["indices", "export", version, "output.json"]);
    let exported: analytics_core::indices::IndexConfig =
        serde_json::from_slice(&std::fs::read(s.client.root.join("output.json")).unwrap()).unwrap();
    assert_eq!(exported, config);
    let template = s.json(&["templates", "default", "--index-version", version]);
    assert_eq!(template["benchmark"], "IDX_cli_index");
    assert_eq!(template["pool_version"], "index:cli_index");
    let mut portable: Value = serde_json::to_value(&config).unwrap();
    portable["script"] = "not accepted".into();
    std::fs::write(
        s.client.root.join("invalid.json"),
        serde_json::to_vec(&portable).unwrap(),
    )
    .unwrap();
    assert!(
        !s.cli(&["indices", "validate", "invalid.json"])
            .status
            .success()
    );
}

#[test]
fn combined_cli_uses_frozen_targets_and_the_shared_account_verifier() {
    let s = Sandbox::new();
    let w = Workspace::open(&s.client.root).unwrap();
    let (m, a, t) = analytics_workflow::demo::create_canonical(&w).unwrap();
    let mut ids = Vec::new();
    for _ in 0..2 {
        let context = analytics_core::task::TaskContext::default();
        let request = WorkflowRequest::Research {
            template: Box::new(t.clone()),
            market: m.id.clone(),
            actions: Some(a.id.clone()),
            audit_only: false,
        };
        let mut run = w
            .begin(
                context.id,
                "allocation source",
                analytics_workflow::snapshot(&w, &request).unwrap(),
            )
            .unwrap();
        analytics_workflow::execute(&w, &request, &run, &context).unwrap();
        w.finish(&mut run, analytics_workflow::RunStatus::Succeeded, None)
            .unwrap();
        ids.push(run.id.to_string());
    }
    let mut plan = s.json(&[
        "research",
        "combined-plan",
        "--sources",
        &ids.join(","),
        "--start",
        "2010-01-02",
        "--end",
        "2010-01-12",
    ]);
    plan["execution"] = serde_json::to_value(t.execution_settings()).unwrap();
    std::fs::write(
        w.resolve("combined.json"),
        serde_json::to_vec_pretty(&plan).unwrap(),
    )
    .unwrap();
    let check = s.json(&["research", "combined", "--plan", "combined.json", "--check"]);
    let check = s.json(&[
        "jobs",
        "wait",
        check["task_id"].as_str().unwrap(),
        "--timeout",
        "30",
    ]);
    assert_eq!(check["record"]["status"], "succeeded");
    let submitted = s.json(&["research", "combined", "--plan", "combined.json"]);
    let task = submitted["task_id"].as_str().unwrap();
    let done = s.json(&["jobs", "wait", task, "--timeout", "30"]);
    assert_eq!(done["results"]["kind"], "combined");
    assert_eq!(
        s.json(&["research", "verify", "--task", task])["status"],
        "consistent"
    );
    let compared = s.json(&["research", "compare-allocations", "--tasks", &ids.join(",")]);
    let compared = s.json(&[
        "jobs",
        "wait",
        compared["task_id"].as_str().unwrap(),
        "--timeout",
        "30",
    ]);
    assert_eq!(compared["record"]["status"], "succeeded");
}
