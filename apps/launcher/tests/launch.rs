//! U5: launch and log tail, with a fake child process (`fake-layer`) standing in for `rebuild.exe`.
//! The real proofs (`gputest --bare`) are not run here: helper lanes never run the GPU proofs.

use rebuild_config::{Config, Value};
use rebuild_launcher::{model::Model, play};
use std::time::{Duration, Instant};

fn temp(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("rebuild-launcher-launch-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(tag: &str, extra: &[&str], proof: bool) -> (std::process::ExitStatus, Vec<String>) {
    let home = temp(tag);
    let cfg_path = home.join("rebuild.toml");
    let log = home.join("frame-line.log");
    let mut m = Model::new(Config::parse("").unwrap());
    m.config.profile = "launch-test".into();
    m.config.set("display.width", Value::Int(2560)).unwrap();
    m.save(&cfg_path).unwrap();

    let exe = std::path::PathBuf::from(env!("CARGO_BIN_EXE_fake-layer"));
    let extra: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
    let mut r = if proof {
        let mut args = vec!["--config".to_string(), cfg_path.display().to_string()];
        args.extend(extra);
        play::Running::start_proof(&exe, &args, &log).unwrap()
    } else {
        play::Running::start(&exe, &cfg_path, &log, &extra).unwrap()
    };
    let mut tail = play::LogTail::new(log);
    let mut lines = Vec::new();
    let t = Instant::now();
    // the UI's loop: poll the child and the tail until the child exits, then drain the tail
    let status = loop {
        lines.extend(tail.poll());
        if let Some(s) = r.poll() {
            lines.extend(tail.poll());
            break s;
        }
        assert!(t.elapsed() < Duration::from_secs(30), "the child never exited");
        std::thread::sleep(Duration::from_millis(5));
    };
    (status, lines)
}

#[test]
fn launch_passes_the_config_and_the_log_is_tailed() {
    let (status, lines) = run("ok", &["--frames", "8"], false);
    assert!(status.success());
    assert_eq!(lines.len(), 9, "{lines:#?}");
    assert_eq!(lines[0], "config profile=launch-test problems=0 width=2560");
    for (i, l) in lines[1..].iter().enumerate() {
        assert!(l.starts_with(&format!("frame {i} |")), "{l}");
    }
}

#[test]
fn a_failing_child_reports_its_exit_code() {
    let (status, lines) = run("fail", &["--frames", "1", "--exit", "3"], false);
    assert_eq!(status.code(), Some(3));
    assert_eq!(lines.len(), 2);
}

#[test]
fn a_missing_layer_is_an_error_not_a_panic() {
    let home = temp("missing");
    let r = play::Running::start(&home.join("no-such.exe"), &home.join("rebuild.toml"), &home.join("log"), &[]);
    assert!(r.is_err());
}

/// The proof path: a binary run as it is, its standard output shown through the same tail.
#[test]
fn a_proof_binary_output_is_tailed() {
    let (status, lines) = run("proof", &["--frames", "3"], true);
    assert!(status.success());
    assert_eq!(lines.len(), 4, "{lines:#?}");
    assert!(lines[0].starts_with("config profile=launch-test problems=0"));
}
/// L6: a launch in a session folder. The child gets `REBUILD_SESSION_DIR` and `REBUILD_FRAME_LOG` (inside
/// the folder) and the matrix's environment; the launcher records the start and the end in
/// `session.toml`; what the child leaves (summary, crash report) is listed with the session.
#[test]
fn a_launch_in_a_session_folder_records_start_and_end() {
    use rebuild_launcher::session;
    let home = temp("session");
    let cfg_path = home.join("rebuild.toml");
    let mut m = Model::new(Config::parse("").unwrap());
    m.config.profile = "stress-cpu".into();
    rebuild_config::builtin("stress-cpu").unwrap().apply(&mut m.config).unwrap();
    m.save(&cfg_path).unwrap();
    let exe = std::path::PathBuf::from(env!("CARGO_BIN_EXE_fake-layer"));
    let sessions = session::sessions_dir(&home);
    let start = session::Start { profile: "stress-cpu", config: &m.config, exe: &exe, config_path: &cfg_path, matrix: Some("m1"), run_seconds: Some(60), measure_from: Some("after_loading") };
    let dir = session::create(&sessions, &start, session::now_unix()).unwrap();
    let log = dir.join(session::FRAME_LOG);
    let env = vec![("REBUILD_RUN_SECONDS".to_string(), "60".to_string()), ("REBUILD_MEASURE_FROM".to_string(), "after_loading".to_string())];
    let extra: Vec<String> = ["--frames", "4", "--exit", "2"].iter().map(|s| s.to_string()).collect();
    let mut r = play::Running::start_in(&exe, &cfg_path, &log, Some(&dir), &env, &extra).unwrap();
    let t = Instant::now();
    let status = loop {
        if let Some(s) = r.poll() {
            break s;
        }
        assert!(t.elapsed() < Duration::from_secs(30), "the child never exited");
        std::thread::sleep(Duration::from_millis(5));
    };
    session::finish(&dir, &session::End { code: status.code(), status: status.to_string(), stopped_by_launcher: r.stopped }, session::now_unix()).unwrap();

    let lines: Vec<String> = std::fs::read_to_string(&log).unwrap().lines().map(String::from).collect();
    assert_eq!(lines.len(), 5, "the frame line went to <session>/frame-line.log: {lines:#?}");
    let list = session::list(&sessions);
    assert_eq!(list.len(), 1);
    let e = &list[0];
    let info = e.info.as_ref().unwrap();
    assert_eq!(info.profile, "stress-cpu");
    assert_eq!(info.exit_code, Some(2));
    assert!(!info.stopped_by_launcher);
    assert_eq!(info.limits, vec!["CPU 25 %".to_string()]);
    assert_eq!(info.setting("limits.cpu_rate"), Some("25"));
    let s = e.summary.as_ref().expect("the child wrote summary.toml into REBUILD_SESSION_DIR");
    assert_eq!(s.avg_fps, Some(30.0));
    assert_eq!(s.measured_from.as_deref(), Some("after_loading"), "the matrix environment reached the child");
    assert_eq!(e.crashes.len(), 1);
    assert!(e.outcome().starts_with("crash report"));
}
