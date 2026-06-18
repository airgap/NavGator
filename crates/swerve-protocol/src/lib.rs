//! swerve-protocol — servo-free wire types for swerve's IPC control surface.
//!
//! Kept deliberately independent of the engine (no `servo` dependency) so the
//! external-control protocol stays stable across engine churn. See `docs/FORK.md`
//! and ROADMAP §R2.

/// A command from an external process over the IPC control socket — lets other apps
/// drive the engine (the seed of the "Servo as an external engine" goal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpcCommand {
    Navigate(String),
    NewTab,
    Reload,
    Back,
    Forward,
    SelectTab(usize),
    CloseTab(usize),
}

impl IpcCommand {
    /// Parse one line of the text protocol, e.g. `navigate https://servo.org`.
    pub fn parse(line: &str) -> Option<Self> {
        let mut parts = line.trim().splitn(2, ' ');
        let verb = parts.next()?;
        let arg = parts.next().unwrap_or("").trim();
        Some(match verb {
            "navigate" => IpcCommand::Navigate(arg.to_string()),
            "new-tab" => IpcCommand::NewTab,
            "reload" => IpcCommand::Reload,
            "back" => IpcCommand::Back,
            "forward" => IpcCommand::Forward,
            "select-tab" => IpcCommand::SelectTab(arg.parse().ok()?),
            "close-tab" => IpcCommand::CloseTab(arg.parse().ok()?),
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::IpcCommand;

    #[test]
    fn parses_commands() {
        assert_eq!(
            IpcCommand::parse("navigate https://servo.org"),
            Some(IpcCommand::Navigate("https://servo.org".into()))
        );
        assert_eq!(IpcCommand::parse("new-tab"), Some(IpcCommand::NewTab));
        assert_eq!(IpcCommand::parse("select-tab 2"), Some(IpcCommand::SelectTab(2)));
        assert_eq!(IpcCommand::parse("bogus"), None);
        assert_eq!(IpcCommand::parse("select-tab x"), None);
    }
}
