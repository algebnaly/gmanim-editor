#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use eframe::egui;
    use crate::cache::{EvictionContext, PreviewFrameCache, forward_distance};
    use crate::playback::{
        advance_playback_time, clamp_playback_time, directional_window,
    };
    use crate::restored_playback_time;
    use crate::SceneId;

    fn test_image() -> std::sync::Arc<egui::ColorImage> {
        std::sync::Arc::new(egui::ColorImage::from_rgba_unmultiplied(
            [1, 1],
            &[0, 0, 0, 255],
        ))
    }

    fn evict_ctx(playhead: u32, total_frames: u32, horizon: u32) -> EvictionContext {
        EvictionContext {
            playhead,
            total_frames,
            forward: true,
            horizon,
        }
    }

    #[test]
    fn cache_honors_horizon() {
        let mut cache = PreviewFrameCache::new(3);
        let ctx = evict_ctx(5, 1200, 48);
        cache.insert(5, test_image(), ctx);
        cache.insert(6, test_image(), ctx);
        cache.insert(20, test_image(), ctx);
        cache.insert(100, test_image(), ctx);
        assert!(cache.contains(5));
        assert!(cache.contains(6));
        assert!(cache.contains(20));
        assert!(!cache.contains(100));
    }

    #[test]
    fn cache_protects_prerendered_frames_across_a_loop_wrap() {
        let mut cache = PreviewFrameCache::new(8);
        for playhead in 0..10u32 {
            let ctx = evict_ctx(playhead, 10, 4);
            cache.load_image(playhead);
            cache.insert((playhead + 3) % 10, test_image(), ctx);
        }
        assert!(cache.contains(0));
        assert!(cache.contains(1));
        let ctx = evict_ctx(0, 10, 4);
        cache.load_image(0);
        cache.insert(3, test_image(), ctx);
        assert!(cache.contains(0));
        assert!(cache.contains(1));
        assert!(!cache.contains(5));
    }

    #[test]
    fn cache_falls_back_to_furthest_frame_when_all_protected() {
        let mut cache = PreviewFrameCache::new(3);
        let ctx = evict_ctx(0, 1200, 48);
        cache.insert(1, test_image(), ctx);
        cache.insert(2, test_image(), ctx);
        cache.insert(3, test_image(), ctx);
        cache.insert(4, test_image(), ctx);
        assert!(cache.contains(1));
        assert!(!cache.contains(4));
    }

    #[test]
    fn forward_distance_wraps_in_playback_direction() {
        assert_eq!(forward_distance(5, 3, 1200, true), 2);
        assert_eq!(forward_distance(1, 1199, 1200, true), 2);
        assert_eq!(forward_distance(1199, 1, 1200, false), 2);
        assert_eq!(forward_distance(3, 5, 1200, false), 2);
        assert_eq!(forward_distance(7, 7, 1200, true), 0);
    }

    #[test]
    fn looping_preserves_forward_and_backward_overshoot() {
        let (forward, playing) = advance_playback_time(9.99, 0.02, 1200, 120, true);
        assert!(playing);
        assert!((forward - 0.01).abs() < 1e-4);

        let (backward, playing) = advance_playback_time(0.01, -0.02, 1200, 120, true);
        assert!(playing);
        assert!((backward - 9.99).abs() < 1e-4);
    }

    #[test]
    fn missing_timeline_does_not_cancel_playback_intent() {
        assert_eq!(advance_playback_time(0.0, 0.01, 0, 120, true), (0.0, true));
    }

    #[test]
    fn restored_playback_time_is_clamped_to_the_new_scene() {
        assert_eq!(clamp_playback_time(5.0, 1200, 120), 5.0);
        assert!((clamp_playback_time(5.0, 10, 10) - 0.9).abs() < 1e-6);
        assert_eq!(clamp_playback_time(5.0, 0, 120), 0.0);
    }

    #[test]
    fn playback_position_belongs_to_the_scene() {
        let scene = SceneId {
            source: "main.py".to_owned(),
            name: "gravity".to_owned(),
        };
        let other_scene = SceneId {
            source: "main.py".to_owned(),
            name: "other".to_owned(),
        };
        let positions = HashMap::from([(scene.clone(), 4.25)]);

        assert_eq!(restored_playback_time(&positions, &scene, 1200, 120), 4.25);
        assert_eq!(
            restored_playback_time(&positions, &other_scene, 1200, 120),
            0.0
        );
    }

    #[test]
    fn playback_selection_never_regresses_outside_a_loop_boundary() {
        let mut cache = PreviewFrameCache::new(8);
        let ctx = evict_ctx(0, 1200, 48);
        for frame in [84, 100, 103, 1199, 0] {
            cache.insert(
                frame,
                std::sync::Arc::new(egui::ColorImage::from_rgba_unmultiplied(
                    [1, 1],
                    &[0, 0, 0, 255],
                )),
                ctx,
            );
        }

        assert_eq!(cache.playback_frame(102, Some(100), true, true), Some(100));
        assert_eq!(cache.playback_frame(0, Some(1199), true, true), Some(0));
        assert_eq!(cache.playback_frame(101, Some(103), false, true), Some(103));
    }

    #[test]
    fn directional_window_uses_effective_end_and_deduplicates_short_loops() {
        assert_eq!(directional_window(8, 10, true, false, 48), vec![8, 9]);
        assert_eq!(directional_window(2, 3, true, true, 48), vec![2, 0, 1]);
        assert_eq!(directional_window(1, 3, false, true, 48), vec![1, 0, 2]);
    }
}
