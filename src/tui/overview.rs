use super::{
    app::App,
    i18n::{self, Language},
    presentation::{path_label, safe_text},
};
use crate::config::ProjectConfig;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap},
};
use std::path::Path;
fn block(title: &'static str) -> Block<'static> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .padding(Padding::horizontal(1))
}
pub(super) fn render(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    root: &Path,
    config: &ProjectConfig,
) {
    let area = area.inner(Margin::new(
        u16::from(area.width >= 40),
        u16::from(area.height >= 22),
    ));
    let language = app.language;
    if area.height < 14 {
        let content = format!(
            "{}
{}

{}",
            safe_text(&config.project),
            path_label(root),
            app.overview_targets(config)
        );
        frame.render_widget(
            Paragraph::new(content)
                .block(block(language.choose(" Project overview ", " 项目概览 ")))
                .wrap(Wrap { trim: false })
                .scroll((app.overview_scroll(), 0)),
            area,
        );
        return;
    }
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(if area.width >= 78 { 4 } else { 5 }),
        ])
        .split(area);
    let info = vec![
        Line::from(Span::styled(
            safe_text(&config.project),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(path_label(root)),
        Line::from(format!(
            "{} {}    {} {}",
            config.components.len(),
            language.choose("components", "个组件"),
            config.environments.len(),
            language.choose("environments", "个环境")
        )),
    ];
    frame.render_widget(
        Paragraph::new(info).block(block(language.choose(" Project ", " 项目 "))),
        areas[0],
    );
    render_targets(frame, areas[2], app, config);
    render_actions(frame, areas[4], language);
}
fn render_targets(frame: &mut Frame<'_>, area: Rect, app: &App, config: &ProjectConfig) {
    let language = app.language;
    if area.width >= 90 {
        let environment = app.preferred_environment(config);
        let rows = environment
            .as_ref()
            .and_then(|name| config.environments.get(name))
            .map(|env| {
                env.components
                    .iter()
                    .skip(
                        usize::from(app.overview_scroll())
                            .min(env.components.len().saturating_sub(1)),
                    )
                    .map(|(name, target)| {
                        let label = app.destination_label(&target.destination);
                        let endpoint = label.split(" · ").next().unwrap_or(&label).to_owned();
                        ratatui::widgets::Row::new([
                            name.to_string(),
                            endpoint,
                            safe_text(&target.root),
                        ])
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let title = format!(
            " {} · {} ",
            language.choose("Deployment targets", "部署目标"),
            environment.as_deref().unwrap_or("—")
        );
        let table = ratatui::widgets::Table::new(
            rows,
            [
                Constraint::Percentage(18),
                Constraint::Percentage(32),
                Constraint::Percentage(50),
            ],
        )
        .header(
            ratatui::widgets::Row::new([
                language.choose("Component", "组件"),
                language.choose("Connection", "连接"),
                language.choose("Directory", "目录"),
            ])
            .style(Style::default().fg(Color::Cyan)),
        )
        .column_spacing(1)
        .block(block("").title(title));
        frame.render_widget(table, area);
    } else {
        let environment = app.preferred_environment(config);
        let mut lines = Vec::new();
        if let Some(env) = environment
            .as_ref()
            .and_then(|name| config.environments.get(name))
        {
            for (name, target) in &env.components {
                let label = app.destination_label(&target.destination);
                let endpoint = label.split(" · ").next().unwrap_or(&label);
                lines.push(Line::from(format!("{name} → {endpoint}")));
                lines.push(Line::from(format!("  {}", safe_text(&target.root))));
            }
        }
        frame.render_widget(
            Paragraph::new(lines)
                .block(block(language.choose(
                    " Deployment targets · ←/→ environment ",
                    " 部署目标 · ←/→ 切换环境 ",
                )))
                .wrap(Wrap { trim: false })
                .scroll((app.overview_scroll(), 0)),
            area,
        );
    }
}
fn render_actions(frame: &mut Frame<'_>, area: Rect, language: Language) {
    let buttons = [
        (
            " d ",
            language.choose("Deploy", "发布"),
            language.choose("Select components & preview", "选择组件并预览"),
        ),
        (
            " m ",
            language.choose("History & recovery", "历史与恢复"),
            language.choose("Inspect status or plan rollback", "查看状态或计划回退"),
        ),
        (
            " e ",
            language.choose("Configuration", "配置"),
            language.choose("Edit local project settings", "编辑本地项目配置"),
        ),
    ];
    let actions = area;
    let outer = block(language.choose(" Actions ", " 操作 "));
    let inner = outer.inner(actions);
    frame.render_widget(outer, actions);
    if area.width >= 78 {
        let columns = Layout::horizontal([
            Constraint::Percentage(33),
            Constraint::Percentage(34),
            Constraint::Percentage(33),
        ])
        .split(inner);
        for ((key, title, detail), cell) in buttons.into_iter().zip(columns.iter()) {
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(vec![
                        Span::styled(
                            key,
                            Style::default()
                                .fg(Color::Black)
                                .bg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(format!(" {title}")),
                    ]),
                    Line::from(Span::styled(detail, Style::default().fg(Color::DarkGray))),
                ]),
                *cell,
            );
        }
    } else {
        frame.render_widget(
            Paragraph::new(
                buttons
                    .map(|(key, title, _)| {
                        Line::from(vec![
                            Span::styled(
                                key,
                                Style::default()
                                    .fg(Color::Cyan)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::raw(format!(" {title}")),
                        ])
                    })
                    .to_vec(),
            ),
            inner,
        );
    }
}
pub(super) fn render_language(frame: &mut Frame<'_>, app: &App) {
    let Some(selected) = app.language_menu else {
        return;
    };
    let bounds = frame.area();
    let width = bounds.width.min(56);
    let height = bounds.height.min(9);
    let area = Rect::new(
        bounds.x + (bounds.width - width) / 2,
        bounds.y + (bounds.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, area);
    let rows = vec![
        Line::from(""),
        super::selected_line(selected == Language::English, "English"),
        super::selected_line(selected == Language::Chinese, "简体中文"),
        Line::from(""),
        Line::from(i18n::choose(
            "↑/↓ Select · Enter Save · Esc Cancel",
            "↑/↓ 选择 · Enter 保存 · Esc 取消",
        )),
    ];
    frame.render_widget(Paragraph::new(rows).block(block(" Language / 语言 ")), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overview_separates_actions_and_localizes_labels_without_modifying_values() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("shipforge.yaml"),
            include_str!("../../docs/examples/shipforge.yaml"),
        )
        .unwrap();
        let crate::config::ProjectConfigState::Loaded(config) =
            crate::config::load(directory.path()).unwrap()
        else {
            panic!("config")
        };
        let mut app = App::new(
            directory.path().join("projects.yaml"),
            directory.path().join("destinations.yaml"),
            directory.path(),
        )
        .unwrap();
        app.language = Language::Chinese;
        app.screen = super::super::app::Screen::Overview {
            root: directory.path().into(),
            config,
        };
        for (width, height) in [(110, 28), (80, 24), (40, 16), (20, 8), (1, 1)] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| super::super::render(frame, &app))
                .unwrap();
            if width >= 80 {
                let buffer = terminal.backend().buffer();
                let lines = (0..height)
                    .map(|y| {
                        (0..width)
                            .map(|x| buffer[(x, y)].symbol())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>();
                let actions = lines
                    .iter()
                    .position(|line| line.replace(' ', "").contains("操作"))
                    .unwrap_or_else(|| panic!("{width}x{height}: {}", lines.join("\n")));
                assert!(
                    lines[actions - 1].trim().is_empty(),
                    "actions must have a separate gutter"
                );
                assert!(lines.join("\n").replace(' ', "").contains("发布"));
                assert!(lines.join("\n").contains("production"));
                assert!(
                    !lines
                        .join("\n")
                        .contains("Select Components and preview deployment")
                );
            }
        }
    }
}
