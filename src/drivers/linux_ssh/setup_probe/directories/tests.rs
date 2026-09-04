use std::fmt::Write as _;

use super::*;

fn output(bytes: impl Into<Vec<u8>>) -> RemoteCommandOutput {
    RemoteCommandOutput {
        exit_status: 0,
        stdout: bytes.into(),
        stderr: Vec::new(),
        stdout_truncated: false,
        stderr_truncated: false,
    }
}

#[test]
fn canonical_browse_paths_allow_read_only_root_and_literal_metacharacters() {
    for path in ["/", "/srv/project", "/srv/a b'c;$(literal)", "/srv/项目"] {
        validate_browse_path(path).unwrap();
        let (canonical, _, list) = commands(path).unwrap();
        assert!(
            canonical
                .render_posix()
                .unwrap()
                .starts_with("'readlink' '-e' '--' ")
        );
        assert!(list.render_posix().unwrap().starts_with("'find' '-P' "));
        assert!(list.render_posix().unwrap().ends_with("'-printf' '%p\\0'"));
    }
    for path in [
        "",
        ".",
        "relative",
        "//srv",
        "/srv/",
        "/srv//app",
        "/srv/./app",
        "/srv/../app",
        "/srv/a\0b",
        "/srv/a\nb",
        "/srv/a\u{202e}b",
        "/srv/a\u{200b}b",
    ] {
        assert!(validate_browse_path(path).is_err(), "{path:?}");
        assert!(commands(path).is_err());
    }
    assert!(validate_browse_path(&format!("/{}", "a".repeat(MAX_PATH_BYTES))).is_err());
}

