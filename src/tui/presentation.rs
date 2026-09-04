//! Pure, bounded presentation of already-redacted values; never execution policy.

const DISPLAY_CHARACTER_LIMIT: usize = 4096;
const TRUNCATION_NOTICE: &str = " [display truncated]";

/// Produces one terminal-safe line. Secret redaction belongs to the caller.
pub(super) fn safe_text(value: &str) -> String {
    let mut characters = value.chars().filter(|character| {
        !character.is_control()
            && !matches!(
                *character,
                '\u{200b}'..='\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2060}'..='\u{206f}'
                    | '\u{feff}'
            )
    });
    let mut text: String = characters.by_ref().take(DISPLAY_CHARACTER_LIMIT).collect();
    if characters.next().is_some() {
        text.push_str(TRUNCATION_NOTICE);
    }
    text
}

/// Keeps the existing ASCII-insensitive `prod` substring warning heuristic.
/// This is a visual hint, not a classification or authorization rule.
pub(super) fn is_production(name: &str) -> bool {
    name.as_bytes()
        .windows(4)
        .any(|window| window.eq_ignore_ascii_case(b"prod"))
}

pub(super) fn environment_label(name: &str) -> String {
    let safe_name = safe_text(name);
    if is_production(name) || is_production(&safe_name) {
        format!("{safe_name} [PRODUCTION]")
    } else {
        safe_name
    }
}

/// Formats a display endpoint without parsing, connecting or rewriting settings.
pub(super) fn endpoint_label(user: &str, host: &str, port: u16) -> String {
    let user = safe_text(user);
    let host = safe_text(host);
    if host.contains(':') && !(host.starts_with('[') && host.ends_with(']')) {
        format!("{user}@[{host}]:{port}")
    } else {
        format!("{user}@{host}:{port}")
    }
}

/// Builds one navigation line, keeping the production warning before long fields.
/// Missing context is not reconstructed, and orphaned fields cannot imply a Project.
pub(super) fn context_label(
    page: &str,
    project: Option<&str>,
    environment: Option<&str>,
    component: Option<&str>,
) -> String {
    let page = safe_text(page);
    let Some(project) = project else {
        return page;
    };
    let production = environment.is_some_and(is_production);
    let environment = environment.map(safe_text);
    let prefix = if production || environment.as_deref().is_some_and(is_production) {
        "[PRODUCTION] "
    } else {
        ""
    };
    let mut context = vec![safe_text(project)];
    if let Some(environment) = environment {
        context.push(environment);
    }
    if let Some(component) = component {
        context.push(safe_text(component));
    }
    format!("{prefix}{} · {page}", context.join(" / "))
}

/// Translates step metadata only. Never apply this mapping to names or log bodies.
/// Unknown values remain intact; a dot alone does not identify a namespace.
pub(super) fn step_label(value: &str) -> String {
    let value = safe_text(value);
    if let Some(version) = value.strip_prefix("cleanup.") {
        return if version.is_empty() {
            "Release cleanup".into()
        } else {
            format!("Cleanup {version}")
        };
    }
    if let Some(stage) = value.strip_prefix("linux-ssh.") {
        return known_step(stage).unwrap_or("Remote operation").into();
    }
    if value == "linux-ssh" {
        return "Remote operation".into();
    }
    for prefix in ["capability.", "capabilities.", "Capability::"] {
        if let Some(capability) = value.strip_prefix(prefix) {
            return capability_label(capability)
                .unwrap_or("Operation support")
                .into();
        }
    }
    known_step(&value)
        .or_else(|| capability_label(&value))
        .map_or(value, String::from)
}

fn known_step(value: &str) -> Option<&'static str> {
    Some(match value {
        "build" | "build.started" => "Building",
        "build-package" => "Build and package",
        "build.packaging" | "packaging" => "Packaging",
        "build.stdout" => "Build output",
        "build.stderr" => "Build diagnostic output",
        "deployment.started" => "Deployment started",
        "deployment.finished" => "Deployment finished",
        "deployment.failed" => "Deployment failed",
        "upload" | "uploading" => "Uploading",
        "prepare" => "Preparing Release",
        "prepare.audit" => "Preparation audit",
        "history.prepare" => "Recording preparation",
        "activate" | "activate.started" => "Activating",
        "activate.finished" => "Activation finished",
        "activate.receipt" => "Activation observation",
        "activate.failure" => "Observation after activation failure",
        "compensate" | "compensate.started" => "Compensating changes",
        "compensate.receipt" | "compensation-receipt" => "Compensation observation",
        "compensate.failure" | "compensation-after-failure" => {
            "Observation after compensation failure"
        }
        "compensate.blocked" => "Compensation blocked",
        "compensation-before-mutation" => "Observation before compensation",
        "rollback" => "Rolling back",
        "rollback-preflight" => "Rollback preflight",
        "rollback-before-mutation" => "Observation before rollback",
        "rollback-receipt" => "Rollback observation",
        "rollback-after-failure" => "Observation after rollback failure",
        "configuration" | "context" => "Configuration check",
        "target" => "Deployment target",
        "destination" => "Connection configuration",
        "connect" => "Connecting",
        "authentication" => "Authenticating",
        "preflight" => "Preflight",
        "observe" => "Current observation",
        "inventory" => "Release inventory",
        "space" => "Disk capacity check",
        "marker" => "Deployment identity check",
        "health" => "Health check",
        "audit" => "Audit record",
        "logs" => "Logs",
        "cleanup" => "Release cleanup",
        _ => return None,
    })
}

