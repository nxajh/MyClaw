//! Keyboard interaction types for QQ Bot inline buttons.

/// Permission metadata for a keyboard button. type=2 means all users can click.
#[derive(Clone, serde::Serialize)]
pub struct ButtonPermission {
    pub r#type: u32,
}

/// What happens when a button is clicked.
#[derive(Clone, serde::Serialize)]
pub struct ButtonAction {
    /// 1 = Callback (INTERACTION_CREATE), 2 = Link (opens URL).
    pub r#type: u32,
    /// Payload delivered in data.resolved.button_data when type=1.
    pub data: String,
    pub permission: ButtonPermission,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub click_limit: Option<u32>,
}

/// Visual rendering of a button.
#[derive(Clone, serde::Serialize)]
pub struct ButtonRenderData {
    pub label: String,
    pub visited_label: String,
    pub style: u32,
}

/// A single button in a keyboard row.
#[derive(Clone, serde::Serialize)]
pub struct Button {
    pub id: String,
    pub render_data: ButtonRenderData,
    pub action: ButtonAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
}

/// A row of buttons.
#[derive(Clone, serde::Serialize)]
pub struct ButtonRow {
    pub buttons: Vec<Button>,
}

/// Keyboard content wrapper.
#[derive(Clone, serde::Serialize)]
pub struct KeyboardContent {
    pub rows: Vec<ButtonRow>,
}

/// A keyboard (grid of button rows). Top-level payload for message body.
#[derive(Clone, serde::Serialize)]
pub struct Keyboard {
    pub content: KeyboardContent,
}

impl Keyboard {
    /// Create a keyboard from a slice of label/value pairs (one row per pair).
    /// Max 5 buttons per row, max 5 rows.
    pub fn from_pairs(pairs: &[(impl AsRef<str>, impl AsRef<str>)]) -> Self {
        let mut rows = Vec::new();
        let mut current_row = Vec::new();
        for (i, (label, value)) in pairs.iter().enumerate() {
            current_row.push(Button {
                id: format!("btn_{}", i),
                render_data: ButtonRenderData {
                    label: label.as_ref().to_string(),
                    visited_label: format!("✓ {}", label.as_ref()),
                    style: 1,
                },
                action: ButtonAction {
                    r#type: 1, // Callback
                    data: value.as_ref().to_string(),
                    permission: ButtonPermission { r#type: 2 }, // All users
                    click_limit: None,
                },
                group_id: None,
            });
            if current_row.len() >= 5 {
                rows.push(ButtonRow {
                    buttons: std::mem::take(&mut current_row),
                });
            }
        }
        if !current_row.is_empty() {
            rows.push(ButtonRow {
                buttons: current_row,
            });
        }
        Self {
            content: KeyboardContent { rows },
        }
    }
}
