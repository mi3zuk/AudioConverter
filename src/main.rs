#![windows_subsystem = "windows"]

use eframe::{egui, App, CreationContext, Frame, NativeOptions};
use eframe::egui::Widget;
use rfd::FileDialog;
use std::{
    env, fs::File, io::Write,
    path::PathBuf,
    process::Command,
    sync::mpsc::{channel, Receiver, Sender},
    thread,
};
use unicode_normalization::UnicodeNormalization;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

// resource
const FFMPEG_BIN: &[u8] = include_bytes!("../ffmpeg.exe");
const FONT_DATA: &[u8]  = include_bytes!("../NotoSansJP-Regular.ttf");

struct FileEntry {
    path:   PathBuf,
    format: String,
}

enum WorkerEvent {
    Log(String),
    Done { success: usize, fail: usize },
    Progress { index: usize },
}

struct AudioApp {
    entries:     Vec<FileEntry>,
    output:      PathBuf,
    bitrate:     String,
    log:         String,
    show_log:    bool,
    is_running:  bool,
    ffmpeg_path: PathBuf,
    tx:          Sender<WorkerEvent>,
    rx:          Receiver<WorkerEvent>,
    delete_input: bool,
    delete_output_on_fail: bool,
    global_format: String,
    pending_overwrites: Vec<(PathBuf, PathBuf, String)>, // (入力, 出力, format)
    overwrite_decisions: Vec<(PathBuf, PathBuf, String)>, // (入力, 出力, format)
    overwrite_dialog: Option<(PathBuf, PathBuf, String, String)>, // (入力, 出力, format, 新しいファイル名)
    last_converted: Vec<PathBuf>, // 直近で変換したファイルのパス
    current_processing_index: Option<usize>,
}

impl AudioApp {
    fn init_ffmpeg() -> PathBuf {
        let mut exe = env::current_exe().unwrap();
        exe.set_file_name("ffmpeg.exe");
        if !exe.exists() {
            let mut f = File::create(&exe).unwrap();
            f.write_all(FFMPEG_BIN).unwrap();
        }
        exe
    }

    fn new(cc: &CreationContext) -> Self {
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert("NotoJP".into(), egui::FontData::from_static(FONT_DATA));
        for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts.families.get_mut(&fam).unwrap().insert(0, "NotoJP".into());
        }
        cc.egui_ctx.set_fonts(fonts);

        let (tx, rx) = channel::<WorkerEvent>();

