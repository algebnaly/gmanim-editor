use eframe::egui;
use interprocess::TryClone;
use interprocess::local_socket::{
    GenericFilePath, GenericNamespaced, ListenerOptions, prelude::*, traits::Listener,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::Ordering;

pub mod ipc;
use ipc::{
    EditorCommand, EditorEvent, PREVIEW_SLOT_COUNT, PreviewLayout, PreviewPixelFormat,
    PreviewShmHeader, ThreadMessage,
};

const PREVIEW_CACHE_BUDGET_BYTES: usize = 512 * 1024 * 1024;
const PREVIEW_CACHE_MIN_FRAMES: usize = PREVIEW_SLOT_COUNT as usize;
const PREVIEW_CACHE_MAX_FRAMES: usize = 256;
const PREVIEW_BUFFER_LOW_WATER: usize = 32;
const PREVIEW_BUFFER_HIGH_WATER: usize = 48;
const PREVIEW_COMPLETIONS_PER_UI_UPDATE: usize = 4;

fn producer_state(active: bool, buffered: usize, low_water: usize, high_water: usize) -> bool {
    if active {
        buffered < high_water
    } else {
        buffered < low_water
    }
}

fn buffered_producer_state(
    active: bool,
    primed: bool,
    buffered: usize,
    low_water: usize,
    high_water: usize,
) -> (bool, bool) {
    let primed = primed || buffered >= high_water;
    let active = if primed {
        buffered < high_water
    } else {
        producer_state(active, buffered, low_water, high_water)
    };
    (active, primed)
}

fn advance_playback_time(
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

fn directional_window(
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

struct RateMeter {
    window_started: std::time::Instant,
    samples: u32,
    rate: f32,
}

impl RateMeter {
    fn new() -> Self {
        Self {
            window_started: std::time::Instant::now(),
            samples: 0,
            rate: 0.0,
        }
    }

    fn reset(&mut self) {
        *self = Self::new();
    }

    fn sample(&mut self) {
        self.samples += 1;
        let elapsed = self.window_started.elapsed();
        if elapsed >= std::time::Duration::from_millis(500) {
            self.rate = self.samples as f32 / elapsed.as_secs_f32();
            self.window_started = std::time::Instant::now();
            self.samples = 0;
        }
    }

    fn rate(&self) -> f32 {
        if self.samples == 0 && self.window_started.elapsed() >= std::time::Duration::from_secs(1) {
            0.0
        } else {
            self.rate
        }
    }
}

struct PreviewFrameCache {
    frames: HashMap<u32, std::sync::Arc<egui::ColorImage>>,
    lru: VecDeque<u32>,
    capacity: usize,
}

impl PreviewFrameCache {
    fn new(capacity: usize) -> Self {
        Self {
            frames: HashMap::new(),
            lru: VecDeque::new(),
            capacity,
        }
    }

    fn contains(&self, frame: u32) -> bool {
        self.frames.contains_key(&frame)
    }

    fn len(&self) -> usize {
        self.frames.len()
    }

    fn capacity(&self) -> usize {
        self.capacity
    }

    fn closest_not_after(&self, frame: u32) -> Option<u32> {
        self.frames
            .keys()
            .copied()
            .filter(|cached| *cached <= frame)
            .max()
    }

    fn closest_not_before(&self, frame: u32) -> Option<u32> {
        self.frames
            .keys()
            .copied()
            .filter(|cached| *cached >= frame)
            .min()
    }

    fn playback_frame(
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

    fn insert(&mut self, frame: u32, image: std::sync::Arc<egui::ColorImage>) {
        if self.frames.insert(frame, image).is_none() {
            self.lru.push_back(frame);
        }
        self.touch(frame);
        while self.frames.len() > self.capacity {
            if let Some(evicted) = self.lru.pop_front() {
                self.frames.remove(&evicted);
            }
        }
    }

    fn load_image(&mut self, frame: u32) -> Option<std::sync::Arc<egui::ColorImage>> {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PreviewRequest {
    id: u64,
    frame: u32,
    slot: u32,
}

#[derive(Clone)]
struct PreviewReadbackConfig {
    shm_id: String,
    layout: PreviewLayout,
}

fn read_preview_image(
    shmem: &shared_memory::Shmem,
    layout: PreviewLayout,
    request_id: u64,
    frame: u32,
    slot: u32,
) -> Result<std::sync::Arc<egui::ColorImage>, String> {
    let header = unsafe { &*(shmem.as_ptr() as *const PreviewShmHeader) };
    if header.published(slot) != Some((request_id, frame)) {
        return Err("preview publication does not match the completed request".to_owned());
    }
    if header.layout().map_err(|error| error.to_string())? != layout {
        return Err("preview shared-memory layout changed unexpectedly".to_owned());
    }
    let width = layout.width as usize;
    let height = layout.height as usize;
    let packed_stride = width * 4;
    let source = unsafe { shmem.as_ptr().add(layout.frame_offset(u64::from(slot))) };
    let opaque = (0..height).all(|row| {
        let source_row = unsafe {
            std::slice::from_raw_parts(source.add(row * layout.stride as usize), packed_stride)
        };
        source_row.chunks_exact(4).all(|pixel| pixel[3] == 255)
    });
    let mut pixels = vec![egui::Color32::TRANSPARENT; width * height];
    if opaque {
        debug_assert_eq!(std::mem::size_of::<egui::Color32>(), 4);
        for row in 0..height {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    source.add(row * layout.stride as usize),
                    pixels.as_mut_ptr().cast::<u8>().add(row * packed_stride),
                    packed_stride,
                );
            }
        }
    } else {
        for row in 0..height {
            let source_row = unsafe {
                std::slice::from_raw_parts(source.add(row * layout.stride as usize), packed_stride)
            };
            for (column, pixel) in source_row.chunks_exact(4).enumerate() {
                pixels[row * width + column] =
                    egui::Color32::from_rgba_unmultiplied(pixel[0], pixel[1], pixel[2], pixel[3]);
            }
        }
    }
    Ok(std::sync::Arc::new(egui::ColorImage::new(
        [width, height],
        pixels,
    )))
}

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let project_dir = if args.len() > 1 {
        std::path::PathBuf::from(&args[1])
    } else {
        std::env::current_dir().unwrap_or_default()
    };

    std::env::set_current_dir(&project_dir).unwrap_or_else(|e| {
        eprintln!("Failed to change directory to {:?}: {}", project_dir, e);
    });

    if std::path::Path::new("pyproject.toml").exists() {
        let _ = std::process::Command::new("uv").arg("sync").status();
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 720.0])
            .with_title("Gmanim Editor"),
        ..Default::default()
    };

    eframe::run_native(
        "Gmanim Editor",
        options,
        Box::new(|cc| Ok(Box::new(GmanimEditorApp::new(cc)))),
    )
}

struct GmanimEditorApp {
    python_script: String,
    current_file: String,
    available_files: Vec<String>,
    execution_result: String,

    // IPC
    ipc_rx: Option<std::sync::mpsc::Receiver<ThreadMessage>>,
    ipc_tx_cmd: Option<std::sync::mpsc::Sender<EditorCommand>>,
    subprocess: Option<std::process::Child>,
    run_counter: u32,
    keep_running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ipc_threads: Vec<std::thread::JoinHandle<()>>,
    ipc_event_tx: Option<std::sync::mpsc::Sender<ThreadMessage>>,

    preview_shmem: Option<shared_memory::Shmem>,
    preview_readback_config: std::sync::Arc<std::sync::Mutex<Option<PreviewReadbackConfig>>>,
    preview_cache: Option<PreviewFrameCache>,
    preview_ready: bool,
    displayed_frame: Option<u32>,
    desired_frame: u32,
    in_flight_requests: HashMap<u64, PreviewRequest>,
    free_preview_slots: VecDeque<u32>,
    next_request_id: u64,
    prefetch_queue: VecDeque<u32>,
    producer_active: bool,
    preview_buffer_primed: bool,
    render_failed: bool,
    total_frames_to_render: u32,
    texture_handle: Option<egui::TextureHandle>,
    has_project: bool,
    is_playing: bool,
    current_time: f32,
    preview_framerate: u32,
    preview_size: (u32, u32),
    available_scenes: Vec<String>,
    selected_scene: String,
    _watcher: Option<notify::RecommendedWatcher>,
    file_changed_rx: std::sync::mpsc::Receiver<()>,
    pending_file_reload: Option<std::time::Instant>,
    time_scale: f32,
    render_rate: RateMeter,
    display_rate: RateMeter,
    skipped_timeline_frames: u64,
    is_looping: bool,
    show_editor: bool,
}

impl GmanimEditorApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut fonts = egui::FontDefinitions::default();
        let font_paths = [
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/wqy-microhei/wqy-microhei.ttc",
        ];

        for path in font_paths.iter() {
            if let Ok(font_data) = std::fs::read(path) {
                fonts.font_data.insert(
                    "cjk_font".to_owned(),
                    std::sync::Arc::new(egui::FontData::from_owned(font_data)),
                );
                if let Some(vec) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                    vec.insert(0, "cjk_font".to_owned());
                }
                if let Some(vec) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
                    vec.push("cjk_font".to_owned());
                }
                break;
            }
        }
        cc.egui_ctx.set_fonts(fonts);

        let has_project = std::path::Path::new(".venv").exists();

        let mut available_files = Vec::new();
        if let Ok(entries) = std::fs::read_dir(".") {
            for entry in entries.flatten() {
                if let Some(ext) = entry.path().extension() {
                    if ext == "py" {
                        if let Some(name) = entry.file_name().to_str() {
                            available_files.push(name.to_string());
                        }
                    }
                }
            }
        }
        available_files.sort();

        let mut current_file = "main.py".to_string();
        if !available_files.contains(&current_file) && !available_files.is_empty() {
            current_file = available_files[0].clone();
        }

        let mut script = String::new();
        if has_project {
            if std::path::Path::new(&current_file).exists() {
                if let Ok(content) = std::fs::read_to_string(&current_file) {
                    script = content;
                }
            }
        }

        let (tx, rx) = std::sync::mpsc::channel();
        let ctx_clone = cc.egui_ctx.clone();

        let mut watcher =
            notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
                if let Ok(event) = res {
                    if event
                        .paths
                        .iter()
                        .any(|p| p.extension().map_or(false, |e| e == "py"))
                    {
                        let _ = tx.send(());
                        ctx_clone.request_repaint();
                    }
                }
            })
            .ok();

        if let Some(w) = &mut watcher {
            use notify::Watcher;
            let _ = w.watch(std::path::Path::new("."), notify::RecursiveMode::Recursive);
        }

        let mut app = Self {
            python_script: script,
            current_file,
            available_files,
            execution_result: String::new(),

            ipc_rx: None,
            ipc_tx_cmd: None,
            subprocess: None,
            run_counter: 0,
            keep_running: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            ipc_threads: Vec::new(),
            ipc_event_tx: None,

            texture_handle: None,
            preview_shmem: None,
            preview_readback_config: std::sync::Arc::new(std::sync::Mutex::new(None)),
            preview_cache: None,
            preview_ready: false,
            displayed_frame: None,
            desired_frame: 0,
            in_flight_requests: HashMap::new(),
            free_preview_slots: (0..PREVIEW_SLOT_COUNT).collect(),
            next_request_id: 1,
            prefetch_queue: VecDeque::new(),
            producer_active: false,
            preview_buffer_primed: false,
            render_failed: false,
            total_frames_to_render: 0,
            has_project,
            is_playing: true,
            current_time: 0.0,
            preview_framerate: 60,
            preview_size: (16, 9),
            available_scenes: Vec::new(),
            selected_scene: String::new(),
            _watcher: watcher,
            file_changed_rx: rx,
            pending_file_reload: None,
            time_scale: 1.0,
            render_rate: RateMeter::new(),
            display_rate: RateMeter::new(),
            skipped_timeline_frames: 0,
            is_looping: true,
            show_editor: true,
        };

        if app.has_project {
            app.run_python(&cc.egui_ctx);
        }

        app
    }

    fn seek_to(&mut self, target_time: f32) {
        self.current_time = target_time;
    }

    fn run_python(&mut self, ctx: &egui::Context) {
        if let Some(mut child) = self.subprocess.take() {
            let _ = child.kill();
            let _ = child.wait();
        }

        // Clean up old threads
        self.keep_running.store(false, Ordering::Release);
        while let Some(handle) = self.ipc_threads.pop() {
            let _ = handle.join();
        }
        self.keep_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));

        self.run_counter += 1;
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();

        let ctrl_socket_name = if cfg!(windows) {
            format!(r"\\.\pipe\gmanim_ctrl_{}_{}", std::process::id(), timestamp)
        } else {
            format!("/tmp/gmanim_ctrl_{}_{}", std::process::id(), timestamp)
        };

        // Remove old UDS file on Unix
        if !cfg!(windows) {
            let _ = std::fs::remove_file(&ctrl_socket_name);
        }

        let socket_name = if cfg!(windows) {
            ctrl_socket_name
                .clone()
                .to_ns_name::<GenericNamespaced>()
                .unwrap()
        } else {
            ctrl_socket_name
                .clone()
                .to_fs_name::<GenericFilePath>()
                .unwrap()
        };

        let listener = match ListenerOptions::new().name(socket_name).create_sync() {
            Ok(l) => l,
            Err(e) => {
                self.execution_result = format!("Failed to create IPC socket: {}", e);
                return;
            }
        };

        let python_exe = if cfg!(windows) {
            ".venv\\Scripts\\python.exe"
        } else {
            ".venv/bin/python"
        };

        let mut child = match std::process::Command::new(python_exe)
            .arg("-m")
            .arg("gmanim.editor_runner")
            .arg(&self.current_file)
            .arg("--ctrl-socket")
            .arg(&ctrl_socket_name)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                self.execution_result = format!("Failed to start python: {}", e);
                return;
            }
        };

        let (tx, rx) = std::sync::mpsc::channel();
        self.ipc_rx = Some(rx);
        self.ipc_event_tx = Some(tx.clone());

        if let Some(mut stderr) = child.stderr.take() {
            let tx_err = tx.clone();
            let ctx_err = ctx.clone();
            let handle_err = std::thread::spawn(move || {
                use std::io::Read;
                let mut s = String::new();
                let _ = stderr.read_to_string(&mut s);
                if !s.trim().is_empty() {
                    let mut file = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open("editor.log")
                        .unwrap_or_else(|_| std::fs::File::create("editor.log").unwrap());
                    use std::io::Write;
                    let _ = file.write_all(s.as_bytes());
                    let _ = tx_err.send(crate::ipc::ThreadMessage::Error(s));
                    ctx_err.request_repaint();
                }
            });
            self.ipc_threads.push(handle_err);
        }

        if let Some(mut stdout) = child.stdout.take() {
            let handle_out = std::thread::spawn(move || {
                use std::io::Read;
                let mut s = String::new();
                let _ = stdout.read_to_string(&mut s);
                if !s.trim().is_empty() {
                    let mut file = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open("editor.log")
                        .unwrap_or_else(|_| std::fs::File::create("editor.log").unwrap());
                    use std::io::Write;
                    let _ = file.write_all(s.as_bytes());
                }
            });
            self.ipc_threads.push(handle_out);
        }

        self.subprocess = Some(child);
        self.execution_result = "Running script...".to_owned();

        // Clear view
        self.texture_handle = None;
        self.displayed_frame = None;
        self.current_time = 0.0;
        self.preview_shmem = None;
        *self.preview_readback_config.lock().unwrap() = None;
        self.preview_cache = None;
        self.preview_ready = false;
        self.desired_frame = 0;
        self.in_flight_requests.clear();
        self.free_preview_slots = (0..PREVIEW_SLOT_COUNT).collect();
        self.prefetch_queue.clear();
        self.producer_active = false;
        self.preview_buffer_primed = false;
        self.render_rate.reset();
        self.display_rate.reset();
        self.skipped_timeline_frames = 0;
        self.render_failed = false;
        self.total_frames_to_render = 0;

        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<EditorCommand>();
        self.ipc_tx_cmd = Some(cmd_tx);

        let ctx_clone = ctx.clone();
        let preview_readback_config = self.preview_readback_config.clone();

        let tx_listen = tx.clone();
        let keep_running_listen = self.keep_running.clone();
        let handle1 = std::thread::spawn(move || {
            let _ = listener
                .set_nonblocking(interprocess::local_socket::ListenerNonblockingMode::Accept);
            let mut conn = loop {
                if !keep_running_listen.load(Ordering::Acquire) {
                    return;
                }
                match listener.accept() {
                    Ok(c) => break c,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    Err(e) => {
                        let _ =
                            tx_listen.send(ThreadMessage::Error(format!("Accept failed: {}", e)));
                        return;
                    }
                }
            };
            let _ = conn.set_nonblocking(false);

            let mut reader = BufReader::new(conn.try_clone().unwrap());

            // Read events
            let tx_clone = tx_listen.clone();
            let ctx_clone_read = ctx_clone.clone();
            let keep_running_read = keep_running_listen.clone();
            let _handle2 = std::thread::spawn(move || {
                let mut line = String::new();
                let mut preview_readback: Option<(String, shared_memory::Shmem, PreviewLayout)> =
                    None;
                while keep_running_read.load(Ordering::Acquire) {
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            if let Ok(event) = serde_json::from_str::<EditorEvent>(&line) {
                                match event {
                                    EditorEvent::ScenesInfo { scenes } => {
                                        let _ = tx_clone.send(ThreadMessage::ScenesInfo(scenes));
                                    }
                                    EditorEvent::SceneReady {
                                        total_frames,
                                        width,
                                        height,
                                        framerate,
                                    } => {
                                        let _ = tx_clone.send(ThreadMessage::SceneReady {
                                            total_frames: total_frames as u32,
                                            width,
                                            height,
                                            framerate,
                                        });
                                    }
                                    EditorEvent::PreviewOpened => {
                                        let _ = tx_clone.send(ThreadMessage::PreviewOpened);
                                    }
                                    EditorEvent::FrameReady {
                                        request_id,
                                        frame,
                                        slot,
                                    } => {
                                        let config =
                                            preview_readback_config.lock().unwrap().clone();
                                        let result = config
                                            .ok_or_else(|| {
                                                "preview readback is not configured".to_owned()
                                            })
                                            .and_then(|config| {
                                                let needs_open = preview_readback
                                                    .as_ref()
                                                    .is_none_or(|(id, _, _)| *id != config.shm_id);
                                                if needs_open {
                                                    let shmem = shared_memory::ShmemConf::new()
                                                        .os_id(&config.shm_id)
                                                        .open()
                                                        .map_err(|error| error.to_string())?;
                                                    preview_readback =
                                                        Some((config.shm_id, shmem, config.layout));
                                                }
                                                let (_, shmem, layout) =
                                                    preview_readback.as_ref().unwrap();
                                                read_preview_image(
                                                    shmem, *layout, request_id, frame, slot,
                                                )
                                            });
                                        match result {
                                            Ok(image) => {
                                                let _ = tx_clone.send(ThreadMessage::FrameReady {
                                                    request_id,
                                                    frame,
                                                    slot,
                                                    image,
                                                });
                                            }
                                            Err(message) => {
                                                let _ =
                                                    tx_clone.send(ThreadMessage::Error(message));
                                            }
                                        }
                                    }
                                    EditorEvent::Error { message } => {
                                        let _ = tx_clone.send(ThreadMessage::Error(message));
                                    }
                                }
                                ctx_clone_read.request_repaint();
                            }
                            line.clear();
                        }
                    }
                }
            });

            // Send commands
            while keep_running_listen.load(Ordering::Acquire) {
                if let Ok(cmd) = cmd_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                    let mut s = serde_json::to_string(&cmd).unwrap();
                    s.push('\n');
                    if conn.write_all(s.as_bytes()).is_err() {
                        break;
                    }
                    if let EditorCommand::Quit = cmd {
                        break;
                    }
                }
            }
            // we don't strictly need to join handle2 here, but we could.
        });

        self.ipc_threads.push(handle1);
    }

    fn prepare_preview(
        &mut self,
        total_frames: u32,
        width: u32,
        height: u32,
        framerate: u32,
    ) -> Result<(), String> {
        let layout = PreviewLayout::packed_rgba(width, height, PREVIEW_SLOT_COUNT)
            .map_err(|error| error.to_string())?;
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos();
        let shm_id = format!(
            "gmanim_preview_{}_{}_{}",
            std::process::id(),
            self.run_counter,
            timestamp
        );
        let shmem = shared_memory::ShmemConf::new()
            .size(layout.total_size)
            .os_id(&shm_id)
            .create()
            .map_err(|error| format!("failed to create preview shared memory: {error}"))?;
        let header = PreviewShmHeader::new(
            width,
            height,
            layout.capacity,
            PreviewPixelFormat::Rgba8Unorm,
        )
        .map_err(|error| error.to_string())?;
        unsafe {
            std::ptr::write(shmem.as_ptr() as *mut PreviewShmHeader, header);
        }

        self.preview_shmem = Some(shmem);
        *self.preview_readback_config.lock().unwrap() = Some(PreviewReadbackConfig {
            shm_id: shm_id.clone(),
            layout,
        });
        let frame_bytes = width as usize * height as usize * 4;
        let cache_capacity = (PREVIEW_CACHE_BUDGET_BYTES / frame_bytes.max(1))
            .clamp(PREVIEW_CACHE_MIN_FRAMES, PREVIEW_CACHE_MAX_FRAMES);
        self.preview_cache = Some(PreviewFrameCache::new(cache_capacity));
        self.preview_ready = false;
        self.displayed_frame = None;
        self.texture_handle = None;
        self.total_frames_to_render = total_frames;
        self.preview_framerate = framerate.max(1);
        self.preview_size = (width, height);
        self.current_time = 0.0;
        self.desired_frame = 0;
        self.in_flight_requests.clear();
        self.free_preview_slots = (0..PREVIEW_SLOT_COUNT).collect();
        self.prefetch_queue.clear();
        self.producer_active = false;
        self.preview_buffer_primed = false;
        self.render_failed = false;
        self.execution_result = "Preparing preview...".to_owned();

        self.ipc_tx_cmd
            .as_ref()
            .ok_or_else(|| "editor command channel is unavailable".to_owned())?
            .send(EditorCommand::OpenPreview { shm_id })
            .map_err(|_| "Python renderer stopped before opening the preview".to_owned())?;
        self.preview_ready = true;
        self.set_preview_target(0);
        Ok(())
    }

    fn accept_preview_frame(
        &mut self,
        request_id: u64,
        frame: u32,
        slot: u32,
        image: std::sync::Arc<egui::ColorImage>,
    ) -> Result<(), String> {
        self.in_flight_requests
            .get(&request_id)
            .copied()
            .filter(|request| request.frame == frame && request.slot == slot)
            .ok_or_else(|| "preview completion does not match the in-flight request".to_owned())?;
        self.preview_cache
            .as_mut()
            .ok_or_else(|| "preview cache is unavailable".to_owned())?
            .insert(frame, image);
        self.render_rate.sample();
        self.in_flight_requests.remove(&request_id);
        self.free_preview_slots.push_back(slot);
        self.execution_result = format!("Frame {frame} ready");
        Ok(())
    }

    fn set_preview_target(&mut self, frame: u32) {
        let frame = frame.min(self.total_frames_to_render.saturating_sub(1));
        if frame != self.desired_frame {
            let crossed_loop_boundary = self.is_looping
                && ((self.time_scale > 0.0 && frame < self.desired_frame)
                    || (self.time_scale < 0.0 && frame > self.desired_frame));
            if self.is_playing && !crossed_loop_boundary {
                self.skipped_timeline_frames +=
                    u64::from(frame.abs_diff(self.desired_frame).saturating_sub(1));
            }
            self.desired_frame = frame;
        }
        self.refill_preview_buffer();
        self.dispatch_preview_requests();
    }

    fn playback_window(&self, limit: usize) -> Vec<u32> {
        directional_window(
            self.desired_frame,
            self.total_frames_to_render,
            self.time_scale >= 0.0,
            self.is_looping,
            limit,
        )
    }

    fn refill_preview_buffer(&mut self) {
        let Some(cache) = self.preview_cache.as_ref() else {
            return;
        };
        let in_flight_frames: HashSet<u32> = self
            .in_flight_requests
            .values()
            .map(|request| request.frame)
            .collect();

        if !self.is_playing {
            self.producer_active = false;
            self.prefetch_queue.clear();
            for distance in 0..=PREVIEW_BUFFER_LOW_WATER {
                for candidate in [
                    self.desired_frame.checked_add(distance as u32),
                    self.desired_frame.checked_sub(distance as u32),
                ]
                .into_iter()
                .flatten()
                {
                    if candidate < self.total_frames_to_render
                        && !cache.contains(candidate)
                        && !in_flight_frames.contains(&candidate)
                        && !self.prefetch_queue.contains(&candidate)
                    {
                        self.prefetch_queue.push_back(candidate);
                    }
                }
            }
            return;
        }

        let window = self.playback_window(PREVIEW_BUFFER_HIGH_WATER);
        let buffered = window
            .iter()
            .take_while(|frame| cache.contains(**frame) || in_flight_frames.contains(*frame))
            .count();
        let effective_high_water = window.len();
        let effective_low_water = PREVIEW_BUFFER_LOW_WATER.min(effective_high_water);
        let was_active = self.producer_active;
        (self.producer_active, self.preview_buffer_primed) = buffered_producer_state(
            self.producer_active,
            self.preview_buffer_primed,
            buffered,
            effective_low_water,
            effective_high_water,
        );
        if was_active && !self.producer_active && !self.preview_buffer_primed {
            self.prefetch_queue.clear();
            return;
        }

        self.prefetch_queue = window
            .into_iter()
            .filter(|frame| {
                !cache.contains(*frame)
                    && !in_flight_frames.contains(frame)
                    && (self.producer_active || *frame == self.desired_frame)
            })
            .collect();
    }

    fn preview_buffer_status(&self) -> (usize, usize) {
        let Some(cache) = self.preview_cache.as_ref() else {
            return (0, 0);
        };
        let in_flight_frames: HashSet<u32> = self
            .in_flight_requests
            .values()
            .map(|request| request.frame)
            .collect();
        let window = self.playback_window(PREVIEW_BUFFER_HIGH_WATER);
        let buffered = window
            .iter()
            .take_while(|frame| cache.contains(**frame) || in_flight_frames.contains(*frame))
            .count();
        (buffered, window.len())
    }

    fn dispatch_preview_requests(&mut self) {
        if !self.preview_ready || self.total_frames_to_render == 0 {
            return;
        }
        while let Some(slot) = self.free_preview_slots.pop_front() {
            let Some(frame) = self.prefetch_queue.pop_front() else {
                self.free_preview_slots.push_front(slot);
                break;
            };
            let cached = self
                .preview_cache
                .as_ref()
                .is_some_and(|cache| cache.contains(frame));
            let pending = self
                .in_flight_requests
                .values()
                .any(|request| request.frame == frame);
            if cached || pending {
                self.free_preview_slots.push_front(slot);
                continue;
            }
            let request_id = self.next_request_id;
            self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
            let Some(tx) = &self.ipc_tx_cmd else {
                self.free_preview_slots.push_front(slot);
                return;
            };
            if tx
                .send(EditorCommand::RenderFrame {
                    request_id,
                    frame,
                    slot,
                })
                .is_err()
            {
                self.free_preview_slots.push_front(slot);
                return;
            }
            self.in_flight_requests.insert(
                request_id,
                PreviewRequest {
                    id: request_id,
                    frame,
                    slot,
                },
            );
        }
    }
}

