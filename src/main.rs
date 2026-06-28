use eframe::egui;
use pyo3::prelude::*;

fn main() -> eframe::Result<()> {
    // 1. Parse command line arguments
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
        println!("Found pyproject.toml, ensuring dependencies are installed via uv...");
        let _ = std::process::Command::new("uv").arg("sync").status();
    }

    // 2. Initialize embedded Python and inject our native extension!
    use gmanim::gmanim;
    pyo3::append_to_inittab!(gmanim);

    // 2. Setup standard eframe options
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
    current_timeline: Option<gmanim_core::animation::Timeline>,
    rendered_frames: Vec<std::sync::Arc<egui::ColorImage>>,
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
    renderer: gmanim_core::vulkan::renderer::VulkanRenderer,
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
            current_timeline: None,
            texture_handle: None,
            rendered_frames: Vec::new(),
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
            renderer: gmanim_core::vulkan::renderer::VulkanRenderer::new(
                std::sync::Arc::new(
                    pollster::block_on(gmanim_core::vulkan::context::VulkanContext::new()).unwrap(),
                ),
                gmanim_core::RendererConfig {
                    msaa_samples: 4,
                    ssaa_factor: 1,
                },
            ),
        };

        if app.has_project {
            app.run_python();
        }

        app
    }

    fn seek_to(&mut self, target_time: f32) {
        self.current_time = target_time;
    }

    fn run_python(&mut self) {
        let selected_scene = self.selected_scene.clone();

        let result = pyo3::Python::attach(
            |py| -> pyo3::PyResult<(Vec<String>, Option<gmanim_core::animation::Timeline>)> {
                let locals = pyo3::types::PyDict::new(py);

                // Inject virtual environment path so `uv` installed packages are accessible
                let setup_script = r#"
import sys
import os
import glob
import gmanim

# Clear registry to avoid ghost scenes
if hasattr(gmanim, 'registry'):
    gmanim.registry.clear()

cwd = os.getcwd()
if cwd not in sys.path:
    sys.path.insert(0, cwd)

if os.path.exists(".venv"):
    site_packages = glob.glob(".venv/lib/python*/site-packages")
    if site_packages:
        venv_path = os.path.abspath(site_packages[0])
        if venv_path not in sys.path:
            sys.path.insert(0, venv_path)

# Unload local user modules to force reload on subsequent runs
for mod_name, mod in list(sys.modules.items()):
    if hasattr(mod, '__file__') and mod.__file__ and mod.__file__.startswith(cwd):
        del sys.modules[mod_name]
"#;
                py.run(
                    &std::ffi::CString::new(setup_script).unwrap(),
                    Some(&locals),
                    Some(&locals),
                )?;

                py.run(
                    &std::ffi::CString::new(&self.python_script[..]).unwrap(),
                    Some(&locals),
                    Some(&locals),
                )?;

                let mut available_scenes = Vec::new();
                let mut timeline_out = None;

                // Extract available scenes
                let gmanim = py.import("gmanim")?;
                if let Ok(registry) = gmanim.getattr("registry") {
                    if let Ok(dict) = registry.cast::<pyo3::types::PyDict>() {
                        for key in dict.keys() {
                            if let Ok(key_str) = key.extract::<String>() {
                                available_scenes.push(key_str);
                            }
                        }

                        if dict.len() > 0 {
                            // Find the scene to execute
                            let mut target_func = None;

                            if !selected_scene.is_empty() {
                                if let Ok(Some(func)) = dict.get_item(&selected_scene) {
                                    target_func = Some(func);
                                }
                            }

                            if target_func.is_none() {
                                // Fallback to first scene
                                target_func = Some(dict.iter().next().unwrap().1);
                            }

                            if let Some(func) = target_func {
                                let scene_class = gmanim.getattr("Scene")?;
                                let scene_obj = scene_class.call0()?;
                                func.call1((&scene_obj,))?;

                                if let Ok(mut py_scene) =
                                    scene_obj
                                        .extract::<pyo3::PyRefMut<'_, gmanim::scene::PyScene>>()
                                {
                                    if let Some(timeline) = (&mut *py_scene).inner.take() {
                                        timeline_out = Some(timeline);
                                    } else {
                                        println!("PyScene inner timeline was None!");
                                    }
                                } else {
                                    println!("Failed to extract PyRefMut<PyScene>");
                                }
                            }
                        }
                    }
                }

                if timeline_out.is_none() {
                    // Fallback: Look for `scene` in globals.
                    if let Some(scene_obj) = locals.get_item("scene")? {
                        if let Ok(mut py_scene) =
                            scene_obj.extract::<pyo3::PyRefMut<'_, gmanim::scene::PyScene>>()
                        {
                            if let Some(timeline) = (&mut *py_scene).inner.take() {
                                timeline_out = Some(timeline);
                            }
                        }
                    }
                }

                Ok((available_scenes, timeline_out))
            },
        );

        match result {
            Ok((scenes, Some(timeline))) => {
                self.available_scenes = scenes;
                if !self.available_scenes.contains(&self.selected_scene) {
                    self.selected_scene =
                        self.available_scenes.first().cloned().unwrap_or_default();
                }

                self.total_frames_to_render = timeline.total_frames();
                self.current_timeline = Some(timeline);
                self.texture_handle = None; // clear texture to force re-render
                self.current_time = 0.0;
                self.rendered_frames.clear();
                self.is_rendering = true;
                self.execution_result = "Execution successful".to_owned();
            }
            Ok((scenes, None)) => {
                self.available_scenes = scenes;
                if !self.available_scenes.contains(&self.selected_scene) {
                    self.selected_scene =
                        self.available_scenes.first().cloned().unwrap_or_default();
                }

                self.current_timeline = None;
                self.texture_handle = None;
                self.rendered_frames.clear();
                self.is_rendering = false;
                self.total_frames_to_render = 0;
                self.execution_result = "Execution completed, but no Scene object found".to_owned();
            }
            Err(e) => {
                self.execution_result =
                    pyo3::Python::attach(|py| format!("Error: {}", e.value(py)));
                self.current_timeline = None;
                self.texture_handle = None;
                self.rendered_frames.clear();
                self.is_rendering = false;
                self.total_frames_to_render = 0;
            }
        }
    }
}

