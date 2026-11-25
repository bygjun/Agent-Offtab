use super::*;
use serde_json::json;
use std::env;

const OLD: &str = "00000000-0000-0000-0000-000000000001";
const FINISHED: &str = "00000000-0000-0000-0000-000000000002";
const RUNNING: &str = "00000000-0000-0000-0000-000000000003";
const NOW: i64 = 1_790_000_010_000;

struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Self {
        let path = env::temp_dir().join(format!("session-desk-test-{}-{name}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn watch(provider: Provider, id: &str) -> Watched {
    Watched {
        provider,
        id: id.to_owned(),
        native_id: None,
        has_transcript: false,
        hidden_at: None,
    }
}

fn compact_scope(watched: &[Watched]) -> ScanScope<'_> {
    static EMPTY_SIGNALS: std::sync::LazyLock<HashMap<String, Signal>> =
        std::sync::LazyLock::new(HashMap::new);
    static EMPTY_ACTIVITY: std::sync::LazyLock<Activity> =
        std::sync::LazyLock::new(Activity::default);
    ScanScope {
        mode: Mode::Compact,
        history_since: NOW - WEEK_MS,
        work_since: NOW - 5_000,
        watched: watched
            .iter()
            .map(|item| (item.id.as_str(), item))
            .collect(),
        signals: &EMPTY_SIGNALS,
        activity: &EMPTY_ACTIVITY,
    }
}

#[test]
fn codex_new_live_and_short_turns_are_found_while_old_rows_stay_hidden() {
    let dir = Fixture::new("codex");
    let db = Connection::open(dir.0.join("state_1.sqlite")).unwrap();
    db.execute_batch(
        "CREATE TABLE threads (id TEXT, title TEXT, cwd TEXT, rollout_path TEXT,
        updated_at INTEGER, archived INTEGER DEFAULT 0, source TEXT DEFAULT 'appServer')",
    )
    .unwrap();
    let live = dir.0.join(format!("rollout-{RUNNING}.jsonl"));
    let open_logs = HashSet::from([live]);
    for (id, at, event) in [
        (OLD, NOW - 30_000, "task_complete"),
        (FINISHED, NOW - 1_000, "task_complete"),
        (RUNNING, NOW - 60_000, "task_started"),
    ] {
        let path = dir.0.join(format!("rollout-{id}.jsonl"));
        fs::write(&path, format!("{}\n{}\n", json!({
            "type": "event_msg", "timestamp": at - 200, "payload": {"type":"user_message", "message":"안녕"}
        }), json!({"type":"event_msg", "timestamp":at, "payload":{"type":event}}))).unwrap();
        db.execute("INSERT INTO threads (id, title, cwd, rollout_path, updated_at) VALUES (?1, ?1, '/test', ?2, ?3)",
            rusqlite::params![id, path.to_str().unwrap(), at / 1000]).unwrap();
    }
    let mut collector = Collector::default();
    let mut partial = false;
    let (_, rows, _) = collector
        .codex(&dir.0, &compact_scope(&[]), Some(&open_logs), &mut partial)
        .unwrap();
    assert!(!partial);
    assert_eq!(rows.len(), 2);
    let ongoing = rows.iter().find(|row| row.id == RUNNING).unwrap();
    assert_eq!(ongoing.active, Some(true));
    let completed = rows.iter().find(|row| row.id == FINISHED).unwrap();
    assert_eq!(completed.active, Some(false));
    assert!(completed.recently_active);
    assert_eq!(completed.preview, "안녕");
    assert_eq!(completed.last_user_at, Some(NOW - 1_200));

    db.execute("UPDATE threads SET archived = 1 WHERE id = ?", [FINISHED])
        .unwrap();
    db.execute("DELETE FROM threads WHERE id = ?", [OLD])
        .unwrap();
    let watched = [
        watch(Provider::Codex, FINISHED),
        watch(Provider::Codex, OLD),
        watch(Provider::Codex, RUNNING),
    ];
    let (_, rows, removed) = collector
        .codex(&dir.0, &compact_scope(&watched), None, &mut partial)
        .unwrap();
    assert!(partial);
    assert_eq!(
        removed,
        HashSet::from([OLD.to_owned(), FINISHED.to_owned()])
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, RUNNING);
}