impl eframe::App for GmanimEditorApp {
    fn on_exit(&mut self) {
        if let Some(mut child) = self.subprocess.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(tx) = &self.ipc_tx_cmd {
            let _ = tx.send(EditorCommand::Quit);
        }
        self.keep_running
            .store(false, std::sync::atomic::Ordering::Release);
        while let Some(handle) = self.ipc_threads.pop() {
            let _ = handle.join();
        }
    }

    fn ui(&mut self, root_ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if !self.has_project {
            root_ui.vertical_centered(|ui| {
                ui.add_space(ui.available_height() / 3.0);
                ui.heading("Welcome to GManim Editor");
                ui.label("A Python virtual environment (.venv) is required to run the editor.");
                ui.label("Please open the editor in a valid GManim project directory.");
            });
            return;
        }

        let ctx = &root_ui.ctx().clone();

        // Check file change
        if self.file_changed_rx.try_recv().is_ok() {
            while self.file_changed_rx.try_recv().is_ok() {}
            self.pending_file_reload = Some(std::time::Instant::now());
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
        if self
            .pending_file_reload
            .is_some_and(|changed_at| changed_at.elapsed() >= std::time::Duration::from_millis(250))
        {
            self.pending_file_reload = None;
            let mut new_available_files = Vec::new();
            if let Ok(entries) = std::fs::read_dir(".") {
                for entry in entries.flatten() {
                    if let Some(ext) = entry.path().extension() {
                        if ext == "py" {
                            if let Some(name) = entry.file_name().to_str() {
                                new_available_files.push(name.to_string());
                            }
                        }
                    }
                }
            }
            new_available_files.sort();
            self.available_files = new_available_files;

            if let Ok(content) = std::fs::read_to_string(&self.current_file) {
                if self.python_script != content {
                    self.python_script = content;
                }
            }
            self.run_python(ctx);
        }

        // Process IPC messages
        let mut ipc_messages = Vec::new();
        if let Some(rx) = self.ipc_rx.as_ref() {
            for _ in 0..PREVIEW_COMPLETIONS_PER_UI_UPDATE {
                let Ok(message) = rx.try_recv() else {
                    break;
                };
                ipc_messages.push(message);
            }
            if ipc_messages.len() == PREVIEW_COMPLETIONS_PER_UI_UPDATE {
                ctx.request_repaint();
            }
        }
        for message in ipc_messages {
            match message {
                ThreadMessage::ScenesInfo(scenes) => {
                    self.available_scenes = scenes;
                    if !self.available_scenes.contains(&self.selected_scene) {
                        self.selected_scene =
                            self.available_scenes.first().cloned().unwrap_or_default();
                    }
                    if let Some(tx) = &self.ipc_tx_cmd {
                        let _ = tx.send(EditorCommand::LoadScene {
                            name: self.selected_scene.clone(),
                        });
                    }
                }
                ThreadMessage::SceneReady {
                    total_frames,
                    width,
                    height,
                    framerate,
                } => {
                    if let Err(error) = self.prepare_preview(total_frames, width, height, framerate)
                    {
                        self.render_failed = true;
                        self.execution_result = format!("Error: {error}");
                    }
                }
                ThreadMessage::PreviewOpened => {
                    self.preview_ready = true;
                    self.execution_result = "Preview ready".to_owned();
                    self.set_preview_target(0);
                }
                ThreadMessage::FrameReady {
                    request_id,
                    frame,
                    slot,
                    image,
                } => {
                    if let Err(error) = self.accept_preview_frame(request_id, frame, slot, image) {
                        self.render_failed = true;
                        self.execution_result = format!("Error: {error}");
                    }
                    self.set_preview_target(self.desired_frame);
                    ctx.request_repaint();
                }
                ThreadMessage::Error(msg) => {
                    self.render_failed = true;
                    self.execution_result = format!("Error: {}", msg);
                }
            }
        }

        // Top Panel
        egui::Panel::top("top_panel").show_inside(root_ui, |ui| {
            ui.horizontal(|ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Quit").clicked() {
                        if let Some(child) = &mut self.subprocess {
                            let _ = child.kill();
                        }
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });

                ui.separator();
                ui.toggle_value(&mut self.show_editor, "📝 Editor");

                ui.separator();

                if ui.button("▶ Run Script").clicked() {
                    self.run_python(ctx);
                }

                ui.separator();
                ui.label("File:");

                let previous_file = self.current_file.clone();
                egui::ComboBox::from_id_salt("file_selector")
                    .selected_text(&self.current_file)
                    .show_ui(ui, |ui| {
                        for file in &self.available_files {
                            ui.selectable_value(&mut self.current_file, file.clone(), file);
                        }
                    });

                if self.current_file != previous_file {
                    if let Ok(content) = std::fs::read_to_string(&self.current_file) {
                        self.python_script = content;
                        self.selected_scene.clear();
                        self.run_python(ctx);
                    }
                }

                ui.separator();
                ui.label("Scene:");

                let previous_scene = self.selected_scene.clone();
                egui::ComboBox::from_id_salt("scene_selector")
                    .selected_text(&self.selected_scene)
                    .show_ui(ui, |ui| {
                        let mut scenes = self.available_scenes.clone();
                        if scenes.is_empty() {
                            scenes.push(self.selected_scene.clone());
                        }
                        for scene_name in scenes {
                            ui.selectable_value(
                                &mut self.selected_scene,
                                scene_name.clone(),
                                scene_name,
                            );
                        }
                    });

                if self.selected_scene != previous_scene && !self.selected_scene.is_empty() {
                    // Start rendering new scene
                    if let Some(tx) = &self.ipc_tx_cmd {
                        let _ = tx.send(EditorCommand::LoadScene {
                            name: self.selected_scene.clone(),
                        });
                        self.current_time = 0.0;
                    }
                }
            });
        });

        // Left Panel
        if self.show_editor {
            egui::Panel::left("left_panel")
                .resizable(true)
                .default_size(600.0)
                .show_inside(root_ui, |ui| {
                    ui.heading("Code");
                    ui.separator();

                    let editor = egui::TextEdit::multiline(&mut self.python_script)
                        .font(egui::TextStyle::Monospace)
                        .code_editor()
                        .desired_rows(30)
                        .lock_focus(true)
                        .desired_width(f32::INFINITY);

                    let mut changed = false;
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        if ui.add(editor).changed() {
                            changed = true;
                        }
                    });

                    if changed && self.has_project {
                        let _ = std::fs::write(&self.current_file, &self.python_script);
                    }

                    ui.separator();
                    ui.label(egui::RichText::new("Execution Output:").strong());
                    ui.label(&self.execution_result);
                });
        }

        // Bottom Panel
        egui::Panel::bottom("bottom_panel").show_inside(root_ui, |ui| {
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    let max_frames = self.total_frames_to_render;
                    let max_time =
                        max_frames.saturating_sub(1) as f32 / self.preview_framerate as f32;

                    if ui.button("⏮").on_hover_text("Restart").clicked() {
                        self.seek_to(0.0);
                    }
                    if ui.button("⏪").on_hover_text("Rewind 2x").clicked() {
                        self.time_scale = -2.0;
                        self.is_playing = true;
                    }
                    if ui.button("◀").on_hover_text("Play Backward").clicked() {
                        self.time_scale = -1.0;
                        self.is_playing = true;
                    }

                    let play_text = if self.is_playing && self.time_scale == 1.0 {
                        "⏸"
                    } else {
                        "▶"
                    };
                    if ui.button(play_text).on_hover_text("Play / Pause").clicked() {
                        if self.is_playing && self.time_scale == 1.0 {
                            self.is_playing = false;
                        } else {
                            if self.current_time >= max_time {
                                self.seek_to(0.0);
                            }
                            self.time_scale = 1.0;
                            self.is_playing = true;
                        }
                    }

                    if ui.button("⏩").on_hover_text("Fast Forward 2x").clicked() {
                        self.time_scale = 2.0;
                        self.is_playing = true;
                    }

                    ui.checkbox(&mut self.is_looping, "🔁 Loop");

                    ui.label("Time scale:");
                    ui.add(egui::DragValue::new(&mut self.time_scale).speed(0.1));
                });

                ui.horizontal(|ui| {
                    ui.label("Timeline:");
                    let mut new_time = self.current_time;
                    let max_frames = self.total_frames_to_render;
                    let max_time =
                        max_frames.saturating_sub(1) as f32 / self.preview_framerate as f32;

                    if ui
                        .add(egui::Slider::new(&mut new_time, 0.0..=(max_time.max(0.1))).text("s"))
                        .changed()
                    {
                        self.seek_to(new_time);
                        self.is_playing = false;
                    }
                });
            });
        });

        // Right Panel (Preview)
        egui::CentralPanel::default().show_inside(root_ui, |ui| {
            ui.heading("Preview");
            ui.separator();

            let status_height = 24.0;
            let (buffered_frames, buffer_target) = self.preview_buffer_status();
            let (cache_len, cache_capacity) = self
                .preview_cache
                .as_ref()
                .map(|cache| (cache.len(), cache.capacity()))
                .unwrap_or_default();
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), status_height),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    if let Some(request) = self.in_flight_requests.values().next() {
                        ui.spinner();
                        ui.label(format!(
                            "Rendering {} frames (next {})",
                            self.in_flight_requests.len(),
                            request.frame
                        ));
                    } else if self.preview_ready {
                        ui.label("Ready");
                    } else {
                        ui.label("Preparing preview");
                    }
                    ui.separator();
                    ui.label(format!(
                        "Scene {} | Display {:.1} | Render {:.1} FPS | Buffer {}/{} | Cache {}/{} | {} | {:.2}x | skipped {}",
                        self.preview_framerate,
                        self.display_rate.rate(),
                        self.render_rate.rate(),
                        buffered_frames,
                        buffer_target,
                        cache_len,
                        cache_capacity,
                        if self.producer_active {
                            if self.preview_buffer_primed { "maintaining" } else { "filling" }
                        } else {
                            "idle"
                        },
                        self.time_scale,
                        self.skipped_timeline_frames
                    ));
                },
            );

            let available_size = ui.available_size();
            let width = available_size.x.max(1.0) as u32;
            let height = available_size.y.max(1.0) as u32;

            let mut need_seek = None;
            if self.is_playing && self.total_frames_to_render > 0 {
                let dt = ui.input(|i| i.stable_dt);
                let delta = dt * self.time_scale;
                let (new_time, keep_playing) = advance_playback_time(
                    self.current_time,
                    delta,
                    self.total_frames_to_render,
                    self.preview_framerate,
                    self.is_looping,
                );
                self.is_playing = keep_playing;
                need_seek = Some(new_time);
            }

            if let Some(time) = need_seek {
                self.seek_to(time);
                ui.ctx().request_repaint();
            }

            let current_frame = ((self.current_time * self.preview_framerate as f32) as u32)
                .min(self.total_frames_to_render.saturating_sub(1));
            self.set_preview_target(current_frame);
            let frame_to_display = self.preview_cache.as_ref().and_then(|cache| {
                if cache.contains(current_frame) {
                    Some(current_frame)
                } else if self.is_playing {
                    cache.playback_frame(
                        current_frame,
                        self.displayed_frame,
                        self.time_scale >= 0.0,
                        self.is_looping,
                    )
                } else {
                    None
                }
            });
            if self.displayed_frame != frame_to_display
                && let Some(frame) = frame_to_display
                && let Some(image) = self
                    .preview_cache
                    .as_mut()
                    .and_then(|cache| cache.load_image(frame))
            {
                if let Some(texture) = &mut self.texture_handle {
                    texture.set(egui::ImageData::Color(image), egui::TextureOptions::LINEAR);
                } else {
                    self.texture_handle = Some(ui.ctx().load_texture(
                        "preview",
                        egui::ImageData::Color(image),
                        egui::TextureOptions::LINEAR,
                    ));
                }
                self.displayed_frame = Some(frame);
                self.display_rate.sample();
            }
            if frame_to_display.is_none() {
                ui.ctx().request_repaint();
            }

            if let Some(tex) = &self.texture_handle {
                let aspect_ratio = self.preview_size.0 as f32 / self.preview_size.1 as f32;
                let mut display_width = width as f32;
                let mut display_height = width as f32 / aspect_ratio;

                if display_height > height as f32 {
                    display_height = height as f32;
                    display_width = height as f32 * aspect_ratio;
                }

                ui.centered_and_justified(|ui| {
                    ui.image(egui::load::SizedTexture::new(
                        tex.id(),
                        [display_width, display_height],
                    ));
                });
            } else {
                ui.label("No scene loaded.");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PreviewFrameCache, advance_playback_time, buffered_producer_state, directional_window,
        producer_state,
    };

    #[test]
    fn preview_cache_evicts_the_least_recently_used_frame() {
        let mut cache = PreviewFrameCache::new(2);
        let image = std::sync::Arc::new(egui::ColorImage::from_rgba_unmultiplied(
            [2, 1],
            &[255, 0, 0, 255, 0, 255, 0, 128],
        ));
        cache.insert(1, image.clone());
        cache.insert(2, image.clone());
        cache.load_image(1).unwrap();
        cache.insert(3, image);

        assert!(cache.contains(1));
        assert!(!cache.contains(2));
        assert!(cache.contains(3));
        let image = cache.load_image(1).unwrap();
        assert_eq!(image.size, [2, 1]);
        assert_eq!(image.pixels[0].to_srgba_unmultiplied(), [255, 0, 0, 255]);
        assert_eq!(image.pixels[1].to_srgba_unmultiplied(), [0, 255, 0, 128]);
    }

    #[test]
    fn producer_uses_low_and_high_water_hysteresis() {
        assert!(!producer_state(false, 32, 32, 48));
        assert!(producer_state(false, 31, 32, 48));
        assert!(producer_state(true, 47, 32, 48));
        assert!(!producer_state(true, 48, 32, 48));
    }

    #[test]
    fn primed_buffer_is_maintained_without_waiting_for_low_water() {
        assert_eq!(
            buffered_producer_state(true, false, 48, 32, 48),
            (false, true)
        );
        assert_eq!(
            buffered_producer_state(false, true, 47, 32, 48),
            (true, true)
        );
        assert_eq!(
            buffered_producer_state(true, true, 48, 32, 48),
            (false, true)
        );
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
    fn playback_selection_never_regresses_outside_a_loop_boundary() {
        let mut cache = PreviewFrameCache::new(8);
        for frame in [84, 100, 103, 1199, 0] {
            cache.insert(
                frame,
                std::sync::Arc::new(egui::ColorImage::from_rgba_unmultiplied(
                    [1, 1],
                    &[0, 0, 0, 255],
                )),
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
