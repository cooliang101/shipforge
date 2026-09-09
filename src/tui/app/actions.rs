//! Visible actions share the existing guarded dispatch and worker ownership.
use super::{App, KeyCode, KeyEvent, Screen};
use crate::tui::i18n;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, List, ListItem, ListState},
};

pub(in crate::tui) struct Actions {
    pub items: Vec<(&'static str, KeyCode)>,
    pub rows: usize,
    pub row: usize,
    pub toggle: bool,
}

impl Actions {
    fn overview() -> Self {
        let choose = i18n::choose;

        let mut actions = Actions::new(
            &[
                (choose("Deploy", "发布"), 'd'),
                (choose("History & recovery", "历史与恢复"), 'm'),
                (choose("Configuration", "配置"), 'e'),
            ],
            0,
            0,
        );
        actions.items.extend([
            (choose("Previous environment", "上一个环境"), KeyCode::Left),
            (choose("Next environment", "下一个环境"), KeyCode::Right),
        ]);
        actions
    }

    pub fn confirmation(label: &'static str, key: char) -> Self {
        // Start on the review, so a repeated Enter cannot authorize a write.
        Self::new(&[(label, key)], 1, 0)
    }
    pub fn new(items: &[(&'static str, char)], rows: usize, row: usize) -> Self {
        Self {
            items: items
                .iter()
                .map(|(label, key)| (*label, KeyCode::Char(*key)))
                .collect(),
            rows,
            row,
            toggle: false,
        }
    }
    pub fn choice(rows: usize, row: usize) -> Self {
        Self {
            items: vec![(i18n::choose("Continue", "继续"), KeyCode::Enter)],
            rows,
            row,
            toggle: true,
        }
    }
}

impl App {
    pub(super) fn handle_projects_exit(&mut self, key: KeyEvent) -> Option<bool> {
        if !matches!(self.screen, Screen::Projects) {
            return None;
        }
        if key.modifiers == super::KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
            self.request_process_exit();
            return Some(false);
        }
        if key.code != KeyCode::Esc || !key.modifiers.is_empty() {
            return None;
        }
        if key.kind != crossterm::event::KeyEventKind::Press {
            return Some(false);
        }
        let now = std::time::Instant::now();
        if self.project_exit_armed_at.take().is_some_and(|previous| {
            now.duration_since(previous) <= std::time::Duration::from_secs(1)
        }) {
            self.request_process_exit();
        } else {
            self.project_exit_armed_at = Some(now);
            self.message = Some(
                i18n::choose(
                    "Press Esc again within 1 second to exit; Ctrl+C also exits.",
                    "1 秒内再按一次 Esc 退出，也可按 Ctrl+C 退出。",
                )
                .into(),
            );
        }
        Some(false)
    }

    pub(in crate::tui) fn action_height(&self, available: u16) -> u16 {
        let Some(actions) = self.page_actions() else {
            return 0;
        };
        if available < 10 && actions.rows > 0 && self.action_cursor.is_none() {
            return 0;
        }
        (u16::try_from(actions.items.len())
            .unwrap_or(u16::MAX)
            .saturating_add(2))
        .min((available / 3).max(3))
    }
    pub(in crate::tui) fn page_actions(&self) -> Option<Actions> {
        if self.management_task.is_some()
            || self.connections_task.is_some()
            || self.project_edit_task.is_some()
            || self.setup_task.is_some()
            || self.remote_target_task.is_some()
            || self.reinitialize_task.is_some()
        {
            return None;
        }
        let choose = i18n::choose;
        Some(match &self.screen {
            Screen::Overview { .. } => Actions::overview(),
            Screen::Projects => Actions::new(
                &[
                    (choose("Connections", "连接管理"), 'c'),
                    (choose("Refresh projects", "刷新项目"), 'f'),
                    (
                        choose("Remove selected recent project", "移除选中的最近项目"),
                        'x',
                    ),
                ],
                self.recent.len() + 1,
                self.selected_recent,
            ),
            Screen::Browser(browser) => Actions::new(
                &[(choose("Use this directory", "使用当前目录"), 's')],
                browser.children.len(),
                browser.selected,
            ),
            Screen::DeploySelection(selection) => {
                let mut actions = Actions::choice(
                    super::deployment_components(selection).len(),
                    selection.component_cursor,
                );
                actions.items.extend([
                    (choose("Previous environment", "上一个环境"), KeyCode::Left),
                    (choose("Next environment", "下一个环境"), KeyCode::Right),
                ]);
                actions
            }
            Screen::SetupComponents(setup) => {
                let mut actions = Actions::choice(setup.report.components.len(), setup.cursor);
                actions
                    .items
                    .push((choose("Add component", "添加组件"), KeyCode::Char('a')));
                actions
            }
            Screen::Management(screen) => return screen.actions(),
            Screen::Connections(screen) => return screen.actions(),
            Screen::ProjectEdit(screen) => return screen.actions(),
            Screen::RemoteSetupSelection(screen) => return screen.actions(),
            Screen::ManualComponent(screen) => return screen.actions(),
            Screen::Reinitialize(screen) => return screen.actions(),
            Screen::DeploymentReview { .. } => {
                Actions::new(&[(choose("Confirm deployment", "确认发布"), 'c')], 0, 0)
            }
            Screen::SetupReview { .. } => {
                Actions::confirmation(choose("Save configuration", "保存配置"), 'c')
            }
            Screen::HostKeyConfirm { .. } => {
                Actions::confirmation(choose("Trust this host key", "信任此主机指纹"), 'y')
            }
            Screen::SetupDestinations(setup) => {
                let mut actions = Actions::new(
                    &[
                        (choose("Add connection", "添加连接"), 'a'),
                        (
                            choose(
                                "Configure remote directory and service",
                                "配置远端目录与服务",
                            ),
                            'e',
                        ),
                        (choose("Continue to preview", "继续预览"), 'n'),
                    ],
                    setup.destinations.len(),
                    setup.destination_cursor,
                );
                actions.items.extend([
                    (choose("Previous component", "上一个组件"), KeyCode::Left),
                    (choose("Next component", "下一个组件"), KeyCode::Right),
                ]);
                actions
            }
            Screen::DeploymentFinished { .. } => {
                Actions::new(&[(choose("View logs", "查看日志"), 'l')], 0, 0)
            }
            _ => return None,
        })
    }

    // None consumes navigation; Some forwards one explicit action, never recursion.
    pub(super) fn action_key(&mut self, mut key: KeyEvent) -> Option<KeyEvent> {
        let Some(actions) = self.page_actions() else {
            self.action_cursor = None;
            return Some(key);
        };
        if !key.modifiers.is_empty() {
            return Some(key);
        }
        let focus = self.action_cursor.or((actions.rows == 0).then_some(0));
        match (key.code, focus) {
            (KeyCode::Down, Some(index)) => {
                self.action_cursor = Some((index + 1).min(actions.items.len().saturating_sub(1)));
            }
            (KeyCode::Up, Some(0)) if actions.rows > 0 => self.action_cursor = None,
            (KeyCode::Up, Some(index)) => self.action_cursor = Some(index.saturating_sub(1)),
            (KeyCode::Down, None) if actions.row + 1 >= actions.rows => {
                self.action_cursor = Some(0);
            }
            (KeyCode::Enter, Some(index)) => {
                key.code = actions.items.get(index)?.1;
                self.action_cursor = None;
                return Some(key);
            }
            (KeyCode::Enter, None) if actions.toggle => {
                key.code = KeyCode::Char(' ');
                return Some(key);
            }
            _ => {
                self.action_cursor = None;
                return Some(key);
            }
        }
        None
    }

    pub(in crate::tui) fn render_actions(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(actions) = self.page_actions() else {
            return;
        };
        let selected = self.action_cursor.or((actions.rows == 0).then_some(0));
        let items = actions
            .items
            .iter()
            .map(|(label, _)| ListItem::new(Line::from(*label)));
        let borders = if area.height < 3 {
            Borders::NONE
        } else {
            Borders::ALL
        };
        let list = List::new(items)
            .block(Block::default().borders(borders).title(i18n::choose(
                " Actions · ↑/↓ Enter ",
                " 操作 · ↑/↓ 选择 Enter 确认 ",
            )))
            .highlight_symbol("> ")
            .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan));
        frame.render_stateful_widget(
            list,
            area,
            &mut ListState::default().with_selected(selected),
        );
    }
}