#[test]
fn claude_busy_pid_and_completed_message_are_found_but_pid_reuse_is_ignored() {
    let dir = Fixture::new("claude");
    let project = dir.0.join("projects/sample");
    let runtime = dir.0.join("sessions");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir(&runtime).unwrap();
    for (id, at) in [(OLD, NOW - 30_000), (FINISHED, NOW - 1_000)] {
        fs::write(
            project.join(format!("{id}.jsonl")),
            format!(
                "{}\n",
                json!({
                    "type":"user", "timestamp":at, "message":{"content":"안녕"}, "cwd":"/test"
                })
            ),
        )
        .unwrap();
    }
    for (pid, id, started) in [(42, RUNNING, NOW - 10_000), (43, OLD, NOW - 60_000)] {
        fs::write(
            runtime.join(format!("{pid}.json")),
            json!({
                "pid":pid, "sessionId":id, "startedAt":started, "status":"busy", "name":"실행 중"
            })
            .to_string(),
        )
        .unwrap();
    }
    let desktop = dir.0.join("desktop/account/workspace");
    fs::create_dir_all(&desktop).unwrap();
    fs::write(
        desktop.join(format!("local_{FINISHED}.json")),
        json!({
            "cliSessionId":FINISHED, "sessionId":format!("local_{OLD}"), "title":"작업 제목"
        })
        .to_string(),
    )
    .unwrap();
    let mut collector = Collector::default();
    let processes = HashMap::from([(42, NOW - 11_000), (43, NOW - 1_000)]);
    let (_, rows, _) = collector
        .claude(
            &dir.0,
            &dir.0.join("desktop"),
            &compact_scope(&[]),
            Some(&processes),
            &mut false,
        )
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows.iter().find(|row| row.id == RUNNING).unwrap().active,
        Some(true)
    );
    let completed = rows.iter().find(|row| row.id == FINISHED).unwrap();
    assert_eq!(completed.active, Some(false));
    assert!(completed.recently_active);
    assert_eq!(completed.title, "작업 제목");
    assert_eq!(completed.native_id, Some(format!("local_{OLD}")));
}

#[test]
fn claude_deleted_desktop_metadata_hides_orphan_logs_and_preserves_live_cli() {
    let dir = Fixture::new("claude-delete");
    let project = dir.0.join("projects/sample");
    let desktop = dir.0.join("desktop");
    let workspace = desktop.join("account/workspace");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&workspace).unwrap();
    let native_id = format!("local_{OLD}");
    let record = workspace.join(format!("{native_id}.json"));
    fs::write(
        &record,
        json!({"cliSessionId":OLD, "sessionId":native_id, "createdAt":NOW}).to_string(),
    )
    .unwrap();
    for (id, entrypoint) in [(OLD, "claude-desktop"), (RUNNING, "cli")] {
        fs::write(
            project.join(format!("{id}.jsonl")),
            format!(
                "{}\n",
                json!({
                    "type":"user", "timestamp":NOW, "entrypoint":entrypoint,
                    "message":{"content":"확인해줘"}
                })
            ),
        )
        .unwrap();
    }
    let mut collector = Collector::default();
    let mut watch_desktop = watch(Provider::Claude, OLD);
    watch_desktop.native_id = Some(native_id.clone());
    let mut watch_cli = watch(Provider::Claude, RUNNING);
    watch_cli.has_transcript = true;
    let watched = [watch_desktop, watch_cli];
    let processes = HashMap::new();
    let scan = |collector: &mut Collector| {
        collector
            .claude(
                &dir.0,
                &desktop,
                &compact_scope(&watched),
                Some(&processes),
                &mut false,
            )
            .unwrap()
    };
    let (_, rows, removed) = scan(&mut collector);
    assert_eq!(rows.len(), 2);
    assert!(removed.is_empty());
    fs::write(
        &record,
        json!({
            "cliSessionId":FINISHED, "sessionId":native_id, "priorCliSessionIds":[OLD]
        })
        .to_string(),
    )
    .unwrap();
    let (_, rows, removed) = scan(&mut collector);
    assert!(removed.is_empty());
    assert!(rows.iter().any(|row| row.id == FINISHED));
    assert!(rows.iter().all(|row| row.id != OLD));

    fs::write(&record, "broken JSON").unwrap();
    let (_, rows, removed) = scan(&mut collector);
    assert!(removed.is_empty());
    assert_eq!(rows.len(), 1);
    fs::remove_file(&record).unwrap();
    let (_, rows, removed) = scan(&mut collector);
    assert_eq!(removed, HashSet::from([OLD.to_owned()]));
    assert_eq!(rows[0].id, RUNNING);
    fs::remove_file(project.join(format!("{RUNNING}.jsonl"))).unwrap();
    let (_, rows, removed) = scan(&mut collector);
    assert!(rows.is_empty());
    assert_eq!(removed, HashSet::from([OLD.to_owned(), RUNNING.to_owned()]));
}

