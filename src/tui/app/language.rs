use super::{App, KeyCode, KeyEvent, KeyModifiers};
impl App {
    pub(super) fn handle_language_key(&mut self, key: KeyEvent) -> bool {
        if let Some(selected) = self.language_menu {
            if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
                self.language_menu = None;
                self.request_deployment_cancellation();
                return true;
            }
            if key.modifiers.is_empty() {
                match key.code {
                    KeyCode::Up | KeyCode::Down => self.language_menu = Some(selected.other()),
                    KeyCode::Esc | KeyCode::F(6) => self.language_menu = None,
                    KeyCode::Enter => {
                        if selected
                            .save(&self.registry_path.with_file_name("ui.json"))
                            .is_ok()
                        {
                            self.language = selected;
                            self.language_menu = None;
                            self.message = Some(
                                selected
                                    .choose("Language saved.", "语言设置已保存。")
                                    .into(),
                            );
                        } else {
                            self.message = Some(
                                self.language
                                    .choose(
                                        "Language could not be saved; selection was not applied.",
                                        "语言设置保存失败，未应用此次选择。",
                                    )
                                    .into(),
                            );
                        }
                    }
                    _ => (),
                }
            }
            return true;
        }
        // F2 already cycles hosts in connection forms. Use F6 globally for language.
        if key.code == KeyCode::F(6) && key.modifiers.is_empty() {
            self.language_menu = Some(self.language);
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::i18n::Language;
    fn press(app: &mut App, code: KeyCode) {
        assert!(!app.handle_key(KeyEvent::new(code, KeyModifiers::NONE)));
    }
    #[test]
    fn language_selection_saves_immediately_and_cancel_keeps_the_previous_language() {
        let directory = tempfile::tempdir().unwrap();
        let projects = directory.path().join("projects.yaml");
        let connections = directory.path().join("destinations.yaml");
        let mut app = App::new(projects.clone(), connections.clone(), directory.path()).unwrap();
        press(&mut app, KeyCode::F(6));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.language, Language::English);
        assert!(!directory.path().join("ui.json").exists());
        press(&mut app, KeyCode::F(6));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.language, Language::Chinese);
        assert!(app.language_menu.is_none());
        let reopened = App::new(projects, connections, directory.path()).unwrap();
        assert_eq!(reopened.language, Language::Chinese);
        assert!(!directory.path().join("shipforge.yaml").exists());
        assert!(!directory.path().join("credentials.yaml").exists());
    }
    #[test]
    fn language_modal_owns_action_keys_and_preserves_underlying_help() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = App::new(
            directory.path().join("projects.yaml"),
            directory.path().join("destinations.yaml"),
            directory.path(),
        )
        .unwrap();
        press(&mut app, KeyCode::F(1));
        assert!(app.help_open);
        press(&mut app, KeyCode::F(6));
        for key in ['d', 'c', 'e', 'q'] {
            press(&mut app, KeyCode::Char(key));
            assert!(matches!(app.screen, super::super::Screen::Projects));
            assert_eq!(app.language_menu, Some(Language::English));
        }
        press(&mut app, KeyCode::Esc);
        assert!(app.help_open);
        assert!(app.language_menu.is_none());
        assert!(!directory.path().join("ui.json").exists());
    }
    #[test]
    fn failed_language_save_leaves_menu_and_current_language_intact() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = App::new(
            directory.path().join("projects.yaml"),
            directory.path().join("destinations.yaml"),
            directory.path(),
        )
        .unwrap();
        std::fs::create_dir(directory.path().join("ui.json")).unwrap();
        press(&mut app, KeyCode::F(6));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.language, Language::English);
        assert_eq!(app.language_menu, Some(Language::Chinese));
        assert!(app.message.unwrap().contains("could not be saved"));
    }
}
