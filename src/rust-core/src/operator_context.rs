use crate::context_observer::{format_visible_windows_inline, is_terminal_bundle, ContextSnapshot};

pub(crate) const NO_CONTEXT_MARKDOWN: &str =
    "- No focused app context has been observed yet.\n\
     - I can still answer questions, use recent action receipts, and run explicit actions you request.\n";

pub(crate) fn format_operator_context_markdown(snapshot: Option<&ContextSnapshot>) -> String {
    let Some(snapshot) = snapshot else {
        return NO_CONTEXT_MARKDOWN.to_string();
    };

    if snapshot.is_screen_locked {
        return "- Screen is locked; context observation is paused.\n\
                - Unlock the screen or name the app/file/contact explicitly before asking Dexter to act.\n"
            .to_string();
    }

    let Some(app_name) = snapshot.app_name.as_deref().filter(|name| !name.is_empty()) else {
        return NO_CONTEXT_MARKDOWN.to_string();
    };

    let bundle_id = snapshot.app_bundle_id.as_deref().unwrap_or("");
    let mut lines = Vec::new();
    lines.push(format!("- Focus: {}", focus_label(snapshot, app_name)));

    if let Some(shell) = snapshot.last_shell_command.as_ref() {
        let exit = shell
            .exit_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        lines.push(format!(
            "- Recent shell: `{}` -> exit {} in `{}`",
            shell.command, exit, shell.cwd
        ));
    }

    if snapshot
        .clipboard_text
        .as_deref()
        .is_some_and(|text| !text.is_empty())
    {
        lines.push("- Clipboard: text is available if you ask Dexter to use it.".to_string());
    }

    if !snapshot.visible_windows.is_empty() {
        lines.push(format!(
            "- Visible windows: {}",
            format_visible_windows_inline(&snapshot.visible_windows, 6)
        ));
    }

    lines.push("- Dexter can:".to_string());
    lines.extend(capability_lines(app_name, bundle_id));
    lines.push(
        "- Approval still applies before externally visible or destructive actions.".to_string(),
    );
    lines.join("\n") + "\n"
}

fn focus_label(snapshot: &ContextSnapshot, app_name: &str) -> String {
    let Some(element) = snapshot.focused_element.as_ref() else {
        return app_name.to_string();
    };

    let label = element.label.as_deref().unwrap_or("").trim();
    let value = element.value_preview.as_deref().unwrap_or("").trim();
    match (label.is_empty(), value.is_empty()) {
        (true, true) => app_name.to_string(),
        (false, true) => format!("{app_name} - {label}"),
        (true, false) => format!("{app_name} - {value}"),
        (false, false) => format!("{app_name} - {label}: {value}"),
    }
}

fn capability_lines(app_name: &str, bundle_id: &str) -> Vec<String> {
    let app_lc = app_name.to_ascii_lowercase();
    let bundle_lc = bundle_id.to_ascii_lowercase();

    if is_terminal_bundle(bundle_id) {
        return vec![
            "  - explain the latest shell error or output".to_string(),
            "  - suggest the next command".to_string(),
            "  - run a local command when you ask clearly".to_string(),
            "  - inspect files from the current workflow".to_string(),
        ];
    }

    if bundle_lc.contains("messages") || app_lc.contains("messages") {
        return vec![
            "  - draft or revise a message".to_string(),
            "  - resolve recipients through Contacts".to_string(),
            "  - send after operator approval".to_string(),
            "  - explain the latest message action receipt".to_string(),
        ];
    }

    if bundle_lc.contains("contacts") || app_lc.contains("contacts") {
        return vec![
            "  - use exact Contacts names for message sends".to_string(),
            "  - explain Contacts resolution failures".to_string(),
            "  - prepare a message action that still requires approval".to_string(),
        ];
    }

    if is_browser_context(&app_lc, &bundle_lc) {
        return vec![
            "  - summarize the current page".to_string(),
            "  - extract links or visible page text".to_string(),
            "  - click, type, or navigate when asked".to_string(),
            "  - request approval for consequential browser actions".to_string(),
        ];
    }

    if bundle_lc.contains("finder") || app_lc.contains("finder") {
        return vec![
            "  - inspect files and folders".to_string(),
            "  - read or summarize selected files".to_string(),
            "  - create, move, or edit files with the existing action policy".to_string(),
        ];
    }

    if is_editor_context(&app_lc, &bundle_lc) {
        return vec![
            "  - explain visible code or text".to_string(),
            "  - inspect nearby project files".to_string(),
            "  - run builds or tests when you ask".to_string(),
            "  - apply code changes through the action system".to_string(),
        ];
    }

    vec![
        "  - answer using the focused app and element context".to_string(),
        "  - use fresh clipboard text when you reference it".to_string(),
        "  - run explicit local actions through the normal approval flow".to_string(),
    ]
}

