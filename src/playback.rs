use std::collections::HashSet;

pub fn advance_playback_time(
    current_time: f32,
    delta: f32,
    total_frames: u32,
    framerate: u32,
    looping: bool,
) -> (f32, bool) {
    if total_frames == 0 || framerate == 0 {
        return (current_time, true);
    }
    let next_time = current_time + delta;
    if looping {
        let duration = total_frames as f32 / framerate as f32;
        return (next_time.rem_euclid(duration), true);
    }
    let last_frame_time = total_frames.saturating_sub(1) as f32 / framerate as f32;
    if next_time >= last_frame_time {
        (last_frame_time, false)
    } else if next_time <= 0.0 {
        (0.0, false)
    } else {
        (next_time, true)
    }
}

pub fn directional_window(
    desired: u32,
    total_frames: u32,
    forward: bool,
    looping: bool,
    limit: usize,
) -> Vec<u32> {
    if total_frames == 0 {
        return Vec::new();
    }
    let mut frames = Vec::with_capacity(limit.min(total_frames as usize));
    let mut seen = HashSet::with_capacity(limit.min(total_frames as usize));
    for offset in 0..limit as u32 {
        let frame = if forward {
            match desired.checked_add(offset) {
                Some(frame) if frame < total_frames => Some(frame),
                Some(frame) if looping => Some(frame % total_frames),
                _ => None,
            }
        } else if offset <= desired {
            Some(desired - offset)
        } else if looping {
            let wrapped = (offset - desired) % total_frames;
            Some((total_frames - wrapped) % total_frames)
        } else {
            None
        };
        let Some(frame) = frame else {
            break;
        };
        if !seen.insert(frame) {
            break;
        }
        frames.push(frame);
    }
    frames
}

pub fn clamp_playback_time(time: f32, total_frames: u32, framerate: u32) -> f32 {
    let last_frame_time = total_frames.saturating_sub(1) as f32 / framerate.max(1) as f32;
    time.clamp(0.0, last_frame_time)
}