#[test]
fn tracks_real_input_independently_of_assistant_and_background_messages() {
    use std::io::Write;
    let dir = Fixture::new("human-input");
    let path = dir.0.join("claude.jsonl");
    let mut file = fs::File::create(&path).unwrap();
    for row in [
        json!({"type":"user","timestamp":100,"message":{"content":"확인해줘"}}),
        json!({"type":"user","timestamp":200,"turnOrigin":"host_synthetic","message":{"content":"재개"}}),
        json!({"type":"user","timestamp":210,"message":{"content":[{"type":"tool_result","content":"자동 결과"}]}}),
        json!({"type":"user","timestamp":220,"message":{"content":"<task-notification>완료</task-notification>"}}),
        json!({"type":"assistant","timestamp":300,"message":{"content":"처리했습니다"}}),
    ] {
        writeln!(file, "{row}").unwrap();
    }
    let mut cache = TranscriptCache::default();
    let log = cache.read(&path, Provider::Claude, false).unwrap();
    assert_eq!(log.last_user_at, Some(100));
    assert_eq!(log.latest.unwrap().at, 300);
    writeln!(
        file,
        "{}",
        json!({
            "type":"user","timestamp":400,"turnOrigin":"human",
            "message":{"content":[{"type":"image","source":{"type":"base64"}}]}
        })
    )
    .unwrap();
    assert_eq!(
        cache
            .read(&path, Provider::Claude, false)
            .unwrap()
            .last_user_at,
        Some(400)
    );
}

#[test]
fn claude_desktop_answer_card_waits_after_the_turn_and_clears_on_reply() {
    let dir = Fixture::new("desktop-answer-card");
    let desktop = dir.0.join("desktop");
    let workspace = desktop.join("account/workspace");
    fs::create_dir_all(&workspace).unwrap();
    let file = workspace.join(format!("local_{OLD}.json"));
    let other = workspace.join(format!("local_{FINISHED}.json"));
    let card_at = NOW - 120_000;
    let mut card = json!({
        "cliSessionId":OLD, "sessionId":format!("local_{OLD}"), "title":"선택 대기",
        "lastActivityAt":card_at, "lastTurnEdgeAt":card_at, "latestUserFrameAt":card_at-1000,
        "turnWrapUp":{"description":"작업 요약", "needsUser":"어느 안으로 진행할까요?"},
    });
    fs::write(&file, card.to_string()).unwrap();
    fs::write(
        other,
        json!({
            "cliSessionId":FINISHED, "sessionId":format!("local_{FINISHED}"), "title":"완료",
            "lastActivityAt":card_at, "lastTurnEdgeAt":card_at, "latestUserFrameAt":card_at-1000,
            "turnWrapUp":{"description":"작업 요약"},
        })
        .to_string(),
    )
    .unwrap();
    let mut collector = Collector::default();
    let processes = HashMap::new();
    let rows = |collector: &mut Collector, watched: &[Watched]| {
        collector
            .claude(
                &dir.0,
                &desktop,
                &compact_scope(watched),
                Some(&processes),
                &mut false,
            )
            .unwrap()
            .1
    };

    let pending = rows(&mut collector, &[]);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, OLD);
    assert_eq!(pending[0].active, Some(false));
    assert_eq!(pending[0].waiting, Some(Waiting::Input));
    assert_eq!(pending[0].waiting_at, Some(card_at));

    card["latestUserFrameAt"] = json!(card_at + 1000);
    card["lastActivityAt"] = json!(card_at + 1000);
    card["title"] = json!("선택 대기에서 응답 완료");
    fs::write(&file, card.to_string()).unwrap();
    let answered = rows(&mut collector, &[watch(Provider::Claude, OLD)]);
    assert_eq!(answered.len(), 1);
    assert_eq!(answered[0].waiting, None);
    assert!(rows(&mut collector, &[]).is_empty());
}

