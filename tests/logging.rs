use std::{path::PathBuf, process::Command};

// Each scenario runs in a separate process because the subscriber is global.
#[test]
fn logging_child() {
    let Some(directory) = std::env::var_os("YITERM_TEST_LOG_DIRECTORY") else {
        return;
    };
    let directory = PathBuf::from(directory);
    let config = directory.join("config.toml");
    let mode = std::env::var("YITERM_TEST_LOG_MODE").unwrap();
    let logging = yiterm::logging::init(
        Some(&config),
        (mode == "override").then_some("warn,yiterm::mouse=trace"),
    )
    .unwrap();
    assert_eq!(logging.warning.is_some(), mode == "fallback");
    tracing::info!(target: "yiterm::test", "info-marker");
    tracing::trace!(target: "yiterm::mouse", "mouse-marker");
    tracing::trace!(target: "yiterm::test", "hidden-marker");
    tracing::error!(target: "yiterm::test", "error-marker");
    if mode == "relay-parent" {
        let mut children = Vec::new();
        for _ in 0..4 {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", "logging_child", "--nocapture"])
                .env("YITERM_TEST_LOG_DIRECTORY", &directory)
                .env("YITERM_TEST_LOG_MODE", "file")
                .envs(yiterm::logging::child_environment())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            children.push(command.spawn().unwrap());
        }
        for child in children {
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(output.stderr.is_empty());
        }
    }
    drop(logging);
}

#[test]
fn files_filters_flush_and_fallback() {
    for mode in ["file", "override", "environment", "fallback"] {
        let directory = tempfile::tempdir().unwrap();
        let logs = directory.path().join("logs");
        if mode == "fallback" {
            std::fs::write(&logs, "not a directory").unwrap();
        }
        // TOML literal strings also preserve Windows backslashes.
        std::fs::write(
            directory.path().join("config.toml"),
            format!(
                "[logging]\nfilter = 'warn,yiterm=info'\nconsole = true\ndirectory = '{}'\n[browser]\nopen_links_in_browser = false\n",
                logs.display()
            ),
        )
        .unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "logging_child", "--nocapture"])
            .env("YITERM_TEST_LOG_DIRECTORY", directory.path())
            .env("YITERM_TEST_LOG_MODE", mode)
            .env_remove("YITERM_LOG");
        if mode == "environment" || mode == "override" {
            command.env("YITERM_LOG", "error");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty(), "Unexpected console logging");
        if mode == "fallback" {
            continue;
        }
        let paths: Vec<_> = std::fs::read_dir(&logs)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(paths.len(), 1);
        let text = std::fs::read_to_string(&paths[0]).unwrap();
        assert!(text.contains("error-marker"));
        assert_eq!(text.contains("info-marker"), mode == "file");
        assert_eq!(text.contains("mouse-marker"), mode == "override");
        assert!(!text.contains("hidden-marker"));
        assert!(!text.contains('\u{1b}'));
    }
}

#[test]
fn app_and_child_processes_share_one_log_file() {
    let directory = tempfile::tempdir().unwrap();
    let logs = directory.path().join("logs");
    std::fs::write(
        directory.path().join("config.toml"),
        format!(
            "[logging]\nfilter = 'warn,yiterm=info'\ndirectory = '{}'\n[browser]\nopen_links_in_browser = false\n",
            logs.display()
        ),
    )
    .unwrap();

    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "logging_child", "--nocapture"])
        .env("YITERM_TEST_LOG_DIRECTORY", directory.path())
        .env("YITERM_TEST_LOG_MODE", "relay-parent")
        .env_remove("YITERM_LOG")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());

    let files: Vec<_> = std::fs::read_dir(&logs)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(files.len(), 1);
    let text = std::fs::read_to_string(&files[0]).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 20);
    assert!(
        lines
            .iter()
            .all(|line| { line.starts_with("pid=") && line.matches("ThreadId(").count() == 1 })
    );
    assert_eq!(text.matches("info-marker").count(), 5);
    assert_eq!(text.matches("error-marker").count(), 5);
}
