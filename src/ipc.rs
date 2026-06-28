use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64};

#[repr(C)]
pub struct ShmHeader {
    pub write_idx: AtomicU64,
    pub read_idx: AtomicU64,
    pub width: u32,
    pub height: u32,
    pub is_rendering: AtomicBool,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "cmd")]
pub enum EditorCommand {
    #[serde(rename = "render_scene")]
    RenderScene { name: String },
    #[serde(rename = "quit")]
    Quit,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "event")]
pub enum EditorEvent {
    #[serde(rename = "scenes_info")]
    ScenesInfo { scenes: Vec<String> },
    #[serde(rename = "start_render")]
    StartRender {
        total_frames: u64,
        width: u32,
        height: u32,
    },
    #[serde(rename = "finish_render")]
    FinishRender,
    #[serde(rename = "error")]
    Error { message: String },
}

pub enum ThreadMessage {
    ScenesInfo(Vec<String>),
    StartRender { total_frames: u32 },
    FrameReady(u64, std::sync::Arc<eframe::egui::ColorImage>),
    FinishRender,
    Error(String),
}