        AudioApp {
            entries: Vec::new(),
            output: env::current_dir().unwrap(),
            bitrate: "192k".into(),
            log: String::new(),
            show_log: false,
            is_running: false,
            ffmpeg_path: Self::init_ffmpeg(),
            tx, rx,
            delete_input: false,
            delete_output_on_fail: true,
            global_format: "mp3".into(),
            pending_overwrites: Vec::new(),
            overwrite_decisions: Vec::new(),
            overwrite_dialog: None,
            last_converted: Vec::new(),
            current_processing_index: None,
        }
    }

    fn start_conversion(&mut self) {
        if self.entries.is_empty() || self.is_running {
            return;
        }
        self.is_running = false;
        self.log.clear();
        self.pending_overwrites.clear();
        self.overwrite_decisions.clear();

        let out = self.output.clone();
        for entry in &self.entries {
            let stem = entry.path.file_stem().unwrap().to_string_lossy().to_string();
            let outp = out.join(format!("{}.{}", stem, entry.format));
            if outp.exists() {
                self.pending_overwrites.push((entry.path.clone(), outp, entry.format.clone()));
            } else {
                self.overwrite_decisions.push((entry.path.clone(), outp, entry.format.clone()));
            }
        }

        if self.pending_overwrites.is_empty() {
            self.launch_conversion();
        }
    }

    fn launch_conversion(&mut self) {
        self.is_running = true;
        let entries: Vec<_> = std::mem::take(&mut self.overwrite_decisions)
            .into_iter()
            .map(|(path, _outp, format)| FileEntry { path, format })
            .collect();
        let out = self.output.clone();
        let br = self.bitrate.clone();
        let ff = self.ffmpeg_path.clone();
        let tx = self.tx.clone();
        let del_input = self.delete_input;
        let del_out_fail = self.delete_output_on_fail;

        // 変換対象ファイルを記録
        self.last_converted = entries.iter().map(|e| e.path.clone()).collect();
        self.current_processing_index = None;

        thread::spawn(move || {
            let mut ok = 0;
            let mut ng = 0;

            for (i, FileEntry { path, format }) in entries.into_iter().enumerate() {
                tx.send(WorkerEvent::Progress { index: i }).ok();
                let s = path.to_string_lossy().nfc().collect::<String>();
                #[cfg(windows)]
                let inp = if !s.starts_with(r"\\?\") && s.chars().nth(1) == Some(':') {
                    format!(r"\\?\{}", s)
                } else {
                    s.clone()
                };
                #[cfg(not(windows))]
                let inp = s.clone();

                let stem = PathBuf::from(&inp).file_stem().unwrap().to_string_lossy().to_string();
                let outp = out.join(format!("{}.{}", stem, format));

                let mut cmd = Command::new(&ff);
                cmd.args(&["-y", "-i", &inp, "-b:a", &br, outp.to_str().unwrap()]);
                #[cfg(windows)] { cmd.creation_flags(0x0800_0000); }

                match cmd.status() {
                    Ok(s) if s.success() => {
                        tx.send(WorkerEvent::Log(format!("OK: {} → {:?}\n", inp, outp))).ok();
                        if del_input { let _ = std::fs::remove_file(&path); }
                        ok += 1;
                    }
                    Ok(s) => {
                        tx.send(WorkerEvent::Log(format!("ERR: {} exit={:?}\n", inp, s.code()))).ok();
                        if del_out_fail { let _ = std::fs::remove_file(&outp); }
                        ng += 1;
                    }
                    Err(e) => {
                        tx.send(WorkerEvent::Log(format!("ERR: {} {}\n", inp, e))).ok();
                        if del_out_fail { let _ = std::fs::remove_file(&outp); }
                        ng += 1;
                    }
                }
            }

            tx.send(WorkerEvent::Done { success: ok, fail: ng }).ok();
        });
    }

    fn drain(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                WorkerEvent::Log(line) => self.log.push_str(&line),
                WorkerEvent::Done { success, fail } => {
                    self.log.push_str(&format!("=== 完了: 成功={} 失敗={} ===\n", success, fail));
                    self.is_running = false;
                    // 完了したファイルをリストから削除
                    self.entries.retain(|e| !self.last_converted.contains(&e.path));
                    self.last_converted.clear();
                    self.current_processing_index = None;
                }
                WorkerEvent::Progress { index } => {
                    self.current_processing_index = Some(index);
                }
            }
        }
    }
}

impl App for AudioApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut Frame) {
        use egui::{ComboBox, ScrollArea};
        self.drain();