impl eframe::App for GmanimEditorApp {
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

        if let Ok(_) = self.file_changed_rx.try_recv() {
            // Drain channel
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
            self.run_python();
        }

        // Top Panel
        egui::Panel::top("top_panel").show_inside(root_ui, |ui| {
            ui.horizontal(|ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Quit").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });

                ui.separator();
                ui.toggle_value(&mut self.show_editor, "📝 Editor");

                ui.separator();

                if ui.button("▶ Run Script").clicked() {
                    self.run_python();
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
                        self.run_python();
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
                    self.run_python();
                }
            });
        });

        // Left Panel - Script Editor
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

        // Bottom Panel - Timeline Scrubbing
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

        // Right Panel - Preview (must be added last as CentralPanel)
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
                    (self.rendered_frames.len().saturating_sub(1) as f32).max(0.0) / 60.0
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
                if let Some(timeline) = &mut self.current_timeline {
                    // Render frames incrementally to avoid freezing UI
                    for _ in 0..2 {
                        if timeline.step_frame() {
                            let w = timeline.ctx.scene_config.output_width as usize;
                            let h = timeline.ctx.scene_config.output_height as usize;
                            self.renderer.render_scene_with_outputs(
                                &timeline.scene,
                                &timeline.ctx.scene_config,
                                None,
                                gmanim_core::vulkan::renderer::RenderOutputs {
                                    cpu_nv12: false,
                                    vulkan_video: false,
                                    cpu_rgba: true,
                                    cpu_yuv444p: false,
                                },
                            );
                            let raw_bytes = self.renderer.get_rgba_bytes();
                            let image = if let Some(bytes) = raw_bytes {
                                if bytes.len() == 0 {
                                    egui::ColorImage::from_rgba_unmultiplied(
                                        [w, h],
                                        &vec![0u8; w * h * 4],
                                    )
                                } else {
                                    egui::ColorImage::from_rgba_unmultiplied([w, h], bytes)
                                }
                            } else {
                                egui::ColorImage::from_rgba_unmultiplied(
                                    [w, h],
                                    &vec![0u8; w * h * 4],
                                )
                            };
                            self.rendered_frames.push(std::sync::Arc::new(image));
                        } else {
                            self.is_rendering = false;
                            break;
                        }
                    }
                    ui.ctx().request_repaint();
                } else {
                    self.is_rendering = false;
                }
            }

            if self.is_rendering {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!(
                        "Rendering: {} / {}",
                        self.rendered_frames.len(),
                        self.total_frames_to_render
                    ));
                });
            }

            let current_frame_idx = (self.current_time * 60.0) as usize;
            let image_to_show = if let Some(img) = self.rendered_frames.get(current_frame_idx) {
                Some(img.clone())
            } else {
                self.rendered_frames.last().cloned()
            };

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
