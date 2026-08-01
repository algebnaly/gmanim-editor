use eframe::egui;
use std::collections::{HashMap, VecDeque};

/// Direction-aware eviction context. Frames within `horizon` of the playhead
/// in playback order belong to the prefetch window and must never be evicted,
/// no matter how long ago they were last touched. The steady-state in-flight
/// cap is deliberately shallow, so a plain LRU would otherwise keep evicting
/// the very next frame to be displayed.
#[derive(Clone, Copy, Debug)]
pub struct EvictionContext {
    pub playhead: u32,
    pub total_frames: u32,
    pub forward: bool,
    pub horizon: u32,
}

/// Distance from the playhead to `frame` in playback order, wrapping around
/// the loop boundary.
pub fn forward_distance(frame: u32, playhead: u32, total_frames: u32, forward: bool) -> u32 {
    if forward {
        if frame >= playhead {
            frame - playhead
        } else {
            total_frames - (playhead - frame)
        }
    } else if playhead >= frame {
        playhead - frame
    } else {
        total_frames - (frame - playhead)
    }
}

pub struct PreviewFrameCache {
    frames: HashMap<u32, std::sync::Arc<egui::ColorImage>>,
    lru: VecDeque<u32>,
    capacity: usize,
}

impl PreviewFrameCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            frames: HashMap::new(),
            lru: VecDeque::new(),
            capacity,
        }
    }

    pub fn contains(&self, frame: u32) -> bool {
        self.frames.contains_key(&frame)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn closest_not_after(&self, frame: u32) -> Option<u32> {
        self.frames
            .keys()
            .copied()
            .filter(|cached| *cached <= frame)
            .max()
    }

    pub fn closest_not_before(&self, frame: u32) -> Option<u32> {
        self.frames
            .keys()
            .copied()
            .filter(|cached| *cached >= frame)
            .min()
    }

    pub fn playback_frame(
        &self,
        desired: u32,
        displayed: Option<u32>,
        forward: bool,
        looping: bool,
    ) -> Option<u32> {
        let candidate = if forward {
            self.closest_not_after(desired)
        } else {
            self.closest_not_before(desired)
        }?;
        let Some(displayed) = displayed else {
            return Some(candidate);
        };
        let crossed_loop_boundary =
            looping && ((forward && desired < displayed) || (!forward && desired > displayed));
        if crossed_loop_boundary
            || (forward && candidate >= displayed)
            || (!forward && candidate <= displayed)
        {
            Some(candidate)
        } else if self.contains(displayed) {
            Some(displayed)
        } else {
            None
        }
    }

    pub fn insert(
        &mut self,
        frame: u32,
        image: std::sync::Arc<egui::ColorImage>,
        eviction: EvictionContext,
    ) {
        if self.frames.insert(frame, image).is_none() {
            self.lru.push_back(frame);
        }
        self.touch(frame);
        while self.frames.len() > self.capacity {
            let candidate = if eviction.total_frames > 0 {
                // Oldest frame outside the protected window...
                self.lru
                    .iter()
                    .copied()
                    .find(|cached| {
                        forward_distance(
                            *cached,
                            eviction.playhead,
                            eviction.total_frames,
                            eviction.forward,
                        ) >= eviction.horizon
                    })
                    // ...or, when everything is protected (tiny scene or a
                    // cache smaller than the window), the frame furthest from
                    // the playhead.
                    .or_else(|| {
                        self.lru.iter().copied().max_by_key(|cached| {
                            forward_distance(
                                *cached,
                                eviction.playhead,
                                eviction.total_frames,
                                eviction.forward,
                            )
                        })
                    })
            } else {
                self.lru.front().copied()
            };
            let Some(evicted) = candidate else {
                break;
            };
            self.lru.retain(|cached| *cached != evicted);
            self.frames.remove(&evicted);
        }
    }

    pub fn load_image(&mut self, frame: u32) -> Option<std::sync::Arc<egui::ColorImage>> {
        if !self.frames.contains_key(&frame) {
            return None;
        }
        self.touch(frame);
        self.frames.get(&frame).cloned()
    }

    fn touch(&mut self, frame: u32) {
        self.lru.retain(|cached| *cached != frame);
        self.lru.push_back(frame);
    }
}