        if ctx.input(|i| i.key_pressed(egui::Key::L) && i.modifiers.alt) {
            self.show_log = !self.show_log;
        }

        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.set_enabled(!self.is_running);
                if ui.button("Add Files").clicked() {
                    if let Some(files) = FileDialog::new().add_filter("Audio", &["mp3", "wav", "flac", "opus", "m4a"]).pick_files() {
                        for p in files {
                            self.entries.push(FileEntry { path: p, format: self.global_format.clone() });
                        }
                    }
                }
                if ui.button("Output Directory").clicked() {
                    if let Some(d) = FileDialog::new().pick_folder() {
                        self.output = d;
                    }
                }
                if self.is_running {
                    ui.label("...Processing...");
                    egui::Spinner::new().ui(ui);
                } else if ui.button("Convert").clicked() {
                    self.start_conversion();
                }
                ui.label("Bitrate:");
                ui.text_edit_singleline(&mut self.bitrate);
            });
            // 出力先ディレクトリの表示
            ui.horizontal(|ui| {
                ui.label("出力先:");
                ui.monospace(self.output.display().to_string());
            });
            ui.horizontal(|ui| {
                ui.label("Format:");
                ComboBox::from_id_source("global_format")
                    .selected_text(self.global_format.clone())
                    .show_ui(ui, |ui| {
                        for ext in ["mp3", "wav", "flac", "m4a", "opus"] {
                            if ui.selectable_value(&mut self.global_format, ext.to_string(), ext).clicked() {
                                for entry in &mut self.entries {
                                    entry.format = self.global_format.clone();
                                }
                            }
                        }
                    });
                if ui.button("Clear All").clicked() {
                    self.entries.clear();
                }
                ui.checkbox(&mut self.delete_input, "Delete Original");
                ui.checkbox(&mut self.delete_output_on_fail, "Delete failed file");
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.label("Entries:");
            ui.separator();
            ScrollArea::vertical().show(ui, |ui| {
                for (idx, entry) in self.entries.iter_mut().enumerate() {
                    ui.group(|ui| {
                        let width = ui.available_width();
                        ui.allocate_ui_with_layout(
                            egui::vec2(width, 0.0),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                let file_name = entry.path.file_name().unwrap().to_string_lossy();
                                let max_chars = 30;
                                let display_name = if file_name.chars().count() > max_chars {
                                    format!("{}...", &file_name.chars().take(max_chars - 3).collect::<String>())
                                } else {
                                    file_name.to_string()
                                };
                                let ext = entry.path.extension().and_then(|e| e.to_str()).unwrap_or("?");
                                // ローディングアニメーション表示
                                if self.is_running && Some(idx) == self.current_processing_index {
                                    egui::Spinner::new().ui(ui);
                                }
                                ui.label(format!("{} (拡張子: {})", display_name, ext));
                                ComboBox::from_id_source(entry.path.to_string_lossy())
                                    .selected_text(entry.format.clone())
                                    .show_ui(ui, |ui| {
                                        for ext in ["mp3", "wav", "flac", "m4a", "opus"] {
                                            ui.selectable_value(&mut entry.format, ext.to_string(), ext);
                                        }
                                    });
                            }
                        );
                    });
                }
            });

            if self.show_log {
                ui.separator();
                ui.label("Log:");
                ScrollArea::vertical().max_height(200.0).show(ui, |ui| {
                    ui.monospace(&self.log);
                });
            }

            // 上書き確認ダイアログ
            if self.overwrite_dialog.is_none() && !self.pending_overwrites.is_empty() {
                // 1件ずつ処理
                let (input, output, format) = self.pending_overwrites[0].clone();
                let file_name = output.file_name().unwrap().to_string_lossy().to_string();
                self.overwrite_dialog = Some((input, output, format, file_name));
            }
            if let Some((input, output, format, new_name)) = self.overwrite_dialog.take() {
                egui::Window::new("警告").collapsible(false).show(ctx, |ui| {
                    ui.label(format!("出力先に既にファイルが存在します: {}", output.display()));
                    if ui.button("上書き").clicked() {
                        self.overwrite_decisions.push((input.clone(), output.clone(), format.clone()));
                        self.pending_overwrites.remove(0);
                        self.overwrite_dialog = None;
                    }
                    ui.horizontal(|ui| {
                        ui.label("新しいファイル名:");
                        let mut name_buf = new_name.clone();
                        if ui.text_edit_singleline(&mut name_buf).lost_focus() && !name_buf.is_empty() {
                            let mut new_path = output.clone();
                            new_path.set_file_name(&name_buf);
                            self.overwrite_decisions.push((input.clone(), new_path, format.clone()));
                            self.pending_overwrites.remove(0);
                            self.overwrite_dialog = None;
                        }
                        if ui.button("この名前で保存").clicked() && !name_buf.is_empty() {
                            let mut new_path = output.clone();
                            new_path.set_file_name(&name_buf);
                            self.overwrite_decisions.push((input.clone(), new_path, format.clone()));
                            self.pending_overwrites.remove(0);
                            self.overwrite_dialog = None;
                        }
                    });
                    ui.separator();
                    if ui.button("破棄").clicked() {
                        self.pending_overwrites.remove(0);
                        self.overwrite_dialog = None;
                    }
                });
            }
            // 全部決まったら変換開始
            if self.pending_overwrites.is_empty() && !self.is_running && !self.overwrite_decisions.is_empty() {
                self.launch_conversion();
            }
        });
    }
}

fn main() {
    let mut opts = NativeOptions::default();
    opts.viewport.min_inner_size = Some(egui::Vec2::new(600.0, 400.0));

    let _ = eframe::run_native(
        "AudioConverter",
        opts,
        Box::new(|cc| Box::new(AudioApp::new(cc))),
    );
}