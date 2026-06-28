use eframe::egui;
use interprocess::TryClone;
use interprocess::local_socket::{
    GenericFilePath, GenericNamespaced, ListenerOptions, prelude::*, traits::Listener,
};
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::Ordering;

pub mod ipc;
use ipc::{EditorCommand, EditorEvent, ShmHeader, ThreadMessage};

struct SendShmem(shared_memory::Shmem);
unsafe impl Send for SendShmem {}
unsafe impl Sync for SendShmem {}
impl SendShmem {
    fn as_ptr(&self) -> *mut u8 {
        self.0.as_ptr()
    }
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

    rendered_frames: Vec<Option<std::sync::Arc<egui::ColorImage>>>,
    rendered_count: u32,
    is_rendering: bool,
    total_frames_to_render: u32,
    texture_handle: Option<egui::TextureHandle>,
    has_project: bool,
    is_playing: bool,
    current_time: f32,
    available_scenes: Vec<String>,
    selected_scene: String,
    _watcher: Option<notify::RecommendedWatcher>,
    file_changed_rx: std::sync::mpsc::Receiver<()>,
    playback_speed: f32,
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

            texture_handle: None,
            rendered_frames: Vec::new(),
            rendered_count: 0,
            is_rendering: false,
            total_frames_to_render: 0,
            has_project,
            is_playing: true,
            current_time: 0.0,
            available_scenes: Vec::new(),
            selected_scene: String::new(),
            _watcher: watcher,
            file_changed_rx: rx,
            playback_speed: 1.0,
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
        let shm_id = format!("gmanim_shm_{}_{}", std::process::id(), timestamp);

        let ctrl_socket_name = if cfg!(windows) {
            format!(r"\\.\pipe\gmanim_ctrl_{}_{}", std::process::id(), timestamp)
        } else {
            format!("/tmp/gmanim_ctrl_{}_{}", std::process::id(), timestamp)
        };

        // Create Shared Memory with 16-frame Ring Buffer
        let created_shm = match shared_memory::ShmemConf::new()
            .size(1920 * 1080 * 4 * 16 + std::mem::size_of::<ShmHeader>())
            .os_id(&shm_id)
            .create()
        {
            Ok(s) => s,
            Err(e) => {
                self.execution_result = format!("Failed to create SHM: {}", e);
                return;
            }
        };

        let header = unsafe { &mut *(created_shm.as_ptr() as *mut ShmHeader) };
        header.is_rendering.store(false, Ordering::Release);
        header.write_idx.store(0, Ordering::Release);
        header.read_idx.store(0, Ordering::Release);

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
            .arg("--shm-id")
            .arg(&shm_id)
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
        self.current_time = 0.0;
        self.rendered_frames.clear();
        self.rendered_count = 0;
        self.is_rendering = false;
        self.total_frames_to_render = 0;

        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<EditorCommand>();
        self.ipc_tx_cmd = Some(cmd_tx);