#[test]
fn directory_parser_preserves_literal_utf8_names_and_sorts_only_direct_children() {
    assert_eq!(
        parse_directory_output(
            "/srv",
            &output(b"/srv/z\0/srv/a b'c;$(literal)\0/srv/\xe9\xa1\xb9\xe7\x9b\xae\0")
        )
        .unwrap(),
        vec!["/srv/a b'c;$(literal)", "/srv/z", "/srv/项目"],
    );
    assert_eq!(
        parse_directory_output("/", &output(b"/var\0/etc\0")).unwrap(),
        vec!["/etc", "/var"]
    );
    assert!(
        parse_directory_output("/srv", &output(Vec::new()))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn malformed_partial_or_outside_directory_evidence_is_rejected_not_empty() {
    for bytes in [
        b"/srv/child".as_slice(),
        b"\0",
        b"/srv/child\0\0",
        b"/srv/child\0/srv/child\0",
        b"/srv-other/child\0",
        b"/srv/child/grandchild\0",
        b"/srv/../outside\0",
        b"/srv/child/\0",
        b"/srv//child\0",
        b"/srv/\xff\0",
        b"/srv/child\nname\0",
        b"relative\0",
        b"/srv\0",
    ] {
        assert!(
            parse_directory_output("/srv", &output(bytes)).is_err(),
            "{bytes:?}"
        );
    }
    assert!(
        parse_directory_output("/srv", &output(format!("/srv/{}\0", "a".repeat(256)))).is_err()
    );
    let mut hidden_control = "/srv/a\u{2067}b".as_bytes().to_vec();
    hidden_control.push(0);
    assert!(parse_directory_output("/srv", &output(hidden_control)).is_err());
}

#[test]
fn failed_truncated_or_excessive_directory_output_never_yields_partial_choices() {
    for mut result in [output(b"/srv/child\0"), output(Vec::new())] {
        result.exit_status = 1;
        assert!(parse_directory_output("/srv", &result).is_err());
        result.exit_status = 0;
        result.stdout_truncated = true;
        assert!(parse_directory_output("/srv", &result).is_err());
        result.stdout_truncated = false;
        result.stderr_truncated = true;
        assert!(parse_directory_output("/srv", &result).is_err());
        result.stderr_truncated = false;
        result.stderr = b"untrusted diagnostic sentinel".to_vec();
        let error = parse_directory_output("/srv", &result).unwrap_err();
        assert!(!error.contains("sentinel"));
    }
    let mut directories = String::new();
    for index in 0..=MAX_DIRECTORIES {
        write!(directories, "/srv/child-{index}\0").unwrap();
    }
    assert!(parse_directory_output("/srv", &output(directories)).is_err());
    assert!(parse_directory_output("/srv", &output(vec![b'a'; MAX_BYTES + 1])).is_err());
}

#[test]
fn command_failures_keep_static_deadline_and_cancellation_categories() {
    assert_eq!(
        command_failure(
            &SshConnectionError::Cancelled,
            "directory enumeration failed"
        ),
        "directory browsing cancelled",
    );
    assert_eq!(
        command_failure(
            &SshConnectionError::Timeout {
                timeout: Duration::from_secs(1),
                phase: "private-phase-sentinel",
            },
            "directory enumeration failed",
        ),
        "directory browsing timed out",
    );
    assert_eq!(
        command_failure(
            &SshConnectionError::Protocol("private-transport-sentinel".into()),
            "directory enumeration failed",
        ),
        "directory enumeration failed",
    );
}

#[test]
fn canonical_observation_requires_exact_path_and_clean_complete_output() {
    require_canonical(&output(b"/srv\n"), "/srv").unwrap();
    for bytes in [
        b"/other\n".as_slice(),
        b"/srv/\n",
        b"/srv\nextra",
        b"/srv\0",
        b"",
        b"/srv",
    ] {
        assert!(require_canonical(&output(bytes), "/srv").is_err());
    }
    let mut truncated = output(b"/srv\n");
    truncated.stdout_truncated = true;
    assert!(require_canonical(&truncated, "/srv").is_err());
}

#[cfg(target_os = "linux")]
fn execute_locally(command: &CommandSpec) -> RemoteCommandOutput {
    let result = std::process::Command::new("sh")
        .arg("-c")
        .arg(command.render_posix().unwrap())
        .output()
        .unwrap();
    RemoteCommandOutput {
        exit_status: u32::try_from(result.status.code().unwrap()).unwrap(),
        stdout: result.stdout,
        stderr: result.stderr,
        stdout_truncated: false,
        stderr_truncated: false,
    }
}

#[cfg(target_os = "linux")]
#[test]
fn actual_read_only_commands_list_immediate_directories_without_following_links_or_running_names() {
    use std::{fs, os::unix::fs::symlink};
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture
        .path()
        .canonicalize()
        .unwrap()
        .join("literal;$(printf inert) ' root");
    fs::create_dir(&root).unwrap();
    for name in ["alpha", "a b;$(printf inert)", "项目"] {
        fs::create_dir(root.join(name)).unwrap();
    }
    fs::create_dir(root.join("alpha/grandchild")).unwrap();
    fs::write(root.join("plain-file"), b"unchanged").unwrap();
    let outside = fixture.path().join("outside");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, root.join("linked-directory")).unwrap();
    let root_text = root.to_str().unwrap();
    let (canonical, directory, list) = commands(root_text).unwrap();
    require_canonical(&execute_locally(&canonical), root_text).unwrap();
    require_success(&execute_locally(&directory)).unwrap();
    let children = parse_directory_output(root_text, &execute_locally(&list)).unwrap();
    assert_eq!(
        children,
        ["a b;$(printf inert)", "alpha", "项目"].map(|name| root
            .join(name)
            .to_str()
            .unwrap()
            .to_owned())
    );
    require_canonical(&execute_locally(&canonical), root_text).unwrap();
    assert_eq!(fs::read(root.join("plain-file")).unwrap(), b"unchanged");
    assert_eq!(fs::read_dir(&root).unwrap().count(), 5);
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
}

#[cfg(target_os = "linux")]
#[test]
fn actual_commands_distinguish_empty_missing_file_and_symlink_roots() {
    use std::{fs, os::unix::fs::symlink};
    let fixture = tempfile::tempdir().unwrap();
    let base = fixture.path().canonicalize().unwrap();
    let empty = base.join("empty");
    fs::create_dir(&empty).unwrap();
    let empty = empty.to_str().unwrap();
    let (canonical, directory, list) = commands(empty).unwrap();
    require_canonical(&execute_locally(&canonical), empty).unwrap();
    require_success(&execute_locally(&directory)).unwrap();
    assert!(
        parse_directory_output(empty, &execute_locally(&list))
            .unwrap()
            .is_empty()
    );
    for (name, make_file) in [("file", true), ("missing", false)] {
        let path = base.join(name);
        if make_file {
            fs::write(&path, b"unchanged").unwrap();
        }
        let path = path.to_str().unwrap();
        let (canonical, directory, _) = commands(path).unwrap();
        assert!(
            require_canonical(&execute_locally(&canonical), path).is_err()
                || require_success(&execute_locally(&directory)).is_err()
        );
    }
    let linked = base.join("link");
    symlink(empty, &linked).unwrap();
    let linked = linked.to_str().unwrap();
    let (canonical, _, _) = commands(linked).unwrap();
    assert!(require_canonical(&execute_locally(&canonical), linked).is_err());
}
