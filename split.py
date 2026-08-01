import re

with open("src/main.rs", "r") as f:
    content = f.read()

# Modules
mods = """pub mod cache;
pub mod ipc;
pub mod playback;
pub mod syntax;
pub mod tests;

use cache::{EvictionContext, PreviewFrameCache};
use ipc::{
    EditorCommand, EditorEvent, PREVIEW_SLOT_COUNT, PreviewLayout, PreviewPixelFormat,
    PreviewShmHeader, ThreadMessage,
};
use playback::{
    advance_playback_time, clamp_playback_time, directional_window, restored_playback_time,
};
use syntax::Highlighter;"""

# Extract the header
header_match = re.search(r"use std::time::\{Duration, Instant\};\n\n(?:pub )?mod ipc;.*?ThreadMessage,\n\};\n", content, flags=re.DOTALL)
if header_match:
    content = content[:header_match.start()] + "use std::time::{Duration, Instant};\n\n" + mods + "\n" + content[header_match.end():]

# Remove Highlighter
content = re.sub(r"struct Highlighter \{.*?\}\n\nimpl Default for Highlighter \{.*?\}\n\nimpl Highlighter \{.*?\}\n\n", "", content, flags=re.DOTALL)

# Remove math/playback functions
content = re.sub(r"fn advance_playback_time\([^)]+\)\s*->\s*\(f32, bool\) \{.*?\}\n\n", "", content, flags=re.DOTALL)
content = re.sub(r"fn directional_window\([^)]+\)\s*->\s*Vec<u32> \{.*?\}\n\n", "", content, flags=re.DOTALL)
content = re.sub(r"fn steady_in_flight_cap[^}]+\}\n\n", "fn steady_in_flight_cap(scene_fps: u32, rtt_secs: f32) -> usize {\n    let rate = scene_fps.max(1) as f32;\n    ((rate * rtt_secs.max(0.0) * PREVIEW_RTT_HEADROOM).ceil() as usize)\n        .clamp(PREVIEW_STEADY_IN_FLIGHT_MIN, PREVIEW_STEADY_IN_FLIGHT_MAX)\n}\n\n", content)

# Remove EvictionContext, forward_distance, PreviewFrameCache
content = re.sub(r"/// Direction-aware eviction context.*?\n\}\n\n", "", content, flags=re.DOTALL)
content = re.sub(r"/// Distance from the playhead to `frame`.*?\n\}\n\n", "", content, flags=re.DOTALL)
content = re.sub(r"struct PreviewFrameCache \{.*?impl PreviewFrameCache \{.*?\}\n\n", "", content, flags=re.DOTALL)

# Remove clamp_playback_time
content = re.sub(r"fn clamp_playback_time\([^)]+\)\s*->\s*f32 \{.*?\}\n\n", "", content, flags=re.DOTALL)

# Remove #[cfg(test)] mod tests { ... } at the end
content = re.sub(r"#\[cfg\(test\)\]\nmod tests \{.*\}\n$", "", content, flags=re.DOTALL)

with open("src/main.rs", "w") as f:
    f.write(content)
