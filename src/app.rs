use std::env;

use crate::command::Command;
use crate::history::{HistoryError, HistoryStore};
use crate::session::{MessageRole, SessionMetadata, SessionStore};

const HELP: &str = "/new  /sessions  /account  /provider  /model  /undo  /redo  /clear  /help  /quit";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: MessageRole,
    pub text: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct InputBuffer {
    text: String,
    cursor: usize,
}

impl InputBuffer {
    #[cfg(test)]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    pub fn insert(&mut self, ch: char) {
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    pub fn backspace(&mut self) {
        let Some(previous) = self.text[..self.cursor].char_indices().next_back().map(|(i, _)| i)
        else {
            return;
        };
        self.text.drain(previous..self.cursor);
        self.cursor = previous;
    }

    pub fn delete(&mut self) {
        if self.cursor == self.text.len() {
            return;
        }
        let next = self.text[self.cursor..]
            .char_indices()
            .nth(1)
            .map_or(self.text.len(), |(i, _)| self.cursor + i);
        self.text.drain(self.cursor..next);
    }

    pub fn move_left(&mut self) {
        if let Some((previous, _)) = self.text[..self.cursor].char_indices().next_back() {
            self.cursor = previous;
        }
    }

    pub fn move_right(&mut self) {
        if self.cursor == self.text.len() {
            return;
        }
        self.cursor = self.text[self.cursor..]
            .char_indices()
            .nth(1)
            .map_or(self.text.len(), |(i, _)| self.cursor + i);
    }

    pub fn move_home(&mut self) {
        self.cursor = self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1);
    }

    pub fn move_end(&mut self) {
        self.cursor = self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |index| self.cursor + index);
    }

    pub fn display_with_cursor(&self) -> String {
        let mut rendered = String::with_capacity(self.text.len() + 3);
        rendered.push_str(&self.text[..self.cursor]);
        rendered.push('▏');
        rendered.push_str(&self.text[self.cursor..]);
        rendered
    }

    pub fn line_count(&self) -> u16 {
        self.text
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count()
            .saturating_add(1)
            .min(u16::MAX as usize) as u16
    }
}

#[derive(Debug)]
pub struct App {
    pub input: InputBuffer,
    pub messages: Vec<Message>,
    pub scroll: u16,
    pub status: String,
    pub should_quit: bool,
    session_store: Option<SessionStore>,
    current_session: Option<SessionMetadata>,
    history: Option<HistoryStore>,
}

impl Default for App {
    fn default() -> Self {
        Self {
            input: InputBuffer::default(),
            messages: Vec::new(),
            scroll: 0,
            status: "main · provider not selected · session -".to_owned(),
            should_quit: false,
            session_store: None,
            current_session: None,
            history: None,
        }
    }
}

impl App {
    pub fn submit(&mut self) {
        if self.input.is_empty() {
            return;
        }

        let input = self.input.take();
        self.scroll = 0;

        if input.trim_start().starts_with('/') {
            self.execute_command(&input);
            return;
        }

        if let Err(error) = self.persist_message(MessageRole::User, &input) {
            self.push_system(&format!("Session persistence failed: {error}"));
        }
        self.messages.push(Message {
            role: MessageRole::User,
            text: input,
        });
        self.messages.push(Message {
            role: MessageRole::Assistant,
            text: "Provider integration is not connected yet.".to_owned(),
        });
    }

    fn execute_command(&mut self, input: &str) {
        match Command::parse(input) {
            Ok(Command::Quit) => self.should_quit = true,
            Ok(Command::Clear) => self.messages.clear(),
            Ok(Command::Help) => self.push_system(HELP),
            Ok(Command::New) => self.new_session(),
            Ok(Command::Sessions) => self.list_sessions(),
            Ok(Command::Undo) => self.undo(),
            Ok(Command::Redo) => self.redo(),
            Ok(Command::Account) | Ok(Command::Provider) | Ok(Command::Model) => {
                self.push_system("Provider and account selection arrives in Phase 6.")
            }
            Err(command) => self.push_system(&format!("Unknown command: {command}")),
        }
    }

    fn new_session(&mut self) {
        if let Err(error) = self.ensure_store() {
            self.push_system(&format!("Cannot open session store: {error}"));
            return;
        }
        let workspace = match env::current_dir() {
            Ok(workspace) => workspace,
            Err(error) => {
                self.push_system(&format!("Cannot resolve workspace: {error}"));
                return;
            }
        };
        let result = self
            .session_store
            .as_ref()
            .expect("session store initialized")
            .create(workspace);
        match result {
            Ok(metadata) => {
                self.messages.clear();
                self.set_current_session(metadata);
                self.push_system("Started a new session.");
            }
            Err(error) => self.push_system(&format!("Cannot create session: {error}")),
        }
    }

    fn list_sessions(&mut self) {
        if let Err(error) = self.ensure_store() {
            self.push_system(&format!("Cannot open session store: {error}"));
            return;
        }
        let result = self
            .session_store
            .as_ref()
            .expect("session store initialized")
            .list();
        match result {
            Ok(list) if list.sessions.is_empty() => self.push_system("No saved sessions."),
            Ok(list) => {
                let mut output = String::new();
                for metadata in list.sessions.iter().take(20) {
                    let id = metadata.id.get(..8).unwrap_or(&metadata.id);
                    output.push_str(id);
                    output.push_str("  ");
                    output.push_str(&metadata.title);
                    output.push('\n');
                }
                if list.sessions.len() > 20 {
                    output.push_str("… more sessions not shown\n");
                }
                if list.skipped_corrupt > 0 {
                    output.push_str(&format!(
                        "{} corrupt session metadata entr{} skipped",
                        list.skipped_corrupt,
                        if list.skipped_corrupt == 1 { "y" } else { "ies" }
                    ));
                }
                self.push_system(output.trim_end());
            }
            Err(error) => self.push_system(&format!("Cannot list sessions: {error}")),
        }
    }