#[test]
fn claude_desktop_detects_a_rendered_choice_with_a_summary_only_in_metadata() {
    let dir = Fixture::new("desktop-rendered-card");
    let desktop = dir.0.join("desktop");
    let workspace = desktop.join("account/workspace");
    let projects = dir.0.join("projects/sample");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&projects).unwrap();
    let metadata = workspace.join(format!("local_{OLD}.json"));
    let transcript = projects.join(format!("{OLD}.jsonl"));
    let turn_end = NOW - 120_000;
    let card = |id, shown, at| {
        [
            json!({"type":"assistant","timestamp":at,"message":{"content":[{"type":"tool_use","name":"mcp__ccd_turn__wrap_up","id":id,"input":{"ask":"어느 안으로 진행할까요?","actions":[{"label":"A"},{"label":"B"}]}}]}}),
            json!({"type":"user","timestamp":at+1,"message":{"content":[{"type":"tool_result","tool_use_id":id,"content":[{"type":"text","text":if shown {"Shown in the next-step box. Your closing message follows."} else {"The app draws no box for this call."}}]}]}}),
        ]
    };
    let write_transcript = |outcome: &[Value]| {
        let events = card("first", true, turn_end - 3_000).into_iter().chain(outcome.iter().cloned()).chain([
            json!({"type":"assistant","timestamp":turn_end,"message":{"stop_reason":"end_turn","content":[{"type":"text","text":"작업을 확인했습니다."}]}}),
        ]);
        fs::write(
            &transcript,
            events
                .map(|event| event.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
    };
    let mut data = json!({
        "cliSessionId":OLD, "sessionId":format!("local_{OLD}"), "title":"선택 카드",
        "lastActivityAt":turn_end, "lastTurnEdgeAt":turn_end, "latestUserFrameAt":turn_end-5_000,
        "turnBoxMounted":true, "turnWrapUp":{"description":"작업 요약"},
    });
    fs::write(&metadata, data.to_string()).unwrap();
    write_transcript(&[]);
    let mut collector = Collector::default();
    let processes = HashMap::new();
    let rows = |collector: &mut Collector| {
        collector
            .claude(
                &dir.0,
                &desktop,
                &compact_scope(&[watch(Provider::Claude, OLD)]),
                Some(&processes),
                &mut false,
            )
            .unwrap()
            .1
    };
    let pending = rows(&mut collector);
    assert_eq!(pending[0].active, Some(false));
    assert_eq!(pending[0].waiting, Some(Waiting::Input));
    assert_eq!(pending[0].waiting_at, Some(turn_end));

    write_transcript(&card("second", false, turn_end - 2_000));
    assert_eq!(
        rows(&mut collector)[0].waiting,
        None,
        "a newer call that renders no card supersedes the old one"
    );
    write_transcript(&[]);
    data["latestUserFrameAt"] = json!(turn_end + 1);
    fs::write(&metadata, data.to_string()).unwrap();
    assert_eq!(
        rows(&mut collector)[0].waiting,
        None,
        "the latest user frame clears a card before transcript updates arrive"
    );

    data["latestUserFrameAt"] = json!(turn_end - 5_000);
    data["turnWrapUp"] = Value::Null;
    fs::write(&metadata, data.to_string()).unwrap();
    assert_eq!(
        rows(&mut collector)[0].waiting,
        None,
        "a cleared wrap-up is not re-created from old transcript events"
    );
}

#[test]
fn completed_parent_remains_in_compact_scope_when_a_codex_child_is_running() {
    let dir = Fixture::new("child");
    let db = Connection::open(dir.0.join("state_1.sqlite")).unwrap();
    db.execute_batch("CREATE TABLE threads (id TEXT, title TEXT, cwd TEXT, rollout_path TEXT, updated_at INTEGER,
        archived INTEGER DEFAULT 0, source TEXT, attached_thread_parent_id TEXT)").unwrap();
    let parent = dir.0.join("parent.jsonl");
    let child = dir.0.join("child.jsonl");
    for (id, path, event, parent_id) in [
        (OLD, &parent, "task_complete", None),
        (RUNNING, &child, "task_started", Some(OLD)),
    ] {
        fs::write(
            path,
            format!(
                "{}\n",
                json!({"type":"event_msg", "timestamp":NOW-100_000, "payload":{"type":event}})
            ),
        )
        .unwrap();
        db.execute(
            "INSERT INTO threads VALUES (?1, ?1, '/test', ?2, ?3, 0, 'appServer', ?4)",
            rusqlite::params![
                id,
                path.to_str().unwrap(),
                (NOW - 100_000) / 1000,
                parent_id
            ],
        )
        .unwrap();
    }
    let rows = Collector::default()
        .codex(
            &dir.0,
            &compact_scope(&[]),
            Some(&HashSet::from([child])),
            &mut false,
        )
        .unwrap()
        .1;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, OLD);
    assert_eq!(rows[0].active, Some(false));
    assert_eq!(rows[0].background, 1);
}
