#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    New,
    Sessions,
    Account,
    Provider,
    Model,
    Undo,
    Redo,
    Clear,
    Help,
    Quit,
}

impl Command {
    pub fn parse(input: &str) -> Result<Self, &str> {
        match input.trim() {
            "/new" => Ok(Self::New),
            "/sessions" => Ok(Self::Sessions),
            "/account" => Ok(Self::Account),
            "/provider" => Ok(Self::Provider),
            "/model" => Ok(Self::Model),
            "/undo" => Ok(Self::Undo),
            "/redo" => Ok(Self::Redo),
            "/clear" => Ok(Self::Clear),
            "/help" => Ok(Self::Help),
            "/quit" => Ok(Self::Quit),
            other => Err(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_commands() {
        assert_eq!(Command::parse("/new"), Ok(Command::New));
        assert_eq!(Command::parse("  /undo  "), Ok(Command::Undo));
        assert_eq!(Command::parse("/quit"), Ok(Command::Quit));
    }

    #[test]
    fn rejects_unknown_commands() {
        assert_eq!(Command::parse("/explode"), Err("/explode"));
    }
}
