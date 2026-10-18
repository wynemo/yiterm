//! Exercise the actual helper executable without contacting GitHub or opening a GUI.
use std::{
    fs,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

fn wait_for(mut condition: impl FnMut() -> bool) {
    let start = Instant::now();
    while !condition() {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "Timed out waiting for helper"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore]
fn parent_fixture() {
    let Some(path) = std::env::var_os("YITERM_UPDATE_TEST_PARENT") else {
        return;
    };
    fs::write(Path::new(&path).join("parent-ready"), b"ready").unwrap();
    wait_for(|| Path::new(&path).join("parent-exit").exists());
}

#[test]
fn helper_waits_for_parent_installs_all_files_and_restarts_without_gui() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path();
    let stage = target.join(".yiterm-update-test");
    let payload = stage.join("payload");
    fs::create_dir_all(&payload).unwrap();
    let exe = if cfg!(windows) {
        "yiterm.exe"
    } else {
        "yiterm"
    };
    let helper_path = stage.join(if cfg!(windows) {
        "updater.exe"
    } else {
        "updater"
    });
    fs::copy(env!("CARGO_BIN_EXE_yiterm"), &helper_path).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_yiterm"), payload.join(exe)).unwrap();
    fs::write(target.join(exe), b"old executable").unwrap();
    let mut files = vec![exe, "TECHNICAL.md"];
    if cfg!(windows) {
        files.extend(["conpty.dll", "OpenConsole.exe"]);
    }
    for name in files.iter().skip(1) {
        fs::write(target.join(name), b"old file").unwrap();
        fs::write(payload.join(name), b"new file").unwrap();
    }
    let mut parent = Process(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "parent_fixture", "--ignored"])
            .env("YITERM_UPDATE_TEST_PARENT", target)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for(|| target.join("parent-ready").exists());
    let args: Vec<std::ffi::OsString> = vec!["--version".into()];
    let plan = serde_json::json!({
        "parent": parent.0.id(), "target": target, "payload": payload,
        "files": files, "args": args, "cwd": target,
    });
    let plan_path = stage.join("plan.json");
    fs::write(&plan_path, serde_json::to_vec(&plan).unwrap()).unwrap();
    let mut helper = Process(
        Command::new(helper_path)
            .arg("--internal-apply-update")
            .arg(plan_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for(|| stage.join("ready").exists() || helper.0.try_wait().unwrap().is_some());
    assert!(stage.join("ready").exists(), "Helper exited before ready");
    assert_eq!(fs::read(target.join(exe)).unwrap(), b"old executable");
    assert!(helper.0.try_wait().unwrap().is_none());
    fs::write(target.join("parent-exit"), b"exit").unwrap();
    assert!(parent.0.wait().unwrap().success());
    let mut status = None;
    wait_for(|| {
        status = helper.0.try_wait().unwrap();
        status.is_some()
    });
    assert!(
        status.unwrap().success(),
        "{:?}",
        fs::read_to_string(stage.join("error.txt"))
    );
    for name in files.iter().skip(1) {
        assert_eq!(fs::read(target.join(name)).unwrap(), b"new file");
    }
    assert!(!target.join(".yiterm-update-error.txt").exists());
    assert!(
        Command::new(target.join(exe))
            .arg("--version")
            .stdout(Stdio::null())
            .status()
            .unwrap()
            .success()
    );
}
