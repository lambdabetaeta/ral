// LOCAL PATCH: route paste actions through native repeat recording.
use crate::{
    actions::{cpaste::PasteOverSelection, Paste},
    clipboard::ClipboardTrait,
    EditorState,
};

/// Handles a paste event.
#[derive(Clone, Debug, Default)]
pub(crate) struct PasteEventHandler {}
impl PasteEventHandler {
    pub(crate) fn on_event(text: String, state: &mut EditorState) {
        state.clip.set_text(text);
        match state.mode {
            crate::EditorMode::Normal | crate::EditorMode::Insert => state.execute_recorded(Paste.into()),
            crate::EditorMode::Visual => state.execute_recorded(PasteOverSelection.into()),
            crate::EditorMode::Search => {} // TODO: Insert into search
        }
    }
}