fn capability_label(value: &str) -> Option<&'static str> {
    Some(match value {
        "local-build" | "LocalBuild" => "Local build",
        "provider-build" | "ProviderBuild" => "Server build",
        "staged-deployment" | "StagedDeployment" => "Release preparation",
        "explicit-activation" | "ExplicitActivation" => "Release activation",
        "observe" | "Observe" => "Current observation",
        "inventory" | "Inventory" => "Release inventory",
        "rollback" | "Rollback" => "Rollback",
        "preview-url" | "PreviewUrl" => "Preview link",
        "remote-logs" | "RemoteLogs" => "Remote logs",
        "retention" | "Retention" => "Release retention",
        "cancellation" | "Cancellation" => "Safe cancellation",
        "traffic-splitting" | "TrafficSplitting" => "Traffic distribution",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_preserves_unicode_but_removes_controls_and_bidirectional_marks() {
        assert_eq!(
            safe_text(
                "项目\0\n\r\t\u{1b}é\u{85}\u{200b}\u{200f}\u{202a}\u{202e}\u{2060}\u{2069}\u{206f}\u{feff}🦀"
            ),
            "项目é🦀"
        );
        for point in (0x200b..=0x200f)
            .chain(0x202a..=0x202e)
            .chain(0x2060..=0x206f)
        {
            assert_eq!(safe_text(&char::from_u32(point).unwrap().to_string()), "");
        }
        assert_eq!(safe_text(""), "");
    }

    #[test]
    fn text_limits_characters_after_filtering_without_splitting_unicode() {
        let exact = "🦀".repeat(DISPLAY_CHARACTER_LIMIT);
        assert_eq!(safe_text(&exact), exact);
        assert_eq!(safe_text(&format!("{exact}\n\u{202e}")), exact);
        assert_eq!(
            safe_text(&format!("{exact}終")),
            format!("{exact}{TRUNCATION_NOTICE}")
        );
        assert_eq!(
            safe_text(&"\n界".repeat(DISPLAY_CHARACTER_LIMIT)),
            "界".repeat(DISPLAY_CHARACTER_LIMIT)
        );
    }

    #[test]
    fn production_hint_retains_case_insensitive_substring_behavior() {
        for name in [
            "prod",
            "PRODUCTION",
            "PrePrOd",
            "product-preview",
            "team-prod-eu",
        ] {
            assert!(is_production(name), "{name}");
        }
        for name in ["", "staging", "proud", "PRÖD", "生产"] {
            assert!(!is_production(name), "{name}");
        }
        assert_eq!(environment_label("PrePrOd"), "PrePrOd [PRODUCTION]");
        assert_eq!(environment_label("test\n环境"), "test环境");
        assert_eq!(
            environment_label("pro\u{202e}duction"),
            "production [PRODUCTION]"
        );
    }

    #[test]
    fn endpoints_handle_hostnames_ipv4_ipv6_and_existing_brackets() {
        for (host, expected) in [
            ("example.com", "deploy@example.com:22"),
            ("127.0.0.1", "deploy@127.0.0.1:22"),
            ("::1", "deploy@[::1]:22"),
            ("[::1]", "deploy@[::1]:22"),
            ("2001:db8::7", "deploy@[2001:db8::7]:22"),
            ("fe80::1%eth0", "deploy@[fe80::1%eth0]:22"),
        ] {
            assert_eq!(endpoint_label("deploy", host, 22), expected);
        }
        assert_eq!(
            endpoint_label("用\n户", "\u{202e}[::1]\0", 65535),
            "用户@[::1]:65535"
        );
        assert_eq!(endpoint_label("a\u{1b}", "::\u{200f}1", 0), "a@[::1]:0");
    }

    #[test]
    fn context_is_ordered_single_line_and_does_not_infer_missing_fields() {
        assert_eq!(
            context_label("History", Some("shop"), Some("test"), Some("api")),
            "shop / test / api · History"
        );
        assert_eq!(
            context_label("History", Some("shop"), None, None),
            "shop · History"
        );
        assert_eq!(
            context_label("History", Some("shop"), None, Some("api")),
            "shop / api · History"
        );
        assert_eq!(
            context_label("Projects", None, Some("production"), Some("api")),
            "Projects"
        );
        assert_eq!(
            context_label(
                "Hi\nstory",
                Some("商\u{202e}店"),
                Some("test"),
                Some("api\t")
            ),
            "商店 / test / api · History"
        );
    }

    #[test]
    fn production_context_prefix_is_visible_before_long_names_and_appears_once() {
        let text = context_label(
            "Deployment",
            Some(&"界".repeat(4097)),
            Some("pRoDuCtIoN"),
            Some("api"),
        );
        assert!(text.starts_with("[PRODUCTION] 界"));
        assert_eq!(text.matches("[PRODUCTION]").count(), 1);
        assert!(text.ends_with(" / pRoDuCtIoN / api · Deployment"));
        assert!(text.contains(TRUNCATION_NOTICE));

        let environment = format!("{}-prod", "界".repeat(DISPLAY_CHARACTER_LIMIT));
        assert!(environment_label(&environment).ends_with(" [PRODUCTION]"));
        assert!(
            context_label("History", Some("shop"), Some(&environment), None)
                .starts_with("[PRODUCTION] shop / ")
        );
    }

    #[test]
    fn real_event_and_history_namespaces_have_explicit_human_labels() {
        for (input, expected) in [
            ("build.started", "Building"),
            ("build.packaging", "Packaging"),
            ("build.stdout", "Build output"),
            ("build.stderr", "Build diagnostic output"),
            ("build-package", "Build and package"),
            ("deployment.started", "Deployment started"),
            ("deployment.finished", "Deployment finished"),
            ("deployment.failed", "Deployment failed"),
            ("linux-ssh.upload", "Uploading"),
            ("linux-ssh.uploading", "Uploading"),
            ("activate.started", "Activating"),
            ("activate.receipt", "Activation observation"),
            ("compensate.blocked", "Compensation blocked"),
            ("rollback-before-mutation", "Observation before rollback"),
            (
                "compensation-after-failure",
                "Observation after compensation failure",
            ),
            ("history.prepare", "Recording preparation"),
            ("prepare.audit", "Preparation audit"),
        ] {
            assert_eq!(step_label(input), expected, "{input}");
        }
    }

    #[test]
    fn unknown_namespace_and_version_values_are_not_split_or_rewritten() {
        for value in [
            "1.2.3",
            "v1.2-alpha_3",
            "custom.action_name",
            "build.custom.action",
            "linux-sshx.upload",
            "custom.linux-ssh.upload",
            "Build.packaging",
        ] {
            assert_eq!(step_label(value), value);
        }
        assert_eq!(step_label("cleanup.v1.2.3_alpha"), "Cleanup v1.2.3_alpha");
        assert_eq!(step_label("cleanup."), "Release cleanup");
        assert_eq!(step_label("linux-ssh"), "Remote operation");
        assert_eq!(
            step_label("linux-ssh.unknown.capability"),
            "Remote operation"
        );
        assert_eq!(step_label("linux-\u{200b}ssh.upload"), "Uploading");
        assert_eq!(step_label(""), "");
    }

    #[test]
    fn capability_metadata_is_translated_without_touching_other_user_fields() {
        for (kebab, debug, expected) in [
            ("local-build", "LocalBuild", "Local build"),
            ("provider-build", "ProviderBuild", "Server build"),
            (
                "staged-deployment",
                "StagedDeployment",
                "Release preparation",
            ),
            (
                "explicit-activation",
                "ExplicitActivation",
                "Release activation",
            ),
            ("observe", "Observe", "Current observation"),
            ("inventory", "Inventory", "Release inventory"),
            ("rollback", "Rollback", "Rollback"),
            ("preview-url", "PreviewUrl", "Preview link"),
            ("remote-logs", "RemoteLogs", "Remote logs"),
            ("retention", "Retention", "Release retention"),
            ("cancellation", "Cancellation", "Safe cancellation"),
            (
                "traffic-splitting",
                "TrafficSplitting",
                "Traffic distribution",
            ),
        ] {
            assert_eq!(step_label(&format!("capability.{kebab}")), expected);
            assert_eq!(step_label(&format!("Capability::{debug}")), expected);
            assert_eq!(safe_text(debug), debug);
            assert_eq!(environment_label(debug), debug);
        }
        assert_eq!(
            step_label("capabilities.unknown-new-operation"),
            "Operation support"
        );
        assert_eq!(
            step_label("custom.capability.local-build"),
            "custom.capability.local-build"
        );
    }
}
