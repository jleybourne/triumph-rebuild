use super::*;
use std::process::{Command, Stdio};

/// Tests that take a lock in this process run one at a time (`release_all` frees every lock the process holds).
static SERIAL: Mutex<()> = Mutex::new(());

/// A fresh folder for one test's lock.
fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rebuild-cardlock-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn quick() -> Command {
    if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", "exit 0"]);
        c
    } else {
        Command::new("true")
    }
}

/// The pid of a process that has ended.
fn dead_pid() -> u32 {
    let mut c = quick().stdout(Stdio::null()).spawn().unwrap();
    let pid = c.id();
    c.wait().unwrap();
    pid
}

/// A process that runs until killed.
fn sleeper() -> std::process::Child {
    let mut c = if cfg!(windows) {
        let mut c = Command::new("ping");
        c.args(["-n", "60", "127.0.0.1"]);
        c
    } else {
        let mut c = Command::new("sleep");
        c.arg("60");
        c
    };
    c.stdout(Stdio::null()).spawn().unwrap()
}

#[test]
fn take_names_this_process_and_drop_removes() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let d = dir("basic");
    let l = CardLock::take_in(&d).unwrap();
    let text = std::fs::read_to_string(d.join("CARD")).unwrap();
    assert_eq!(Owner::parse(&text).unwrap().pid, std::process::id());
    assert!(text.contains("tool "));
    assert!(CardLock::take_in(&d).unwrap_err().contains("already holds"));
    drop(l);
    assert!(!d.join("CARD").exists());
    drop(CardLock::take_in(&d).unwrap());
    assert!(std::fs::read_dir(&d).unwrap().next().is_none(), "no private files left behind");
}

#[test]
fn measuring_refuses() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let d = dir("measuring");
    std::fs::write(d.join("MEASURING"), "").unwrap();
    assert!(CardLock::take_in(&d).unwrap_err().contains("measurement"));
    assert!(!d.join("CARD").exists());
}

#[test]
fn a_dead_owner_is_taken_over() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let d = dir("dead");
    let pid = dead_pid();
    std::fs::write(d.join("CARD"), format!("pid {pid}\ntool gone.exe\n")).unwrap();
    let l = CardLock::take_in(&d).unwrap();
    assert_eq!(Owner::parse(&std::fs::read_to_string(l.path()).unwrap()).unwrap().pid, std::process::id());
}

#[test]
fn the_older_format_is_read_and_taken_over() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let d = dir("older");
    let pid = dead_pid();
    std::fs::write(d.join("CARD"), format!("rebuild-pipelines pid {pid}\n")).unwrap();
    assert_eq!(Owner::parse(&format!("rebuild-pipelines pid {pid}\n")), Some(Owner { pid, start: None }));
    drop(CardLock::take_in(&d).unwrap());
}

#[test]
fn a_running_owner_keeps_it() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let d = dir("running");
    let mut s = sleeper();
    let o = Owner { pid: s.id(), start: sys::start_time(s.id()) };
    std::fs::write(d.join("CARD"), o.text("sleeper")).unwrap();
    let e = CardLock::take_in(&d).unwrap_err();
    assert!(e.contains("another lane") && e.contains(&s.id().to_string()), "{e}");
    assert!(!CardLock::release_dead(&d, s.id()), "a running worker's lock stays");
    s.kill().unwrap();
    s.wait().unwrap();
    assert!(CardLock::release_dead(&d, o.pid));
    assert!(!d.join("CARD").exists());
}

#[cfg(windows)]
#[test]
fn a_reused_pid_is_stale() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let d = dir("reused");
    let me = Owner::me();
    assert!(me.start.is_some());
    assert!(me.alive());
    // this pid, but a creation time it never had: the pid was reused, the owner is gone
    let other = Owner { pid: me.pid, start: Some(me.start.unwrap() - 1) };
    assert!(!other.alive());
    std::fs::write(d.join("CARD"), other.text("earlier")).unwrap();
    drop(CardLock::take_in(&d).unwrap());
}

#[test]
fn an_unnamed_lock_is_held_while_fresh() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let d = dir("unnamed");
    std::fs::write(d.join("CARD"), "").unwrap();
    assert!(CardLock::take_in(&d).unwrap_err().contains("being written"));
}

#[test]
fn release_all_frees_what_this_process_holds() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let d = dir("release");
    let l = CardLock::take_in(&d).unwrap();
    release_all();
    assert!(!d.join("CARD").exists());
    // another lane takes it; the earlier guard's drop must leave that one alone
    std::fs::write(d.join("CARD"), format!("pid {}\n", sleeper_pid_placeholder())).unwrap();
    drop(l);
    assert!(d.join("CARD").exists());
}

/// A pid that is not this process (only written, never checked for life).
fn sleeper_pid_placeholder() -> u32 {
    std::process::id().wrapping_add(4)
}

/// Run as a child by the exit tests: takes the lock in `CARDLOCK_CHILD_DIR` and leaves through `exit` or
/// `std::process::exit` as `CARDLOCK_CHILD_HOW` says.
#[test]
fn child_takes_and_exits() {
    let (Ok(d), Ok(how)) = (std::env::var("CARDLOCK_CHILD_DIR"), std::env::var("CARDLOCK_CHILD_HOW")) else { return };
    let _l = CardLock::take_in(Path::new(&d)).unwrap();
    match how.as_str() {
        "exit" => crate::exit(0),
        _ => std::process::exit(0),
    }
}

fn run_child(d: &Path, how: &str) {
    let st = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "tests::child_takes_and_exits", "--test-threads", "1"])
        .env("CARDLOCK_CHILD_DIR", d)
        .env("CARDLOCK_CHILD_HOW", how)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(st.success());
}

#[test]
fn exit_releases_the_lock() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let d = dir("exit");
    run_child(&d, "exit");
    assert!(!d.join("CARD").exists());
}

#[test]
fn process_exit_leaves_a_stale_lock_that_is_taken_over() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let d = dir("process-exit");
    run_child(&d, "process");
    assert!(d.join("CARD").exists(), "std::process::exit skips the drop");
    drop(CardLock::take_in(&d).expect("the dead child's lock is stale"));
}