#[cfg(test)]
impl App {
    // Workflow tests explicitly move from a multi-select list to Continue.
    pub(super) fn enter_primary(&mut self) -> bool {
        if let Some(actions) = self.page_actions()
            && actions.toggle
            && self.action_cursor.is_none()
        {
            for _ in 0..actions.rows.saturating_sub(actions.row) {
                self.handle_key(KeyEvent::new(KeyCode::Down, super::KeyModifiers::NONE));
            }
        }
        self.handle_key(KeyEvent::new(KeyCode::Enter, super::KeyModifiers::NONE))
    }
}

#[cfg(test)]
mod tests {
    use super::super::KeyModifiers;
    use super::*;

    fn fixture() -> (tempfile::TempDir, App) {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("shipforge.yaml"),
            include_str!("../../../docs/examples/shipforge.yaml"),
        )
        .unwrap();
        let crate::config::ProjectConfigState::Loaded(config) =
            crate::config::load(directory.path()).unwrap()
        else {
            panic!("fixture config");
        };
        let mut app = App::new(
            directory.path().join("projects.yaml"),
            directory.path().join("destinations.yaml"),
            directory.path(),
        )
        .unwrap();
        app.show_overview(directory.path().into(), config);
        (directory, app)
    }

    fn press(app: &mut App, code: KeyCode) {
        assert!(!app.handle_key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    fn assert_highlighted_action(app: &App, label: &str) {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 10)).unwrap();
        terminal
            .draw(|frame| crate::tui::render(frame, app))
            .unwrap();
        let highlighted: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .filter(|cell| cell.bg == Color::Cyan)
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            highlighted.contains(label),
            "missing focused {label}: {highlighted}"
        );
    }

    #[test]
    fn overview_and_component_selection_use_arrows_and_enter() {
        let (_directory, mut app) = fixture();
        press(&mut app, KeyCode::Enter);
        let Screen::DeploySelection(selection) = &app.screen else {
            panic!("selection");
        };
        let original = selection.selected.clone();
        let count = super::super::deployment_components(selection).len();
        assert_eq!(app.action_cursor, Some(0));
        assert_highlighted_action(&app, "Continue");
        press(&mut app, KeyCode::Up);
        press(&mut app, KeyCode::Enter);
        let Screen::DeploySelection(selection) = &app.screen else {
            panic!("Enter must toggle, never plan");
        };
        assert_eq!(selection.selected.len(), original.len() - 1);
        press(&mut app, KeyCode::Enter);
        let Screen::DeploySelection(selection) = &app.screen else {
            unreachable!()
        };
        assert_eq!(selection.selected, original);
        for _ in 0..count {
            press(&mut app, KeyCode::Down);
        }
        assert_eq!(app.action_cursor, Some(0));
        press(&mut app, KeyCode::Up);
        assert_eq!(app.action_cursor, None);
        assert!(matches!(app.screen, Screen::DeploySelection(_)));
    }

    #[tokio::test]
    async fn configuration_menu_is_available_without_letter_shortcuts() {
        let (_directory, mut app) = fixture();
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.page_actions().unwrap().items[2].1, KeyCode::Char('e'));
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.screen, Screen::ProjectEdit(_)));
        assert_eq!(app.action_cursor, None);
        app.shutdown();
    }

    #[test]
    fn deployment_confirmation_defaults_to_confirm_and_rejects_modified_enter() {
        let (_directory, mut app) = fixture();
        let Screen::Overview { root, config } = app.screen.clone() else {
            unreachable!()
        };
        app.screen = Screen::DeploymentReview {
            plan: crate::application::DeploymentPlan {
                selection: crate::application::DeploymentSelection {
                    project_root: root,
                    config,
                    environment: "production".into(),
                    components: std::collections::BTreeSet::default(),
                },
                activation_order: vec![],
                entries: vec![],
                git: crate::application::GitWorktreeState::Clean,
                git_metadata: crate::application::GitMetadata::default(),
            },
            scroll: 0,
        };
        assert_highlighted_action(&app, "Confirm deployment");
        press(&mut app, KeyCode::PageDown);
        assert_highlighted_action(&app, "Confirm deployment");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL));
        assert!(matches!(app.screen, Screen::DeploymentReview { .. }));
        assert_eq!(
            app.action_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .unwrap()
                .code,
            KeyCode::Char('c')
        );
        assert_eq!(app.action_cursor, None);
    }
    #[test]
    fn projects_ctrl_c_uses_safe_exit() {
        let (_directory, mut app) = fixture();
        app.screen = Screen::Projects;
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(app.exit_state(), super::super::ExitState::Waiting);
        app.shutdown();
        assert!(app.exit_ready());
    }

    #[test]
    fn projects_require_two_consecutive_esc_presses_within_one_second() {
        let (_directory, mut app) = fixture();
        press(&mut app, KeyCode::Esc); // Returning from overview must not arm exit.
        assert!(matches!(app.screen, Screen::Projects));
        assert!(app.project_exit_armed_at.is_none());
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.exit_state(), super::super::ExitState::Running);
        assert!(app.message.as_deref().unwrap().contains("Esc"));
        press(&mut app, KeyCode::Down);
        assert!(app.project_exit_armed_at.is_none());
        press(&mut app, KeyCode::Esc);
        app.project_exit_armed_at = Some(
            std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(2))
                .unwrap(),
        );
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.exit_state(), super::super::ExitState::Running);
        app.handle_key(KeyEvent::new_with_kind(
            KeyCode::Esc,
            KeyModifiers::NONE,
            crossterm::event::KeyEventKind::Repeat,
        ));
        assert_eq!(app.exit_state(), super::super::ExitState::Running);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.exit_state(), super::super::ExitState::Waiting);
    }

    #[test]
    fn closing_project_help_does_not_count_towards_double_escape_exit() {
        let (_directory, mut app) = fixture();
        app.screen = Screen::Projects;
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::F(1));
        press(&mut app, KeyCode::Esc);
        assert!(!app.help_open);
        assert!(app.project_exit_armed_at.is_none());
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.exit_state(), super::super::ExitState::Running);
    }

    #[test]
    fn projects_footer_shows_both_exit_methods() {
        let (_directory, mut app) = fixture();
        app.screen = Screen::Projects;
        app.language = crate::tui::i18n::Language::Chinese;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 10)).unwrap();
        terminal
            .draw(|frame| crate::tui::render(frame, &app))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(text.contains("Ctrl+C"));
        assert!(text.contains("Esc"));
        assert!(text.replace(' ', "").contains("退出"));
    }
}