    fn persist_message(&mut self, role: MessageRole, content: &str) -> Result<(), String> {
        self.ensure_store()?;
        if self.current_session.is_none() {
            let workspace = env::current_dir().map_err(|error| error.to_string())?;
            let metadata = self
                .session_store
                .as_ref()
                .expect("session store initialized")
                .create(workspace)
                .map_err(|error| error.to_string())?;
            self.set_current_session(metadata);
        }

        let store = self
            .session_store
            .as_ref()
            .expect("session store initialized");
        let metadata = self
            .current_session
            .as_mut()
            .expect("current session initialized");
        store
            .append_message(&metadata.id, role, content)
            .map_err(|error| error.to_string())?;
        store.touch(metadata).map_err(|error| error.to_string())?;
        Ok(())
    }

    fn ensure_store(&mut self) -> Result<(), String> {
        if self.session_store.is_none() {
            self.session_store = Some(SessionStore::discover().map_err(|error| error.to_string())?);
        }
        Ok(())
    }

    fn set_current_session(&mut self, metadata: SessionMetadata) {
        let id = metadata.id.get(..8).unwrap_or(&metadata.id);
        self.status = format!("main · provider not selected · session {id}");
        self.current_session = Some(metadata);
        self.history = None;
    }

    fn undo(&mut self) {
        if let Err(error) = self.ensure_history() {
            self.push_system(&error);
            return;
        }
        let result = self.history.as_ref().expect("history initialized").undo();
        match result {
            Ok(change_set) => {
                if let Err(error) = self.sync_history_cursor() {
                    self.push_system(&format!("Undo succeeded but metadata update failed: {error}"));
                    return;
                }
                self.push_system(&format!("Undid turn {}.", change_set.id));
            }
            Err(HistoryError::NothingToUndo) => self.push_system("Nothing to undo."),
            Err(error) => self.push_system(&format!("Undo blocked: {error}")),
        }
    }

    fn redo(&mut self) {
        if let Err(error) = self.ensure_history() {
            self.push_system(&error);
            return;
        }
        let result = self.history.as_ref().expect("history initialized").redo();
        match result {
            Ok(change_set) => {
                if let Err(error) = self.sync_history_cursor() {
                    self.push_system(&format!("Redo succeeded but metadata update failed: {error}"));
                    return;
                }
                self.push_system(&format!("Redid turn {}.", change_set.id));
            }
            Err(HistoryError::NothingToRedo) => self.push_system("Nothing to redo."),
            Err(error) => self.push_system(&format!("Redo blocked: {error}")),
        }
    }

    fn ensure_history(&mut self) -> Result<(), String> {
        if self.history.is_some() {
            return Ok(());
        }
        self.ensure_store()?;
        let metadata = self
            .current_session
            .as_ref()
            .ok_or_else(|| "No active session.".to_owned())?;
        let store = self.session_store.as_ref().expect("session store initialized");
        let root = store
            .history_root(&metadata.id)
            .map_err(|error| error.to_string())?;
        let history = HistoryStore::open(root, metadata.workspace.clone())
            .map_err(|error| error.to_string())?;
        self.history = Some(history);
        Ok(())
    }

    fn sync_history_cursor(&mut self) -> Result<(), String> {
        let cursor = self
            .history
            .as_ref()
            .expect("history initialized")
            .state()
            .map_err(|error| error.to_string())?
            .cursor;
        let metadata = self
            .current_session
            .as_mut()
            .ok_or_else(|| "No active session.".to_owned())?;
        metadata.history_cursor = cursor;
        self.session_store
            .as_ref()
            .expect("session store initialized")
            .save_metadata(metadata)
            .map_err(|error| error.to_string())
    }

    fn push_system(&mut self, text: &str) {
        self.messages.push(Message {
            role: MessageRole::System,
            text: text.to_owned(),
        });
    }

    pub fn scroll_up(&mut self, amount: u16) {
        self.scroll = self.scroll.saturating_add(amount);
    }

    pub fn scroll_down(&mut self, amount: u16) {
        self.scroll = self.scroll.saturating_sub(amount);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_editing_respects_char_boundaries() {
        let mut input = InputBuffer::default();
        input.insert('a');
        input.insert('ğ');
        input.insert('🙂');
        input.move_left();
        input.backspace();
        assert_eq!(input.as_str(), "a🙂");
        input.delete();
        assert_eq!(input.as_str(), "a");
    }

    #[test]
    fn multiline_home_and_end_stay_on_current_line() {
        let mut input = InputBuffer::default();
        for ch in "one\ntwo".chars() {
            input.insert(ch);
        }
        input.move_home();
        input.insert('>');
        assert_eq!(input.as_str(), "one\n>two");
        input.move_end();
        input.insert('<');
        assert_eq!(input.as_str(), "one\n>two<");
    }

    #[test]
    fn redo_placeholder_does_not_quit() {
        let mut app = App::default();
        for ch in "/redo".chars() {
            app.input.insert(ch);
        }
        app.submit();
        assert!(!app.should_quit);
        assert_eq!(app.messages.len(), 1);
    }
}