fn is_browser_context(app_lc: &str, bundle_lc: &str) -> bool {
    app_lc.contains("safari")
        || app_lc.contains("chrome")
        || app_lc.contains("firefox")
        || app_lc.contains("arc")
        || bundle_lc.contains("safari")
        || bundle_lc.contains("chrome")
        || bundle_lc.contains("firefox")
        || bundle_lc.contains("arc")
}

fn is_editor_context(app_lc: &str, bundle_lc: &str) -> bool {
    app_lc.contains("xcode")
        || app_lc.contains("cursor")
        || app_lc.contains("visual studio code")
        || app_lc.contains("zed")
        || app_lc.contains("sublime")
        || bundle_lc.contains("xcode")
        || bundle_lc.contains("vscode")
        || bundle_lc.contains("cursor")
        || bundle_lc.contains("zed")
        || bundle_lc.contains("sublime")
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use crate::context_observer::{
        AxElementInfo, ContextSnapshot, ShellCommandContext, VisibleWindowInfo,
    };

    use super::format_operator_context_markdown;

    fn snapshot(app_name: &str, bundle_id: &str) -> ContextSnapshot {
        ContextSnapshot {
            app_bundle_id: Some(bundle_id.to_string()),
            app_name: Some(app_name.to_string()),
            focused_element: None,
            is_screen_locked: false,
            clipboard_text: None,
            clipboard_changed_at: None,
            visible_windows: Vec::new(),
            last_shell_command: None,
            snapshot_hash: 42,
            last_updated: Utc::now(),
        }
    }

    #[test]
    fn no_context_reports_available_fallback() {
        let markdown = format_operator_context_markdown(None);
        assert!(markdown.contains("No focused app context"));
        assert!(markdown.contains("run explicit actions"));
    }

    #[test]
    fn terminal_context_surfaces_shell_capabilities() {
        let mut snap = snapshot("iTerm2", "com.googlecode.iterm2");
        snap.last_shell_command = Some(ShellCommandContext {
            command: "make test".to_string(),
            cwd: "/Users/jason/Developer/Dex".to_string(),
            exit_code: Some(2),
            received_at: Utc::now(),
        });
        let markdown = format_operator_context_markdown(Some(&snap));
        assert!(markdown.contains("Focus: iTerm2"));
        assert!(markdown.contains("Recent shell: `make test` -> exit 2"));
        assert!(markdown.contains("explain the latest shell error"));
        assert!(markdown.contains("run a local command"));
    }

    #[test]
    fn messages_context_surfaces_contacts_backed_send_capabilities() {
        let markdown =
            format_operator_context_markdown(Some(&snapshot("Messages", "com.apple.MobileSMS")));
        assert!(markdown.contains("resolve recipients through Contacts"));
        assert!(markdown.contains("send after operator approval"));
    }

    #[test]
    fn browser_context_surfaces_browser_worker_capabilities() {
        let mut snap = snapshot("Safari", "com.apple.Safari");
        snap.focused_element = Some(AxElementInfo {
            role: "AXWebArea".to_string(),
            label: Some("Example Page".to_string()),
            value_preview: None,
            is_sensitive: false,
        });
        let markdown = format_operator_context_markdown(Some(&snap));
        assert!(markdown.contains("Focus: Safari - Example Page"));
        assert!(markdown.contains("summarize the current page"));
        assert!(markdown.contains("click, type, or navigate"));
    }

    #[test]
    fn visible_windows_are_reported_for_operator_status() {
        let mut snap = snapshot("Claude", "com.anthropic.claudefordesktop");
        snap.visible_windows = vec![
            VisibleWindowInfo {
                owner_name: "Claude".to_string(),
                title: Some("Dexter debugging".to_string()),
                x: 20,
                y: 40,
                width: 1200,
                height: 900,
                is_frontmost: true,
            },
            VisibleWindowInfo {
                owner_name: "Terminal".to_string(),
                title: Some("Dexter Live Logs".to_string()),
                x: 1300,
                y: 80,
                width: 900,
                height: 700,
                is_frontmost: false,
            },
        ];

        let markdown = format_operator_context_markdown(Some(&snap));
        assert!(
            markdown.contains(
                "Visible windows: frontmost Claude \"Dexter debugging\"; Terminal \"Dexter Live Logs\""
            ),
            "visible window metadata should be operator-visible: {markdown}"
        );
    }

    #[test]
    fn locked_screen_reports_paused_context() {
        let mut snap = snapshot("Finder", "com.apple.finder");
        snap.is_screen_locked = true;
        let markdown = format_operator_context_markdown(Some(&snap));
        assert!(markdown.contains("Screen is locked"));
        assert!(markdown.contains("context observation is paused"));
    }
}
