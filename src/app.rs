use crate::command::Command;

const HELP: &str = "/new  /sessions  /account  /provider  /model  /undo  /redo  /clear  /help  /quit";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub text: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct InputBuffer {
    text: String,
    cursor: usize,
}

impl InputBuffer {
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
}

impl Default for App {
    fn default() -> Self {
        Self {
            input: InputBuffer::default(),
            messages: Vec::new(),
            scroll: 0,
            status: "main · provider not selected · session -".to_owned(),
            should_quit: false,
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

        self.messages.push(Message {
            role: Role::User,
            text: input,
        });
        self.messages.push(Message {
            role: Role::System,
            text: "Provider integration is not connected yet.".to_owned(),
        });
    }

    fn execute_command(&mut self, input: &str) {
        match Command::parse(input) {
            Ok(Command::Quit) => self.should_quit = true,
            Ok(Command::Clear) => self.messages.clear(),
            Ok(Command::Help) => self.push_system(HELP),
            Ok(Command::New) => self.push_system("Session creation arrives in Phase 3."),
            Ok(Command::Sessions) => self.push_system("Session picker arrives in Phase 3."),
            Ok(Command::Undo) | Ok(Command::Redo) => {
                self.push_system("Transactional history arrives in Phase 4.")
            }
            Ok(Command::Account) | Ok(Command::Provider) | Ok(Command::Model) => {
                self.push_system("Provider and account selection arrives in Phase 6.")
            }
            Err(command) => self.push_system(&format!("Unknown command: {command}")),
        }
    }

    fn push_system(&mut self, text: &str) {
        self.messages.push(Message {
            role: Role::System,
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