        let ctx_clone = ctx.clone();

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
                while keep_running_read.load(Ordering::Acquire) {
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            if let Ok(event) = serde_json::from_str::<EditorEvent>(&line) {
                                match event {
                                    EditorEvent::ScenesInfo { scenes } => {
                                        let _ = tx_clone.send(ThreadMessage::ScenesInfo(scenes));
                                    }
                                    EditorEvent::StartRender {
                                        total_frames,
                                        width: _,
                                        height: _,
                                    } => {
                                        let _ = tx_clone.send(ThreadMessage::StartRender {
                                            total_frames: total_frames as u32,
                                        });
                                    }
                                    EditorEvent::FinishRender => {
                                        let _ = tx_clone.send(ThreadMessage::FinishRender);
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

        // Background thread to poll SHM during rendering
        let tx_shm = tx.clone();
        let ctx_shm = ctx.clone();
        let send_shmem = SendShmem(created_shm);
        let keep_running_shm = self.keep_running.clone();
        let handle3 = std::thread::spawn(move || {
            loop {
                if !keep_running_shm.load(Ordering::Acquire) {
                    break;
                }

                let header = unsafe { &*(send_shmem.as_ptr() as *const ShmHeader) };
                let is_rendering = header.is_rendering.load(Ordering::Acquire);
                let write_idx = header.write_idx.load(Ordering::Acquire);
                let mut read_idx = header.read_idx.load(Ordering::Acquire);

                if write_idx > read_idx {
                    let width = header.width as usize;
                    let height = header.height as usize;
                    let size = width * height * 4;
                    let base_pixels_ptr =
                        unsafe { send_shmem.as_ptr().add(std::mem::size_of::<ShmHeader>()) };

                    while read_idx < write_idx {
                        let buf_idx = (read_idx % 16) as usize;
                        let pixels_ptr = unsafe { base_pixels_ptr.add(buf_idx * size) };

                        let mut buf = vec![0u8; size];
                        unsafe {
                            std::ptr::copy_nonoverlapping(pixels_ptr, buf.as_mut_ptr(), size);
                        }

                        let image = egui::ColorImage::from_rgba_unmultiplied([width, height], &buf);
                        let _ = tx_shm.send(ThreadMessage::FrameReady(
                            read_idx,
                            std::sync::Arc::new(image),
                        ));

                        read_idx += 1;
                        header.read_idx.store(read_idx, Ordering::Release);
                    }
                    ctx_shm.request_repaint();
                } else if !is_rendering {
                    std::thread::sleep(std::time::Duration::from_millis(16));
                } else {
                    std::thread::yield_now();
                }
            }
        });

        self.ipc_threads.push(handle1);
        self.ipc_threads.push(handle3);
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
        if let Ok(_) = self.file_changed_rx.try_recv() {
            while let Ok(_) = self.file_changed_rx.try_recv() {}
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
        if let Some(rx) = &self.ipc_rx {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    ThreadMessage::ScenesInfo(scenes) => {
                        self.available_scenes = scenes;
                        if !self.available_scenes.contains(&self.selected_scene) {
                            self.selected_scene =
                                self.available_scenes.first().cloned().unwrap_or_default();
                        }
                        if let Some(tx) = &self.ipc_tx_cmd {
                            let _ = tx.send(EditorCommand::RenderScene {
                                name: self.selected_scene.clone(),
                            });
                        }
                    }
                    ThreadMessage::StartRender { total_frames } => {
                        self.total_frames_to_render = total_frames;
                        self.is_rendering = true;
                        self.rendered_frames = vec![None; total_frames as usize];
                        self.rendered_count = 0;
                        self.execution_result = "Rendering...".to_owned();
                    }
                    ThreadMessage::FrameReady(idx, img) => {
                        if (idx as usize) < self.rendered_frames.len() {
                            if self.rendered_frames[idx as usize].is_none() {
                                self.rendered_count += 1;
                            }
                            self.rendered_frames[idx as usize] = Some(img);
                        }
                    }
                    ThreadMessage::FinishRender => {
                        self.is_rendering = false;
                        self.execution_result = "Execution successful".to_owned();
                    }
                    ThreadMessage::Error(msg) => {
                        self.is_rendering = false;
                        self.execution_result = format!("Error: {}", msg);
                    }
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
                        let _ = tx.send(EditorCommand::RenderScene {
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
                    let max_time = max_frames as f32 / 60.0;

                    if ui.button("⏮").on_hover_text("Restart").clicked() {
                        self.seek_to(0.0);
                    }
                    if ui.button("⏪").on_hover_text("Rewind 2x").clicked() {
                        self.playback_speed = -2.0;
                        self.is_playing = true;
                    }
                    if ui.button("◀").on_hover_text("Play Backward").clicked() {
                        self.playback_speed = -1.0;
                        self.is_playing = true;
                    }

                    let play_text = if self.is_playing && self.playback_speed == 1.0 {
                        "⏸"
                    } else {
                        "▶"
                    };
                    if ui.button(play_text).on_hover_text("Play / Pause").clicked() {
                        if self.is_playing && self.playback_speed == 1.0 {
                            self.is_playing = false;
                        } else {
                            if self.current_time >= max_time {
                                self.seek_to(0.0);
                            }
                            self.playback_speed = 1.0;
                            self.is_playing = true;
                        }
                    }

                    if ui.button("⏩").on_hover_text("Fast Forward 2x").clicked() {
                        self.playback_speed = 2.0;
                        self.is_playing = true;
                    }

                    ui.checkbox(&mut self.is_looping, "🔁 Loop");

                    ui.label("Speed:");
                    ui.add(egui::DragValue::new(&mut self.playback_speed).speed(0.1));
                });

                ui.horizontal(|ui| {
                    ui.label("Timeline:");
                    let mut new_time = self.current_time;
                    let max_frames = self.total_frames_to_render;
                    let max_time = max_frames as f32 / 60.0;

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

            let available_size = ui.available_size();
            let width = available_size.x.max(1.0) as u32;
            let height = available_size.y.max(1.0) as u32;

            let mut need_seek = None;
            if self.is_playing {
                let max_time = self.total_frames_to_render as f32 / 60.0;
                let rendered_max_time = if self.is_rendering {
                    (self.rendered_count.saturating_sub(1) as f32).max(0.0) / 60.0
                } else {
                    max_time
                };

                let dt = ui.input(|i| i.stable_dt);
                let delta = dt * self.playback_speed;
                let mut new_time = self.current_time + delta;

                if self.playback_speed > 0.0 && new_time >= rendered_max_time {
                    if self.is_rendering {
                        new_time = rendered_max_time;
                    } else if new_time >= max_time {
                        if self.is_looping {
                            new_time = 0.0;
                        } else {
                            new_time = max_time;
                            self.is_playing = false;
                        }
                    }
                } else if self.playback_speed < 0.0 && new_time < 0.0 {
                    if self.is_looping {
                        new_time = rendered_max_time;
                    } else {
                        new_time = 0.0;
                        self.is_playing = false;
                    }
                }
                need_seek = Some(new_time);
            }

            if let Some(time) = need_seek {
                self.seek_to(time);
                ui.ctx().request_repaint();
            }

            if self.is_rendering {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!(
                        "Rendering: {} / {}",
                        self.rendered_count, self.total_frames_to_render
                    ));
                });
            }

            let current_frame_idx = (self.current_time * 60.0) as usize;
            let mut image_to_show = None;
            if !self.rendered_frames.is_empty() {
                let max_idx = current_frame_idx.min(self.rendered_frames.len().saturating_sub(1));
                for i in (0..=max_idx).rev() {
                    if let Some(Some(img)) = self.rendered_frames.get(i) {
                        image_to_show = Some(img.clone());
                        break;
                    }
                }
                if image_to_show.is_none() {
                    for i in (current_frame_idx + 1)..self.rendered_frames.len() {
                        if let Some(Some(img)) = self.rendered_frames.get(i) {
                            image_to_show = Some(img.clone());
                            break;
                        }
                    }
                }
            }

            if let Some(image) = image_to_show {
                if let Some(tex) = &mut self.texture_handle {
                    tex.set(image, egui::TextureOptions::LINEAR);
                } else {
                    let texture =
                        ui.ctx()
                            .load_texture("preview", image, egui::TextureOptions::LINEAR);
                    self.texture_handle = Some(texture);
                }
            }

            if let Some(tex) = &self.texture_handle {
                let aspect_ratio = 16.0 / 9.0;
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
