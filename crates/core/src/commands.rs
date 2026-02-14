use std::sync::atomic::AtomicBool;

pub static EXIT: AtomicBool = AtomicBool::new(false);
pub fn get_exit() -> &'static AtomicBool {
    &EXIT
}
pub fn set_exit() {
    EXIT.store(true, std::sync::atomic::Ordering::Relaxed);
}

pub enum Command {
    Help,
    Exit,
    Cancel,
    History,
    Skills,
    Rules,
    Settings,
    Status,
    Editor,
    Unknown,
}

impl Command {
    pub fn match_command(&mut self, command: String) {
        if command.is_empty() {
            *self = Command::Unknown;
        }
        *self = match command.as_str() {
            "/help" => Command::Help,
            "/exit" => Command::Exit,
            "/cancel" => Command::Cancel,
            "/history" => Command::History,
            "/skills" => Command::Skills,
            "/rules" => Command::Rules,
            "/settings" => Command::Settings,
            "/status" => Command::Status,
            "/editor" => Command::Editor,
            _ => Command::Unknown,
        };
    }
    pub fn execute(&self) {
        if let Command::Exit = self {}
    }
}
