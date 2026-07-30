pub use gmanim_editor_protocol::{
    EditorCommand, EditorEvent, PREVIEW_SLOT_COUNT, PreviewLayout, PreviewPixelFormat,
    PreviewShmHeader,
};

pub enum ThreadMessage {
    ScenesInfo(Vec<String>),
    SceneReady {
        total_frames: u32,
        width: u32,
        height: u32,
        framerate: u32,
    },
    PreviewOpened,
    FrameReady {
        request_id: u64,
        frame: u32,
        slot: u32,
        image: std::sync::Arc<egui::ColorImage>,
    },
    Error(String),
}
