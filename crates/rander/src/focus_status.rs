#[derive(Default)]
pub enum FocusStatus {
    #[default]
    InputWidget,
    ChatWidget,
    EditorWidget,
    HistoryWidget,
    StatusWidget,
    SettingsWidget,
}

impl FocusStatus {
    pub fn change_focus(&mut self, new_focus: FocusStatus) {
        *self = new_focus;
    }

    pub fn get_current_focus(&self) -> &FocusStatus {
        self
    }
}

impl Eq for FocusStatus {}

impl PartialEq for FocusStatus {
    fn eq(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (FocusStatus::InputWidget, FocusStatus::InputWidget)
                | (FocusStatus::ChatWidget, FocusStatus::ChatWidget)
                | (FocusStatus::EditorWidget, FocusStatus::EditorWidget)
        )
    }
}

pub static CURRENT_FOCUS: FocusStatus = FocusStatus::InputWidget;
