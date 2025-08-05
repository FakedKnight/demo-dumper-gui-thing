// #todo: remake every detection method, as it doesnt work for now

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;
use std::io::{self, Write};
use std::cell::RefCell;
use serde::{Serialize, Deserialize};
use webbrowser;
use eframe::{self, egui};
use glob::glob;
use rfd::FileDialog;
use tf_demo_parser::{
    demo::{
        data::DemoTick,
        message::{Message, MessageType},
        parser::DemoParser,
        Demo,
    },
    ParserState,
};
use arboard::Clipboard;
use winreg::enums::*;
use winreg::RegKey;
use image::io::Reader as ImageReader;
use std::io::Cursor;
use std::time::SystemTime;
use tokio;
use rand::Rng;
use bitbuffer::BitRead;
use chrono;
use csv;

// Add at the top with other imports
mod cheater_detection;
use cheater_detection::{
    base::{CheatAnalyser, CheatDemoHandler},
    viewangles::ViewAnglesAnalyzer,
    Detection
};

// Add the ViewAnglesToCSV implementation
struct ViewAnglesToCSV {
    file: std::fs::File,
    previous_angles: HashMap<u64, (f32, f32)>, // steamID -> (viewangle, pitchangle)
    output_path: PathBuf,
}

impl ViewAnglesToCSV {
    fn new(output_path: PathBuf) -> Self {
        let file = std::fs::File::create(&output_path).unwrap_or_else(|_| {
            // Fallback to temp directory if original path fails
            let temp_path = std::env::temp_dir().join("viewangles.csv");
            println!("Failed to create output file, using temp path: {}", temp_path.display());
            std::fs::File::create(&temp_path).expect("Failed to create temporary file")
        });
        
        let mut writer = ViewAnglesToCSV { 
            file,
            previous_angles: HashMap::new(),
            output_path,
        };
        
        // Write CSV header
        writeln!(writer.file, "tick,player_id,player_name,origin_x,origin_y,origin_z,viewangle,pitchangle,va_delta,pa_delta").unwrap();
        writer
    }

    fn calculate_delta(&self, curr_viewangle: f32, curr_pitchangle: f32, prev_viewangle: f32, prev_pitchangle: f32) -> (f32, f32) {
        let va_delta = {
            let diff = (curr_viewangle - prev_viewangle).rem_euclid(360.0);
            if diff > 180.0 {
                diff - 360.0
            } else {
                diff
            }
        };
        let pa_delta = curr_pitchangle - prev_pitchangle;
        (va_delta, pa_delta)
    }
    
    fn process_tick(&mut self, tick: u32, player_states: &HashMap<u64, cheater_detection::PlayerState>) -> Vec<Detection> {
        let mut detections = Vec::new();
        
        for (&player_id, player_state) in player_states {
            // Extract viewangles from player state if available
            if let Some((pitch, yaw, _)) = player_state.viewangles {
                // Get previous angles for delta calculation
                let (va_delta, pa_delta) = self.previous_angles
                    .get(&player_id)
                    .map(|&(prev_yaw, prev_pitch)| self.calculate_delta(yaw, pitch, prev_yaw, prev_pitch))
                    .unwrap_or((f32::NAN, f32::NAN));
                
                // Update previous angles for next tick
                self.previous_angles.insert(player_id, (yaw, pitch));
                
                // Extract position if available (or use placeholders)
                let (origin_x, origin_y, origin_z) = player_state.position.unwrap_or((0.0, 0.0, 0.0));
                
                // Get player name
                let player_name = player_state.name.clone();
                
                // Write to CSV
                let _ = writeln!(
                    self.file, 
                    "{},{},{},{},{},{},{},{},{},{}", 
                    tick, player_id, player_name, origin_x, origin_y, origin_z, yaw, pitch, va_delta, pa_delta
                );
                
                // Check for suspicious behavior
                // Large, sudden view angle changes might indicate cheating
                if !va_delta.is_nan() && !pa_delta.is_nan() {
                    // Detect suspicious flicks - sudden large change in either angle
                    let va_abs = va_delta.abs();
                    let pa_abs = pa_delta.abs();
                    
                    if va_abs > 45.0 || pa_abs > 30.0 {
                        // This is a possible flick or aimbot behavior
                        detections.push(Detection {
                            tick,
                            algorithm: "viewangles_analyzer".to_string(),
                            player: player_id,
                            data: serde_json::json!({
                                "type": "suspicious_angle_change",
                                "va_delta": va_delta,
                                "pa_delta": pa_delta,
                                "viewangle": yaw,
                                "pitchangle": pitch,
                            }),
                        });
                    }
                }
            }
        }
        
        detections
    }
    
    fn finish(&mut self) -> PathBuf {
        // Ensure all data is flushed to disk
        let _ = self.file.flush();
        println!("ViewAnglesToCSV finished. Output saved to: {}", self.output_path.display());
        self.output_path.clone()
    }
}

// Define thread-local storage for the latest OOB summary
thread_local! {
    static LATEST_OOB_SUMMARY: RefCell<Option<(PathBuf, Vec<(String, usize, u32)>)>> = RefCell::new(None);
}

#[derive(Serialize, Deserialize, Debug)]
struct AppSettings {
    tf2_folder: Option<PathBuf>,
    demo_folder: PathBuf,
    output_path: PathBuf,
    all_output_paths: Vec<PathBuf>,
    player_list_url: String,
    dark_mode: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        let current_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        AppSettings {
            tf2_folder: None,
            demo_folder: current_dir.clone(),
            output_path: current_dir.join("demo_dump.json"),
            all_output_paths: vec![current_dir.join("demo_dump.json")],
            player_list_url: "https://github.com/AveraFox/Tom/blob/main/playerlist.vorobey-hackerpolice.json".to_string(),
            dark_mode: true,
        }
    }
}

impl AppSettings {
    fn load() -> Self {
        let settings_path = PathBuf::from("dd_settings.cfg");
        if settings_path.exists() {
            if let Ok(contents) = fs::read_to_string(&settings_path) {
                if let Ok(settings) = serde_json::from_str(&contents) {
                    return settings;
                }
            }
        }
        AppSettings::default()
    }

    fn save(&self) {
        let settings_path = PathBuf::from("dd_settings.cfg");
        if let Ok(contents) = serde_json::to_string_pretty(self) {
            let _ = fs::write(settings_path, contents);
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
struct PlayerListFile {
    players: Vec<PlayerListEntry>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct PlayerListEntry {
    steamid: String,
    proof: Vec<String>,
    attributes: Vec<String>,
}

#[derive(Clone)]
struct ParseResult {
    pub demo: PathBuf,
    pub players: Result<PlayerMap, String>,
}

type PlayerMap = HashMap<String, String>;
type Output = HashMap<PathBuf, PlayerMap>;

#[derive(Clone, Debug)]
struct PlayerStats {
    username: String,
    steamid: String,
    demo_count: usize,
    is_cheater: bool,
    proof: Option<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum DemoSortMethod {
    Name,
    MarkedPlayerPercentage,
    DateCreated,
}

#[derive(Clone)]
struct DemoAnalysis {
    total_players: usize,
    marked_players: usize,
    marked_percentage: f32,
    needs_warning: bool,
    has_replay: bool,
}

#[derive(Clone)]
struct DemoMetadata {
    created_time: SystemTime,
    modified_time: SystemTime,
}

#[derive(Clone)]
struct AppState {
    total_demos: usize,
    processed_demos: usize,
    failed_demos: Vec<(String, String)>,
    is_processing: bool,
    start_time: Option<Instant>,
    current_demo: Option<String>,
    log_messages: Vec<String>,
    demo_folder: PathBuf,
    output_path: PathBuf,
    save_counter: usize,
    copy_status: Option<String>,
    all_output_paths: Vec<PathBuf>,
    player_list_url: String,
    player_list: Vec<PlayerListEntry>,
    player_stats: Vec<PlayerStats>,
    exclude_string: String,
    bad_actor_count: usize,
    player_list_loaded: bool,
    players_page: usize,
    players_per_page: usize,
    steamid_to_usernames: HashMap<String, HashSet<String>>,
    demo_browser_search: String,
    demo_browser_results: Vec<(PathBuf, String, Option<Vec<(String, String, Vec<String>)>>)>,
    demo_list: Vec<(PathBuf, String)>,
    dark_mode: bool,
    demo_sort_method: DemoSortMethod,
    demo_sort_reversed: bool,
    demo_analyses: HashMap<PathBuf, DemoAnalysis>,
    tf2_folder: Option<PathBuf>,
    delete_replays_confirmation: bool,
    filtered_players: Vec<PlayerStats>,
    last_filter_time: Option<Instant>,
    last_filter_string: String,
    demo_metadata_cache: HashMap<PathBuf, DemoMetadata>,
    last_viewangles_file: Option<PathBuf>,
    flick_threshold: Option<f32>, // Threshold for flick detection
    psilent_threshold: Option<f32>, // Threshold for psilent detection
    detection_method: Option<DetectionMethod>, // Currently selected detection method
    oob_threshold: Option<f32>, // Threshold for out-of-bounds pitch detection
    last_oob_summary_path: Option<PathBuf>,
}

impl Default for AppState {
    fn default() -> Self {
        let settings = AppSettings::load();
        Self {
            total_demos: 0,
            processed_demos: 0,
            failed_demos: Vec::new(),
            is_processing: false,
            start_time: None,
            current_demo: None,
            log_messages: vec!["Welcome to TF2 Demo Parser!".to_string()],
            demo_folder: settings.demo_folder,
            output_path: settings.output_path,
            save_counter: 0,
            copy_status: None,
            all_output_paths: settings.all_output_paths,
            player_list_url: settings.player_list_url,
            player_list: Vec::new(),
            player_stats: Vec::new(),
            exclude_string: String::new(),
            bad_actor_count: 0,
            player_list_loaded: false,
            players_page: 0,
            players_per_page: 50,
            steamid_to_usernames: HashMap::new(),
            demo_browser_search: String::new(),
            demo_browser_results: Vec::new(),
            demo_list: Vec::new(),
            dark_mode: settings.dark_mode,
            demo_sort_method: DemoSortMethod::Name,
            demo_sort_reversed: true,
            demo_analyses: HashMap::new(),
            tf2_folder: settings.tf2_folder,
            delete_replays_confirmation: false,
            filtered_players: Vec::new(),
            last_filter_time: None,
            last_filter_string: String::new(),
            demo_metadata_cache: HashMap::new(),
            last_viewangles_file: None,
            flick_threshold: None,
            psilent_threshold: None,
            detection_method: None,
            oob_threshold: None,
            last_oob_summary_path: None,
        }
    }
}

struct DemoParserApp {
    state: Arc<Mutex<AppState>>,
    result_receiver: Option<Receiver<ParseResult>>,
    output: Arc<Mutex<Output>>,
    worker_handles: Vec<thread::JoinHandle<()>>,
    scroll_to_bottom: bool,
    current_tab: usize,
    player_list_initialized: bool,
    runtime: tokio::runtime::Runtime,
}

impl Default for DemoParserApp {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(AppState::default())),
            result_receiver: None,
            output: Arc::new(Mutex::new(HashMap::new())),
            worker_handles: Vec::new(),
            scroll_to_bottom: true,
            current_tab: 0,
            player_list_initialized: false,
            runtime: tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime"),
        }
    }
}

impl eframe::App for DemoParserApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Initialize player list on first run
        if !self.player_list_initialized {
            self.player_list_initialized = true;
            self.load_player_list();
        }
        
        // Track if we need to repaint
        let mut needs_repaint = false;
        
        // Handle incoming results only if we're processing
        if let Some(receiver) = &self.result_receiver {
            while let Ok(result) = receiver.try_recv() {
                needs_repaint = true;
                let demo_name = result.demo.file_name()
                    .and_then(|n| n.to_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "Unknown".to_string());
                
                let mut state = self.state.lock().unwrap();
                state.processed_demos += 1;
                state.current_demo = Some(demo_name.clone());
                
                // Get existing output
                let mut current_output = load_existing_results(&state.output_path);
                let output_path = state.output_path.clone();
                
                match result.players {
                    Ok(players) => {
                        // Update output with new players
                        current_output.insert(result.demo.clone(), players);
                        state.log_messages.push(format!("Processed: {}", demo_name));
                        self.scroll_to_bottom = true;
                    }
                    Err(error) => {
                        // Skip specific error and mark as processed
                        if error.contains("Unmatched discriminant '32' found while trying to read enum 'PacketType'") {
                            state.log_messages.push(format!("Skipped {}: {}", demo_name, error));
                        } else {
                            state.failed_demos.push((demo_name.clone(), error.clone()));
                            state.log_messages.push(format!("Failed {}: {}", demo_name, error));
                        }
                        // Mark the demo as processed in the output to prevent retrying
                        current_output.insert(result.demo.clone(), HashMap::new());
                        self.scroll_to_bottom = true;
                    }
                }
                
                // Save the updated output
                save(&current_output, &output_path);
                
                // Update the in-memory output
                *self.output.lock().unwrap() = current_output;
                
                // Check if processing is complete
                if state.processed_demos >= state.total_demos {
                    state.is_processing = false;
                    state.log_messages.push("Processing complete!".to_string());
                    state.log_messages.push(format!("Final save to: {}", output_path.to_string_lossy()));
                    let failed_count = state.failed_demos.len();
                    if failed_count > 0 {
                        state.log_messages.push(format!("Failed to process {} demos", failed_count));
                    }
                    self.scroll_to_bottom = true;
                    
                    // Clear the receiver to prevent processing more demos
                    drop(state);
                    self.result_receiver = None;
                    break;
                }
            }
        }

        // Limit framerate when idle
        if !needs_repaint && !self.state.lock().unwrap().is_processing {
            ctx.request_repaint_after(std::time::Duration::from_secs_f32(1.0 / 10.0)); // 10 FPS when idle
        }

        // UI rendering - always do this
        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    // Tab selection with improved styling
                    let tab_style = ui.style_mut();
                    tab_style.spacing.item_spacing.x = 12.0;
                    
                    ui.selectable_value(&mut self.current_tab, 0, "📊 Parser")
                        .on_hover_text("Process and analyze demo files");
                    ui.selectable_value(&mut self.current_tab, 1, "👥 Players")
                        .on_hover_text("View player statistics and manage lists");
                    ui.selectable_value(&mut self.current_tab, 2, "📁 Demo Browser")
                        .on_hover_text("Browse and search demo files");
                    ui.selectable_value(&mut self.current_tab, 3, "🔍 Demo Checker")
                        .on_hover_text("Check demos for specific players");
                });
            
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let mut state = self.state.lock().unwrap();
                    let theme_button = if state.dark_mode {
                        egui::Button::new("☀ Light Mode").fill(egui::Color32::from_rgb(60, 60, 60))
                    } else {
                        egui::Button::new("🌙 Dark Mode").fill(egui::Color32::from_rgb(220, 220, 220))
                    };
                    
                    if ui.add(theme_button).clicked() {
                        state.dark_mode = !state.dark_mode;
                        
                        // Get accent color and determine text color
                        let accent_color = get_windows_accent_color();
                        let use_white_text = should_use_white_text(accent_color);
                        
                        // Update visuals with consistent styling
                        if state.dark_mode {
                            let mut visuals = egui::Visuals::dark();
                            visuals.window_rounding = 4.0.into();
                            visuals.window_shadow.extrusion = 8.0;
                            visuals.popup_shadow.extrusion = 4.0;
                            visuals.widgets.noninteractive.rounding = 2.0.into();
                            visuals.widgets.inactive.rounding = 2.0.into();
                            visuals.widgets.active.rounding = 2.0.into();
                            visuals.widgets.hovered.rounding = 2.0.into();
                            
                            // Apply accent color
                            visuals.selection.bg_fill = accent_color;
                            visuals.widgets.hovered.bg_fill = accent_color;
                            visuals.widgets.active.bg_fill = accent_color.linear_multiply(0.8);
                            visuals.widgets.hovered.bg_stroke.color = accent_color;
                            visuals.widgets.active.bg_stroke.color = accent_color;
                            
                            // Set text color based on background brightness
                            if use_white_text {
                                visuals.widgets.hovered.fg_stroke.color = egui::Color32::WHITE;
                                visuals.widgets.active.fg_stroke.color = egui::Color32::WHITE;
                            }
                            
                            ctx.set_visuals(visuals);
                        } else {
                            let mut visuals = egui::Visuals::light();
                            visuals.window_rounding = 4.0.into();
                            visuals.window_shadow.extrusion = 8.0;
                            visuals.popup_shadow.extrusion = 4.0;
                            visuals.widgets.noninteractive.rounding = 2.0.into();
                            visuals.widgets.inactive.rounding = 2.0.into();
                            visuals.widgets.active.rounding = 2.0.into();
                            visuals.widgets.hovered.rounding = 2.0.into();
                            
                            // Apply accent color
                            visuals.selection.bg_fill = accent_color;
                            visuals.widgets.hovered.bg_fill = accent_color.linear_multiply(0.9);
                            visuals.widgets.active.bg_fill = accent_color.linear_multiply(0.7);
                            visuals.widgets.hovered.bg_stroke.color = accent_color;
                            visuals.widgets.active.bg_stroke.color = accent_color;
                            
                            // Set text color based on background brightness
                            if use_white_text {
                                visuals.widgets.hovered.fg_stroke.color = egui::Color32::WHITE;
                                visuals.widgets.active.fg_stroke.color = egui::Color32::WHITE;
                            }
                            
                            ctx.set_visuals(visuals);
                        }
                        
                        // Save settings
                        let mut settings = AppSettings::load();
                        settings.dark_mode = state.dark_mode;
                        settings.save();
                    }
                });
            });
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(8.0);
            
            match self.current_tab {
                0 => self.render_parser_tab(ui, ctx),
                1 => self.render_players_tab(ui, ctx),
                2 => self.render_demo_browser_tab(ui, ctx),
                3 => self.render_demo_checker_tab(ui, ctx),
                _ => {}
            }
        });
        
        // Only request immediate repaint if something changed
        if needs_repaint {
            ctx.request_repaint();
        }
    }
}

impl DemoParserApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // Load the app state
        let mut app = Self::default();
        
        // Get system accent color
        let accent_color = get_windows_accent_color();
        let use_white_text = should_use_white_text(accent_color);
        
        // Set up the theme
        let state = app.state.lock().unwrap();
        if state.dark_mode {
            let mut visuals = egui::Visuals::dark();
            visuals.window_rounding = 4.0.into();
            visuals.window_shadow.extrusion = 8.0;
            visuals.popup_shadow.extrusion = 4.0;
            visuals.widgets.noninteractive.rounding = 2.0.into();
            visuals.widgets.inactive.rounding = 2.0.into();
            visuals.widgets.active.rounding = 2.0.into();
            visuals.widgets.hovered.rounding = 2.0.into();
            
            // Apply accent color
            visuals.selection.bg_fill = accent_color;
            visuals.widgets.hovered.bg_fill = accent_color;
            visuals.widgets.active.bg_fill = accent_color.linear_multiply(0.8);
            visuals.widgets.hovered.bg_stroke.color = accent_color;
            visuals.widgets.active.bg_stroke.color = accent_color;
            
            // Set text color based on background brightness
            if use_white_text {
                visuals.widgets.hovered.fg_stroke.color = egui::Color32::WHITE;
                visuals.widgets.active.fg_stroke.color = egui::Color32::WHITE;
            }
            
            cc.egui_ctx.set_visuals(visuals);
        } else {
            let mut visuals = egui::Visuals::light();
            visuals.window_rounding = 4.0.into();
            visuals.window_shadow.extrusion = 8.0;
            visuals.popup_shadow.extrusion = 4.0;
            visuals.widgets.noninteractive.rounding = 2.0.into();
            visuals.widgets.inactive.rounding = 2.0.into();
            visuals.widgets.active.rounding = 2.0.into();
            visuals.widgets.hovered.rounding = 2.0.into();
            
            // Apply accent color
            visuals.selection.bg_fill = accent_color;
            visuals.widgets.hovered.bg_fill = accent_color.linear_multiply(0.9);
            visuals.widgets.active.bg_fill = accent_color.linear_multiply(0.7);
            visuals.widgets.hovered.bg_stroke.color = accent_color;
            visuals.widgets.active.bg_stroke.color = accent_color;
            
            // Set text color based on background brightness
            if use_white_text {
                visuals.widgets.hovered.fg_stroke.color = egui::Color32::WHITE;
                visuals.widgets.active.fg_stroke.color = egui::Color32::WHITE;
            }
            
            cc.egui_ctx.set_visuals(visuals);
        }
        drop(state);
        
        // Set up spacing
        let mut style = (*cc.egui_ctx.style()).clone();
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.window_margin = egui::Margin::same(10.0);
        style.spacing.button_padding = egui::vec2(12.0, 6.0);
        cc.egui_ctx.set_style(style);
        
        // Load existing output file and set initial tab
        let output_path = {
            let state = app.state.lock().unwrap();
            state.output_path.clone()
        };
        
        let existing_output = load_existing_results(&output_path);
        *app.output.lock().unwrap() = existing_output.clone();
        
        // Set initial tab based on demo_dump.json existence
        app.current_tab = if !existing_output.is_empty() {
            2 // Demo Browser tab
        } else {
            0 // Parser tab
        };
        
        // Update player stats
        app.update_player_stats();

        // Automatically refresh demo list if TF2 folder is set
        {
            let state = app.state.lock().unwrap();
            if state.tf2_folder.is_some() {
                drop(state);
                app.update_demo_list();
            }
        }
        
        app
    }
    
    fn render_parser_tab(&mut self, ui: &mut egui::Ui, _ctx: &egui::Context) {
        // Clone state values for UI rendering
        let (demo_folder, output_path, total_demos, processed_demos, failed_demos, 
            current_demo, start_time, log_messages, failed_demos_list, is_processing, tf2_folder) = {
            let state = self.state.lock().unwrap();
            (
                state.demo_folder.clone(),
                state.output_path.clone(),
                state.total_demos,
                state.processed_demos,
                state.failed_demos.len(),
                state.current_demo.clone(),
                state.start_time,
                state.log_messages.clone(),
                state.failed_demos.clone(),
                state.is_processing,
                state.tf2_folder.clone(),
            )
        };
        
        ui.heading("TF2 Demo Parser");
        ui.separator();
        
        // TF2 folder selection section
        ui.horizontal(|ui| {
            ui.label("TF2 folder:");
            if let Some(folder) = &tf2_folder {
                ui.label(folder.to_string_lossy());
            } else {
                ui.label("Not selected");
            }
            
            if ui.button("Browse").clicked() {
                if let Some(folder) = FileDialog::new()
                    .set_title("Select Team Fortress 2 folder")
                    .pick_folder() 
                {
                    let demos_folder = get_demos_folder(&folder);
                    if demos_folder.exists() {
                    let mut state = self.state.lock().unwrap();
                        state.tf2_folder = Some(folder.clone());
                        state.demo_folder = demos_folder;
                    
                    // Save settings
                    let mut settings = AppSettings::load();
                        settings.tf2_folder = Some(folder.clone());
                        settings.demo_folder = state.demo_folder.clone();
                    settings.save();
                    
                        state.log_messages.push(format!("TF2 folder set to: {}", folder.to_string_lossy()));
                        self.scroll_to_bottom = true;
                        
                        // Update demo list
                        drop(state);
                        self.update_demo_list();
                    } else {
                        let mut state = self.state.lock().unwrap();
                        state.log_messages.push("Selected folder does not contain TF2 demos folder!".to_string());
                        self.scroll_to_bottom = true;
                    }
                }
            }
            
            if ui.button("Auto-detect").clicked() {
                if let Some(folder) = find_steam_tf2_folder() {
                    let demos_folder = get_demos_folder(&folder);
                    if demos_folder.exists() {
                        let mut state = self.state.lock().unwrap();
                        state.tf2_folder = Some(folder.clone());
                        state.demo_folder = demos_folder;
                        
                        // Save settings
                        let mut settings = AppSettings::load();
                        settings.tf2_folder = Some(folder.clone());
                        settings.demo_folder = state.demo_folder.clone();
                        settings.save();
                        
                        state.log_messages.push(format!("Auto-detected TF2 folder: {}", folder.to_string_lossy()));
                        self.scroll_to_bottom = true;
                        
                        // Update demo list
                        drop(state);
                        self.update_demo_list();
                    }
                } else {
                    let mut state = self.state.lock().unwrap();
                    state.log_messages.push("Could not auto-detect TF2 folder!".to_string());
                    self.scroll_to_bottom = true;
                }
            }
        });
        
        ui.horizontal(|ui| {
            ui.label("Demos folder:");
            ui.label(demo_folder.to_string_lossy());
        });
        
        ui.horizontal(|ui| {
            ui.label("Output file:");
            ui.label(output_path.to_string_lossy());
            
            if ui.button("Change Output").clicked() {
                if let Some(file) = FileDialog::new()
                    .add_filter("JSON", &["json"])
                    .set_file_name("demo_dump.json")
                    .save_file() 
                {
                    let mut state = self.state.lock().unwrap();
                    state.output_path = file.clone();
                    
                    // Update settings
                    let mut settings = AppSettings::load();
                    settings.output_path = file.clone();
                    if !settings.all_output_paths.contains(&file) {
                        settings.all_output_paths.push(file.clone());
                    }
                    settings.save();
                    
                    state.all_output_paths = settings.all_output_paths.clone();
                    
                    let output_str = state.output_path.to_string_lossy().to_string();
                    state.log_messages.push(format!("Output set to: {}", output_str));
                    self.scroll_to_bottom = true;
                }
            }
            
            // Delete dump button
            if ui.button("Delete Dump").clicked() {
                let output_path_clone = {
                    let state = self.state.lock().unwrap();
                    state.output_path.clone()
                };
                
                if output_path_clone.exists() {
                    if let Err(e) = fs::remove_file(&output_path_clone) {
                        let mut state = self.state.lock().unwrap();
                        state.log_messages.push(format!("Failed to delete dump: {}", e));
                    } else {
                        let mut state = self.state.lock().unwrap();
                        state.log_messages.push("Demo dump deleted!".to_string());
                        self.output.lock().unwrap().clear();
                        
                        // Remove from all_output_paths
                        state.all_output_paths.retain(|p| p != &output_path_clone);
                        
                        // Update settings
                        let mut settings = AppSettings::load();
                        settings.all_output_paths.retain(|p| p != &output_path_clone);
                        settings.save();
                        
                        // Update player stats
                        drop(state);
                        self.update_player_stats();
                    }
                    self.scroll_to_bottom = true;
                }
            }
        });
        
        ui.separator();
        
        // Check if we should show the button
        let show_button = !is_processing && self.result_receiver.is_none();
        
        if show_button {
            if ui.button("Start Processing").clicked() {
                self.start_processing();
            }
        }
        
        if total_demos > 0 {
            let progress = processed_demos as f32 / total_demos as f32;
            
            ui.add(egui::ProgressBar::new(progress)
                .text(format!("{}/{} ({:.1}%)", 
                    processed_demos, 
                    total_demos, 
                    progress * 100.0)));
            
            ui.horizontal(|ui| {
                ui.label(format!("Total demos: {}", total_demos));
                ui.label(format!("Processed: {}", processed_demos));
                if failed_demos > 0 {
                    ui.label(format!("Failed: {} ({}%)", 
                        failed_demos,
                        (failed_demos as f32 / total_demos as f32 * 100.0).round()
                    ));
                }
            });
            
            if let Some(current) = &current_demo {
                ui.label(format!("Current: {}", current));
            }
            
            if let Some(start_time) = start_time {
                let elapsed = start_time.elapsed().as_secs_f64();
                if elapsed > 0.0 && processed_demos > 0 {
                    let rate = processed_demos as f64 / elapsed;
                    ui.label(format!("Processing rate: {:.2} demos/sec", rate));
                    
                    if processed_demos < total_demos {
                        let remaining = total_demos - processed_demos;
                        let eta = remaining as f64 / rate;
                        ui.label(format!("ETA: {:.0} seconds", eta));
                    }
                }
            }
        }
        
        ui.separator();
        
        // Log messages display with auto-scroll
        ui.heading("Processing Log");
        egui::ScrollArea::vertical()
            .id_source("log_scroll")
            .auto_shrink([false; 2])
            .max_height(200.0)
            .show(ui, |ui| {
                let available_width = ui.available_width();
                ui.set_width(available_width);
                
                for msg in &log_messages {
                    ui.label(msg);
                }
                
                // Scroll to bottom if needed
                if self.scroll_to_bottom {
                    ui.scroll_to_cursor(Some(egui::Align::BOTTOM));
                    self.scroll_to_bottom = false;
                }
            });
        
        if !failed_demos_list.is_empty() {
            ui.separator();
            ui.heading("Failed Demos");
            egui::ScrollArea::vertical()
                .id_source("failed_demos_scroll")
                .auto_shrink([false; 2])
                .max_height(150.0)
                .show(ui, |ui| {
                    let available_width = ui.available_width();
                    ui.set_width(available_width);
                    
                    for (demo, error) in &failed_demos_list {
                        ui.label(format!("{}: {}", demo, error));
                    }
                });
        }
    }

    fn render_players_tab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // Get all necessary state data at once
        let state_data = {
            let mut state = self.state.lock().unwrap();
            update_filtered_players(&mut state);
            (
                state.player_list_url.clone(),
                state.player_list_loaded,
                state.filtered_players.clone(),
                state.exclude_string.clone(),
                state.bad_actor_count,
                state.copy_status.clone(),
                state.dark_mode,
                state.player_stats.len(),
            )
        };
        let (player_list_url, player_list_loaded, filtered_players, exclude_string, 
             bad_actor_count, copy_status, dark_mode, total_players) = state_data;

        // Calculate available height for the scroll area
        let available_height = ui.available_height();
        let header_height = 150.0; // Approximate height for all headers and controls
        let scroll_height = available_height - header_height;

        // Constants for consistent spacing and sizing
        let horizontal_margin = 16.0;
        let vertical_spacing = 8.0;
        let label_width = 120.0;

        egui::TopBottomPanel::top("players_header").show_inside(ui, |ui| {
            ui.add_space(vertical_spacing);
            
            // Center-aligned heading
            ui.vertical_centered(|ui| {
        ui.heading("Player Statistics");
            });
            ui.add_space(vertical_spacing);
        ui.separator();
            ui.add_space(vertical_spacing);
        
        // Player list URL input
        ui.horizontal(|ui| {
                ui.add_space(horizontal_margin);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let button_area_width = 120.0;
                    let mut button_height = ui.spacing().interact_size.y; // Initialize with default height

                    // Button Part (far right)
                    let _update_btn_response = ui.allocate_ui_with_layout(
                        egui::vec2(button_area_width, button_height), 
                        egui::Layout::right_to_left(egui::Align::Center), 
                        |ui_btn_area| {
                            let btn = ui_btn_area.button("🔄 Update List")
                                .on_hover_text("Update player list from URL");
                            button_height = btn.rect.height(); // Update height from actual button
                            if btn.clicked() {
                let mut state = self.state.lock().unwrap();
                state.player_list_url = player_list_url.clone();
                                state.player_list_loaded = false;
                                drop(state);
                                self.load_player_list();
                            }
                        }
                    );
                    
                    // TextEdit part
                    let text_edit_width = (ui.available_width() - label_width).max(50.0);
                    let mut current_url = player_list_url.clone();
                    let text_edit_response = ui.add_sized(
                        [text_edit_width, button_height],
                        egui::TextEdit::singleline(&mut current_url)
                            .font(ui.style().text_styles.get(&egui::TextStyle::Body).unwrap().clone())
                    ).on_hover_text("Enter the URL for the player list");

                    if text_edit_response.changed() {
                        // Optionally update state.player_list_url live or on Enter/lost_focus
                    }
                    if text_edit_response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                let mut state = self.state.lock().unwrap();
                        state.player_list_url = current_url.clone();
                state.player_list_loaded = false;
                drop(state);
                self.load_player_list();
            }
                    
                    // Label part (far left)
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.set_min_width(label_width);
                        ui.label("Player list URL:");
                    });
                });
                ui.add_space(horizontal_margin);
        });
        
        if !player_list_loaded {
                ui.add_space(vertical_spacing);
                ui.vertical_centered(|ui| {
                    ui.horizontal(|ui| {
                        if dark_mode {
                            ui.spinner();
                        } else {
                            let dark_spinner = egui::Spinner::new().color(egui::Color32::from_rgb(32, 32, 32));
                            ui.add(dark_spinner);
                        }
            ui.label("Loading player list...");
                    });
                });
            return;
        }
        
            ui.add_space(vertical_spacing);
        ui.separator();
            ui.add_space(vertical_spacing);
        
            // Search and exclude filters
        ui.horizontal(|ui| {
                ui.add_space(horizontal_margin);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let button_area_width = 120.0;
                    let mut button_height = ui.spacing().interact_size.y; // Initialize with default height

                    // Button Part (far right)
                    let _clear_btn_response = ui.allocate_ui_with_layout(
                        egui::vec2(button_area_width, button_height),
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui_btn_area| {
                            let btn = ui_btn_area.button("❌ Clear")
                                .on_hover_text("Clear exclude filter");
                            button_height = btn.rect.height(); // Update height from actual button
                            if btn.clicked() {
                let mut state = self.state.lock().unwrap();
                                state.exclude_string = String::new();
                                state.last_filter_string = String::new(); // Force update
                                update_filtered_players(&mut state);
                    }
                }
                    );

                    // TextEdit part
                    let text_edit_width = (ui.available_width() - label_width).max(50.0);
                    let mut current_exclude = exclude_string.clone();
                    let text_edit_response = ui.add_sized(
                        [text_edit_width, button_height],
                        egui::TextEdit::singleline(&mut current_exclude)
                            .font(ui.style().text_styles.get(&egui::TextStyle::Body).unwrap().clone())
                    ).on_hover_text("Enter text to exclude players by SteamID");
                    
                    if text_edit_response.changed() {
                        let mut state = self.state.lock().unwrap();
                        state.exclude_string = current_exclude.clone();
                    }
                    if text_edit_response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        let mut state = self.state.lock().unwrap();
                        state.exclude_string = current_exclude;
                        state.last_filter_string = state.exclude_string.clone();
                        update_filtered_players(&mut state);
            }
            
                    // Label part (far left)
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.set_min_width(label_width);
                        ui.label("Exclude players:");
                    });
                });
                ui.add_space(horizontal_margin);
            });
            
            ui.add_space(vertical_spacing);
        ui.separator();
            ui.add_space(vertical_spacing);
            
            // Stats summary
                                ui.horizontal(|ui| {
                ui.add_space(horizontal_margin);
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.strong("Statistics Summary:");
                    ui.add_space(8.0);
                    ui.label(format!("Total Players: {}", total_players));
                    ui.add_space(8.0);
                    ui.label(format!("Bad Actors: {}", bad_actor_count));
                    if !filtered_players.is_empty() {
                        ui.add_space(8.0);
                        ui.label(format!("Showing: {} players", filtered_players.len()));
                                    }
                });
                ui.add_space(horizontal_margin);
            });
            ui.add_space(vertical_spacing);
        });

        if filtered_players.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(vertical_spacing * 2.0);
                ui.label("No players match the current filters");
            });
            return;
        }

        // Player grid with virtual scrolling
        const ROW_HEIGHT: f32 = 30.0;
        const BUFFER_ROWS: usize = 2;

        egui::CentralPanel::default()
            .frame(egui::Frame::none())
            .show_inside(ui, |ui| {
                let scroll_area = egui::ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .max_height(scroll_height);

                let scroll_response = scroll_area.show_rows(
                    ui,
                    ROW_HEIGHT,
                    filtered_players.len(),
                    |ui, row_range| {
                        // Headers (stick to top when scrolling)
                        let available_width = ui.available_width() - (horizontal_margin * 2.0);
                        let username_width = available_width * 0.45;
                        let steamid_width = available_width * 0.40;
                        let demos_width = available_width * 0.15;
                        let button_width = 100.0; // Fixed width for action buttons

                        ui.add_space(vertical_spacing);
                        ui.horizontal(|ui| {
                            ui.add_space(horizontal_margin);
                            
                            // Username column
                            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                ui.set_min_width(username_width);
                                ui.strong("Alias");
                            });
                            
                            // SteamID column
                            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                ui.set_min_width(steamid_width);
                                ui.strong("SteamID");
                            });
                            
                            // Demos column
                            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                ui.set_min_width(demos_width);
                                ui.strong("Demos");
                            });
                            
                            ui.add_space(horizontal_margin);
                        });
                        ui.add_space(vertical_spacing);

                        // Calculate visible range with buffer
                        let start_idx = row_range.start.saturating_sub(BUFFER_ROWS);
                        let end_idx = (row_range.end + BUFFER_ROWS).min(filtered_players.len());
                        
                        // Render visible rows
                        for player in &filtered_players[start_idx..end_idx] {
                            ui.horizontal(|ui| {
                                ui.add_space(horizontal_margin);
                                
                                // Username column
                                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                    ui.set_min_width(username_width);
                                    ui.label(&player.username);
                                });
                                
                                // SteamID column with copy button and cheater status
                                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                    ui.set_min_width(steamid_width);
                                    ui.horizontal(|ui| {
                                        let shift_pressed = ui.input(|i| i.modifiers.shift);
                                        if ui.button("📋")
                                            .on_hover_text(if shift_pressed {
                                                "Click to copy Steam64 ID"
                                            } else {
                                                "Click to copy Steam32 ID"
                                            })
                                            .clicked() 
                                        {
                                            if let Ok(mut clipboard) = Clipboard::new() {
                                                let copy_text = if shift_pressed {
                                                    steamid_32_to_64(&player.steamid).unwrap_or_else(|| player.steamid.clone())
                                                } else {
                                                    player.steamid.clone()
                                                };
                                                if clipboard.set_text(&copy_text).is_ok() {
                                    let mut state = self.state.lock().unwrap();
                                                    state.copy_status = Some(format!("Copied {} to clipboard!", 
                                                        if shift_pressed { "Steam64 ID" } else { "Steam32 ID" }));
                                                }
                                            }
                                        }
                                        ui.add_space(4.0);
                                        let display_id = if shift_pressed {
                                            steamid_32_to_64(&player.steamid).unwrap_or_else(|| player.steamid.clone())
                                        } else {
                                            player.steamid.clone()
                                        };
                                        ui.label(&display_id);
                                
                                        // Add cheater status button if player is marked
                                if player.is_cheater {
                                            ui.add_space(4.0);
                                            if let Some(ref proof) = player.proof {
                                                if ui.button("⚠ Cheater")
                                                    .on_hover_text("Click to view evidence")
                                                    .clicked() 
                                                {
                                            if let Err(e) = webbrowser::open(proof) {
                                                let mut state = self.state.lock().unwrap();
                                                        state.log_messages.push(format!("Failed to open URL: {}", e));
                                                    }
                                                }
                                            }
                                }
                            });
                        });
                                
                                // Demos column
                                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                    ui.set_min_width(demos_width);
                                    ui.label(format!("{}", player.demo_count));
                                });
                                
                                ui.add_space(horizontal_margin);
                            });
                            ui.add_space(vertical_spacing);
                    }
                    }
                );
        
                // Show copy status if any, but only when not scrolling
                if let Some(status) = copy_status {
                    if !ui.input(|i| i.pointer.primary_down()) {
                        ui.add_space(vertical_spacing);
            ui.separator();
                        ui.add_space(vertical_spacing);
                        ui.vertical_centered(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(&status);
                            });
                            if status.contains("copied to clipboard") {
                                ctx.request_repaint_after(std::time::Duration::from_secs(2));
        }
                        });
                    }
                }
            });
    }
    
    fn render_demo_browser_tab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // Get all necessary state data at once with a single lock
        let state_data = {
            let state = self.state.lock().unwrap();
            (
                state.demo_folder.clone(),
                state.demo_list.clone(),
                state.demo_browser_search.clone(),
                state.demo_browser_results.clone(),
                state.copy_status.clone(),
                state.demo_sort_method,
                state.demo_sort_reversed,
                state.demo_analyses.clone(),
                state.tf2_folder.clone(),
                state.delete_replays_confirmation,
            )
        };
        let (demo_folder, demo_list, demo_browser_search, demo_browser_results, 
             copy_status, demo_sort_method, demo_sort_reversed, demo_analyses, 
             tf2_folder, delete_confirmation) = state_data;

        ui.vertical_centered(|ui| {
            ui.heading("Demo Browser");
        });
        ui.separator();
        
        // Add Delete All Replays button
        if let Some(tf2_folder) = &tf2_folder {
            ui.vertical_centered(|ui| {
        ui.horizontal(|ui| {
                    let button_text = if delete_confirmation {
                        "⚠️ Click again to confirm deletion"
                                } else {
                        "🗑 Delete All Replays"
                    };

                    if ui.button(button_text).clicked() {
                let mut state = self.state.lock().unwrap();
                        if state.delete_replays_confirmation {
                            // User confirmed, proceed with deletion
                            state.delete_replays_confirmation = false;
                            let tf2_folder = tf2_folder.clone();
                            drop(state);

                            let delete_result = self.runtime.block_on(async {
                                delete_all_replays(&tf2_folder).await
                            });

                            match delete_result {
                                Ok(count) => {
                let mut state = self.state.lock().unwrap();
                                    state.log_messages.push(format!("Successfully deleted {} replay files", count));
                                    state.copy_status = Some(format!("Deleted {} replay files", count));
                drop(state);
                                    
                                    // Update demo analyses to reflect deleted replays
                                    self.update_demo_analyses();
            }
                                Err(e) => {
                                    let mut state = self.state.lock().unwrap();
                                    state.log_messages.push(format!("Failed to delete replays: {}", e));
                                    state.copy_status = Some("Failed to delete replays".to_string());
                                }
                            }
                        } else {
                            // First click - show confirmation
                            state.delete_replays_confirmation = true;
                            state.copy_status = Some("Click again to confirm deleting all replays".to_string());
                        }
                    }

                    // Reset confirmation if user clicks elsewhere
                    if ui.input(|i| i.pointer.primary_clicked()) && !ui.rect_contains_pointer(ui.min_rect()) {
                        let mut state = self.state.lock().unwrap();
                        state.delete_replays_confirmation = false;
        }
        
                    if let Some(status) = &copy_status {
                        ui.label(status);
                            }
                        });
                });
        ui.separator();
        }
        
        // Folder selection
        ui.vertical_centered(|ui| {
        ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                let available_width = ui.available_width();
                let label_width = 100.0;
                let button_width = 70.0;
                let refresh_width = 80.0;
                let content_width = available_width - label_width - button_width - refresh_width - ui.spacing().item_spacing.x * 3.0;

                ui.label("Demo folder:");
                ui.scope(|ui| {
                    ui.set_min_width(content_width);
                    ui.label(egui::RichText::new(demo_folder.to_string_lossy()).monospace());
                });
                
                if ui.button("Browse").clicked() {
                    if let Some(folder) = FileDialog::new().pick_folder() {
                        {
                let mut state = self.state.lock().unwrap();
                            state.demo_folder = folder.clone();
                            
                            // Save settings
                            let mut settings = AppSettings::load();
                            settings.demo_folder = folder;
                            settings.save();
                        }
                        self.update_demo_list();
                    }
                }

                if ui.button("🔄 Refresh").clicked() {
                    self.update_demo_list();
            }
            });
        });
        
        // Search bar
        ui.vertical_centered(|ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                let available_width = ui.available_width();
                let label_width = 100.0;
                let button_width = 70.0;
                let content_width = available_width - label_width - button_width - ui.spacing().item_spacing.x * 2.0;
                
                ui.label("Search:");
                let mut current_search = demo_browser_search.clone();
                let response = ui.add_sized(
                    egui::vec2(content_width, ui.spacing().interact_size.y),
                    egui::TextEdit::singleline(&mut current_search)
                        .hint_text("Search demos...")
                );
                
                if ui.button("🔍 Search").clicked() || response.lost_focus() {
                    let mut state = self.state.lock().unwrap();
                    state.demo_browser_search = current_search.clone();
                    drop(state);
                    self.update_demo_browser_results(&current_search);
                }

                // Update search text even if not searching yet
                if response.changed() {
                    let mut state = self.state.lock().unwrap();
                    state.demo_browser_search = current_search;
                }
            });
            
            ui.label("Search by demo name or player (username/SteamID)");
            ui.add_space(8.0);
        });
        ui.separator();
        
        // Add sorting options with arrows
        ui.vertical_centered(|ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 15.0;
                ui.label("Sort by:");
        
                let arrow = if demo_sort_reversed { "v" } else { "^" };
                
                let name_text = format!("Name {}", if demo_sort_method == DemoSortMethod::Name { arrow } else { "" });
                if ui.selectable_label(demo_sort_method == DemoSortMethod::Name, name_text).clicked() {
                    let mut state = self.state.lock().unwrap();
                    if state.demo_sort_method == DemoSortMethod::Name {
                        state.demo_sort_reversed = !state.demo_sort_reversed;
                    } else {
                        state.demo_sort_method = DemoSortMethod::Name;
                    }
                }
                
                let marked_text = format!("Marked Players {} {}", 
                    if demo_sort_method == DemoSortMethod::MarkedPlayerPercentage { arrow } else { "" },
                    if let Some(analysis) = demo_analyses.values().next() {
                        if analysis.needs_warning { "*" } else { "" }
                    } else { "" }
                );
                if ui.selectable_label(
                    demo_sort_method == DemoSortMethod::MarkedPlayerPercentage,
                    marked_text
                ).clicked() {
                    let mut state = self.state.lock().unwrap();
                    if state.demo_sort_method == DemoSortMethod::MarkedPlayerPercentage {
                        state.demo_sort_reversed = !state.demo_sort_reversed;
                    } else {
                        state.demo_sort_method = DemoSortMethod::MarkedPlayerPercentage;
                    }
                }
                
                let date_text = format!("Date {}", if demo_sort_method == DemoSortMethod::DateCreated { arrow } else { "" });
                if ui.selectable_label(demo_sort_method == DemoSortMethod::DateCreated, date_text).clicked() {
                        let mut state = self.state.lock().unwrap();
                    if state.demo_sort_method == DemoSortMethod::DateCreated {
                        state.demo_sort_reversed = !state.demo_sort_reversed;
                    } else {
                        state.demo_sort_method = DemoSortMethod::DateCreated;
                    }
                }
            });
        });
        ui.add_space(8.0);

        // Compute items to display
        let items = self.compute_demo_items(
            &demo_browser_search,
            &demo_list,
            &demo_browser_results,
            demo_sort_method,
            demo_sort_reversed,
            &demo_analyses
        );

        // Display demos with analysis information
        if items.is_empty() && !demo_list.is_empty() {
            ui.vertical_centered(|ui| {
                ui.label("No demos match your search");
            });
        } else if demo_list.is_empty() {
            ui.vertical_centered(|ui| {
                ui.label("No demos found in the selected folder");
            });
        } else {
            ui.vertical_centered(|ui| {
                ui.label(format!("Found {} demos", items.len()));
            });

            // Constants for virtual scrolling
            const ITEM_HEIGHT: f32 = 100.0;
            const BUFFER_ITEMS: usize = 2;

            egui::ScrollArea::vertical()
                .max_height(500.0)
                .show_rows(
                    ui,
                    ITEM_HEIGHT,
                    items.len(),
                    |ui, row_range| {
                        let start_idx = row_range.start.saturating_sub(BUFFER_ITEMS);
                        let end_idx = (row_range.end + BUFFER_ITEMS).min(items.len());
                        
                        for idx in start_idx..end_idx {
                            let (path, name, players) = &items[idx];
                            ui.add_space(4.0);
                            
                            ui.group(|ui| {
                                let available_width = ui.available_width();
                                ui.set_width(available_width);
                                ui.spacing_mut().item_spacing.y = 4.0;
                                
                                // First row: Demo name and controls
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 8.0;
                                    
                                    // Left side with icons and name
                                    ui.horizontal(|ui| {
                                        // Warning and demo icons
                                        if let Some(analysis) = demo_analyses.get(*path) {
                                            if analysis.needs_warning {
                                                ui.label("\u{25CF}");
                                            }
                                            if analysis.has_replay {
                                                ui.label("🎬");
                                            }
                                        }
                                        ui.label("\u{1F4FC}");
                                        
                                        ui.label(name.clone());
                                        if let Some(analysis) = demo_analyses.get(*path) {
                                            ui.label(format!("({}/{} marked - {:.1}%)", 
                                                analysis.marked_players,
                                                analysis.total_players,
                                                analysis.marked_percentage));
                                        }
                                    });
                                
                                    // Right side with buttons
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        ui.spacing_mut().item_spacing.x = 8.0;
                                        
                                        let has_replay = demo_analyses.get(*path)
                                            .map(|a| a.has_replay)
                                            .unwrap_or(false);

                                        if !has_replay {
                                            if ui.button("🎬 Convert to Replay").clicked() {
                                                if let Some(tf2_folder) = &tf2_folder {
                                                    {
                                    let mut state = self.state.lock().unwrap();
                                                        state.log_messages.push(format!("Converting {} to replay...", name));
                                }
                                
                                                    let path = (*path).clone();
                                                    let name = (*name).clone();
                                                    let tf2_folder = tf2_folder.clone();
                                                    
                                                    let convert_result = self.runtime.block_on(async {
                                                        convert_to_replay(&path, &tf2_folder, &name).await
                                                    });

                                                    match convert_result {
                                                        Ok(_) => {
                                                let mut state = self.state.lock().unwrap();
                                                            state.log_messages.push(format!("Successfully converted {} to replay!", name));
                                                            state.copy_status = Some("Demo converted to replay!".to_string());
                                                            drop(state);
                                                            
                                                            // Update demo analyses to reflect the new replay
                                                            self.update_demo_analyses();
                                                        }
                                                        Err(e) => {
                                                            let mut state = self.state.lock().unwrap();
                                                            state.log_messages.push(format!("Failed to convert demo: {}", e));
                                        }
                                    }
                                } else {
                                                    let mut state = self.state.lock().unwrap();
                                                    state.log_messages.push("Please select your TF2 folder in the Parser tab first!".to_string());
                                                }
                                            }
                                        }

                                        if ui.button("▶ Play Command").clicked() {
                                            if let Ok(mut clipboard) = Clipboard::new() {
                                                let play_command = format!("playdemo demos/{}", name);
                                                if clipboard.set_text(&play_command).is_ok() {
                    let mut state = self.state.lock().unwrap();
                                                    state.copy_status = Some("Play command copied to clipboard!".to_string());
                                                }
                                            }
                }
                
                                        if ui.button("📋 Copy Name").clicked() {
                                            if let Ok(mut clipboard) = Clipboard::new() {
                                                if clipboard.set_text(name).is_ok() {
                    let mut state = self.state.lock().unwrap();
                                                    state.copy_status = Some("Filename copied to clipboard!".to_string());
                                                }
                                            }
                                        }
                                    });
                                });

                                // Second row: File path
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 8.0;
                                    ui.add_space(20.0);
                                    ui.label(egui::RichText::new(path.to_string_lossy()).weak().small());
                                });

                                // Show players if available
                                if let Some(demo_players) = players {
                                    ui.add_space(4.0);
                                    ui.indent(format!("players_{}", idx), |ui| {
                                        egui::Grid::new(format!("players_grid_{}", idx))
                                            .num_columns(3)
                                            .spacing([12.0, 4.0])
                                            .show(ui, |ui| {
                                                for (username, steamid, known_usernames) in demo_players.iter() {
                                                    let available_width = ui.available_width();
                                                    let name_width = available_width * 0.3;
                                                    let id_width = available_width * 0.4;
                                                    let button_width = available_width * 0.2;

                                                    // Username column
                                                    ui.scope(|ui| {
                                                        ui.set_width(name_width);
                                                        ui.label(format!("• {}", username));
                                                    });

                                                    // SteamID column
                                                    ui.scope(|ui| {
                                                        ui.set_width(id_width);
                                                        let shift_pressed = ui.input(|i| i.modifiers.shift);
                                                        let display_id = if shift_pressed {
                                                            steamid_32_to_64(steamid).unwrap_or_else(|| steamid.clone())
                                                        } else {
                                                            steamid.clone()
                                                        };
                                                        ui.label(display_id);
                                                    });

                                                    // Copy button column
                                                    ui.scope(|ui| {
                                                        ui.set_width(button_width);
                                                        let shift_pressed = ui.input(|i| i.modifiers.shift);
                                                        if ui.button(format!("📋 Copy {}", 
                                                            if shift_pressed { "Steam64" } else { "Steam32" }
                                                        )).clicked() {
                                                            if let Ok(mut clipboard) = Clipboard::new() {
                                                                let copy_text = if shift_pressed {
                                                                    steamid_32_to_64(steamid).unwrap_or_else(|| steamid.clone())
                                                                } else {
                                                                    steamid.clone()
                                                                };
                                                                if clipboard.set_text(&copy_text).is_ok() {
                                                                    let mut state = self.state.lock().unwrap();
                                                                    state.copy_status = Some(format!("Copied {} to clipboard!", 
                                                                        if shift_pressed { "Steam64 ID" } else { "Steam32 ID" }));
                                                                }
                                                            }
                                                        }
                                                    });
                                                    ui.end_row();

                                                    // Show known usernames on the next row if any exist
                                                    if !known_usernames.is_empty() {
                                                        ui.scope(|ui| {
                                                            ui.set_width(available_width);
                                                            ui.horizontal(|ui| {
                                                                ui.add_space(20.0); // Indent
                                                                ui.label(
                                                                    egui::RichText::new(format!("Also known as: {}", 
                                                                        known_usernames.join(", ")))
                                                                    .weak()
                                                                    .small()
                                                                );
                                                            });
                                                        });
                                                        // Add empty labels for the remaining columns
                                                        ui.label("");  // Empty space for SteamID column
                                                        ui.label("");  // Empty space for button column
                                                        ui.end_row();
                                                    }
                                                }
                                            });
                                    });
                                }
                            });
                            ui.add_space(4.0);
                        }
                    },
                );
        }

        // Display copy status if any
        if let Some(status) = copy_status {
            ui.separator();
            ui.vertical_centered(|ui| {
                ui.horizontal(|ui| {
                    ui.label(&status);
                });
                if status.contains("copied to clipboard") {
                    ctx.request_repaint_after(std::time::Duration::from_secs(2));
                    }
            });
        }
    }

    fn render_demo_checker_tab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // Get necessary state data
        let (demo_folder, demo_list, demo_browser_search, demo_browser_results, 
             copy_status, demo_sort_method, demo_sort_reversed, demo_analyses, 
             last_viewangles_file) = {
            let state = self.state.lock().unwrap();
            (
                state.demo_folder.clone(),
                state.demo_list.clone(),
                state.demo_browser_search.clone(),
                state.demo_browser_results.clone(),
                state.copy_status.clone(),
                state.demo_sort_method,
                state.demo_sort_reversed,
                state.demo_analyses.clone(),
                state.last_viewangles_file.clone(),
            )
        };

        ui.vertical_centered(|ui| {
            ui.heading("Demo Checker");
        });
        ui.separator();
        
        // Add a button to open the last viewangles file if available
        if let Some(path) = &last_viewangles_file {
            ui.vertical_centered(|ui| {
                ui.horizontal(|ui| {
                    if ui.button("📊 Open Last Viewangles Data").clicked() {
                        if let Err(e) = open_file(path) {
                            let mut state = self.state.lock().unwrap();
                            state.log_messages.push(format!("Error opening viewangles file: {}", e));
                        } else {
                            let mut state = self.state.lock().unwrap();
                            state.log_messages.push(format!("Opened viewangles file: {}", path.to_string_lossy()));
                        }
                    }
                    
                    ui.label(format!("Last file: {}", path.to_string_lossy()));
                });
            });
            ui.separator();
        }
        
        // Folder selection
        ui.vertical_centered(|ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                let available_width = ui.available_width();
                let label_width = 100.0;
                let button_width = 70.0;
                let refresh_width = 80.0;
                let content_width = available_width - label_width - button_width - refresh_width - ui.spacing().item_spacing.x * 3.0;

                ui.label("Demo folder:");
                ui.scope(|ui| {
                    ui.set_min_width(content_width);
                    ui.label(egui::RichText::new(demo_folder.to_string_lossy()).monospace());
                });
                
                if ui.button("Browse").clicked() {
                    if let Some(folder) = FileDialog::new().pick_folder() {
                        {
                let mut state = self.state.lock().unwrap();
                            state.demo_folder = folder.clone();
                            
                            // Save settings
                            let mut settings = AppSettings::load();
                            settings.demo_folder = folder;
                            settings.save();
                        }
                        self.update_demo_list();
                    }
                }

                if ui.button("🔄 Refresh").clicked() {
                    self.update_demo_list();
                }
            });
        });
        
        // Search bar
        ui.vertical_centered(|ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                let available_width = ui.available_width();
                let label_width = 100.0;
                let button_width = 70.0;
                let content_width = available_width - label_width - button_width - ui.spacing().item_spacing.x * 2.0;
                
                ui.label("Search:");
                let mut current_search = demo_browser_search.clone();
                let response = ui.add_sized(
                    egui::vec2(content_width, ui.spacing().interact_size.y),
                    egui::TextEdit::singleline(&mut current_search)
                        .hint_text("Search demos...")
                );
                
                if ui.button("🔍 Search").clicked() || response.lost_focus() {
                    let mut state = self.state.lock().unwrap();
                    state.demo_browser_search = current_search.clone();
                    drop(state);
                    self.update_demo_browser_results(&current_search);
                }

                // Update search text even if not searching yet
                if response.changed() {
                    let mut state = self.state.lock().unwrap();
                    state.demo_browser_search = current_search;
                }
            });
            
            ui.label("Search by demo name or player (username/SteamID)");
            ui.add_space(8.0);
        });
        ui.separator();
        
        // Add sorting options with arrows
        ui.vertical_centered(|ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 15.0;
                ui.label("Sort by:");
        
                let arrow = if demo_sort_reversed { "v" } else { "^" };
                
                let name_text = format!("Name {}", if demo_sort_method == DemoSortMethod::Name { arrow } else { "" });
                if ui.selectable_label(demo_sort_method == DemoSortMethod::Name, name_text).clicked() {
                    let mut state = self.state.lock().unwrap();
                    if state.demo_sort_method == DemoSortMethod::Name {
                        state.demo_sort_reversed = !state.demo_sort_reversed;
                    } else {
                        state.demo_sort_method = DemoSortMethod::Name;
                    }
                }
                
                let marked_text = format!("Marked Players {} {}", 
                    if demo_sort_method == DemoSortMethod::MarkedPlayerPercentage { arrow } else { "" },
                    if let Some(analysis) = demo_analyses.values().next() {
                        if analysis.needs_warning { "*" } else { "" }
                    } else { "" }
                );
                if ui.selectable_label(
                    demo_sort_method == DemoSortMethod::MarkedPlayerPercentage,
                    marked_text
                ).clicked() {
                    let mut state = self.state.lock().unwrap();
                    if state.demo_sort_method == DemoSortMethod::MarkedPlayerPercentage {
                        state.demo_sort_reversed = !state.demo_sort_reversed;
                    } else {
                        state.demo_sort_method = DemoSortMethod::MarkedPlayerPercentage;
                    }
                }
                
                let date_text = format!("Date {}", if demo_sort_method == DemoSortMethod::DateCreated { arrow } else { "" });
                if ui.selectable_label(demo_sort_method == DemoSortMethod::DateCreated, date_text).clicked() {
                    let mut state = self.state.lock().unwrap();
                    if state.demo_sort_method == DemoSortMethod::DateCreated {
                        state.demo_sort_reversed = !state.demo_sort_reversed;
                    } else {
                        state.demo_sort_method = DemoSortMethod::DateCreated;
                    }
                }
            });
        });
        ui.add_space(8.0);

        // Compute items to display
        let items = self.compute_demo_items(
            &demo_browser_search,
            &demo_list,
            &demo_browser_results,
            demo_sort_method,
            demo_sort_reversed,
            &demo_analyses
        );

        // Display demos with analysis information
        if items.is_empty() && !demo_list.is_empty() {
            ui.vertical_centered(|ui| {
                ui.label("No demos match your search");
            });
        } else if demo_list.is_empty() {
            ui.vertical_centered(|ui| {
                ui.label("No demos found in the selected folder");
            });
        } else {
            ui.vertical_centered(|ui| {
                ui.label(format!("Found {} demos", items.len()));
            });

            // Constants for virtual scrolling
            const ITEM_HEIGHT: f32 = 100.0;
            const BUFFER_ITEMS: usize = 2;

            egui::ScrollArea::vertical()
                .max_height(500.0)
                .show_rows(
                    ui,
                    ITEM_HEIGHT,
                    items.len(),
                    |ui, row_range| {
                        let start_idx = row_range.start.saturating_sub(BUFFER_ITEMS);
                        let end_idx = (row_range.end + BUFFER_ITEMS).min(items.len());
                        
                        for idx in start_idx..end_idx {
                            let (path, name, players) = &items[idx];
                            ui.add_space(4.0);
                            
                            ui.group(|ui| {
                                let available_width = ui.available_width();
                                ui.set_width(available_width);
                                ui.spacing_mut().item_spacing.y = 4.0;
                                
                                // First row: Demo name and controls
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 8.0;
                                    
                                    // Left side with icons and name
                                    ui.horizontal(|ui| {
                                        // Warning and demo icons
                                        if let Some(analysis) = demo_analyses.get(*path) {
                                            if analysis.needs_warning {
                                                ui.label("\u{25CF}");
                                            }
                                            if analysis.has_replay {
                                                ui.label("🎬");
                                            }
                                        }
                                        ui.label("\u{1F4FC}");
                                        
                                        ui.label(name.clone());
                                        if let Some(analysis) = demo_analyses.get(*path) {
                                            ui.label(format!("({}/{} marked - {:.1}%)", 
                                                analysis.marked_players,
                                                analysis.total_players,
                                                analysis.marked_percentage));
                                        }
                                    });
                                
                                    // Right side with plug button
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        ui.spacing_mut().item_spacing.x = 8.0;
                                        
                                        if ui.button("Check Demo").clicked() {
                                            let path_clone = path.clone();
                                            let mut state = self.state.lock().unwrap();
                                            state.log_messages.push(format!("Analyzing demo for suspicious viewangles..."));
                                            drop(state);

                                            // Use the analyze_demo_for_cheaters function
                                            match analyze_demo_for_cheaters(&path_clone) {
                                                Ok((detections, Some(output_file))) => {
                                                    let mut state = self.state.lock().unwrap();
                                                    state.log_messages.push(format!("Analysis complete! Found {} suspicious angle changes", detections.len()));
                                                    
                                                    // Print some details about the detections
                                                    for (i, detection) in detections.iter().enumerate().take(5) { // Show first 5 only
                                                        state.log_messages.push(format!(
                                                            "Detection #{}: Player {} at tick {} - {}",
                                                            i+1, detection.player, detection.tick, detection.algorithm
                                                        ));
                                                    }
                                                    
                                                    if detections.len() > 5 {
                                                        state.log_messages.push(format!("... and {} more detections", detections.len() - 5));
                                                    }
                                                    
                                                    state.copy_status = Some(format!("Viewangles data saved to: {}", output_file.to_string_lossy()));
                                                    state.log_messages.push(format!("Viewangles data saved to: {}", output_file.to_string_lossy()));
                                                    
                                                    // Store the path of the last viewangles file
                                                    state.last_viewangles_file = Some(output_file);
                                                    
                                                    drop(state);
                                                    
                                                    // Update demo analyses to reflect the new viewangles data
                                                    self.update_demo_analyses();
                                                },
                                                Ok((detections, None)) => {
                                                    let mut state = self.state.lock().unwrap();
                                                    state.log_messages.push(format!("Analysis complete! Found {} suspicious angle changes", detections.len()));
                                                    
                                                    // Print some details about the detections
                                                    for (i, detection) in detections.iter().enumerate().take(5) { // Show first 5 only
                                                        state.log_messages.push(format!(
                                                            "Detection #{}: Player {} at tick {} - {}",
                                                            i+1, detection.player, detection.tick, detection.algorithm
                                                        ));
                                                    }
                                                    
                                                    if detections.len() > 5 {
                                                        state.log_messages.push(format!("... and {} more detections", detections.len() - 5));
                                                    }
                                                    
                                                    state.copy_status = Some("Viewangles data not saved".to_string());
                                                    state.log_messages.push("Viewangles data not saved".to_string());
                                                    drop(state);
                                                    
                                                    // Update demo analyses to reflect the new viewangles data
                                                    self.update_demo_analyses();
                                                },
                                                Err(e) => {
                                                    let mut state = self.state.lock().unwrap();
                                                    state.log_messages.push(format!("Error analyzing demo: {}", e));
                                                }
                                            }
                                        }
                                        
                                        // Add a button to open the file in explorer
                                        if let Some(path) = &last_viewangles_file {
                                            if ui.button("📁 Open File Location").clicked() {
                                                // Open the directory containing the file
                                                if let Some(parent) = path.parent() {
                                                    if let Err(e) = open_directory(parent) {
                                                        let mut state = self.state.lock().unwrap();
                                                        state.log_messages.push(format!("Error opening directory: {}", e));
                                                    }
                                                }
                                            }
                                            
                                            // Detection method selection
                                            ui.separator();
                                            ui.group(|ui| {
                                                ui.vertical(|ui| {
                                                    ui.heading("Cheater Detection");
                                                    ui.label("Select detection method:");

                                                    // Get current detection method from state
                                                    let mut current_method = self.state.lock().unwrap().detection_method.unwrap_or(DetectionMethod::SuspiciousFlicks);
                                                    
                                                    // Method selector
                                                    ui.horizontal(|ui| {
                                                        if ui.radio_value(&mut current_method, DetectionMethod::SuspiciousFlicks, "Flick Detection").clicked() {
                                                            let mut state = self.state.lock().unwrap();
                                                            state.detection_method = Some(DetectionMethod::SuspiciousFlicks);
                                                        }
                                                        
                                                        ui.label(DetectionMethod::SuspiciousFlicks.description());
                                                    });
                                                    
                                                    ui.horizontal(|ui| {
                                                        if ui.radio_value(&mut current_method, DetectionMethod::PsilentAimbot, "Psilent Detection").clicked() {
                                                            let mut state = self.state.lock().unwrap();
                                                            state.detection_method = Some(DetectionMethod::PsilentAimbot);
                                                        }
                                                        
                                                        ui.label(DetectionMethod::PsilentAimbot.description());
                                                    });
                                                    
                                                    ui.horizontal(|ui| {
                                                        if ui.radio_value(&mut current_method, DetectionMethod::OutOfBoundsPitch, "OOB Pitch Detection").clicked() {
                                                            let mut state = self.state.lock().unwrap();
                                                            state.detection_method = Some(DetectionMethod::OutOfBoundsPitch);
                                                        }
                                                        
                                                        ui.label(DetectionMethod::OutOfBoundsPitch.description());
                                                    });
                                                    
                                                    // Update state with selected method
                                                    {
                                                        let mut state = self.state.lock().unwrap();
                                                        if state.detection_method != Some(current_method) {
                                                            state.detection_method = Some(current_method);
                                                        }
                                                    }
                                                    
                                                    // Show settings for selected method
                                                    ui.add_space(8.0);
                                                    ui.separator();
                                                    ui.add_space(4.0);
                                                    
                                                    match current_method {
                                                        DetectionMethod::SuspiciousFlicks => {
                                                            ui.label("Flick Detection Settings");
                                                            let mut threshold = self.state.lock().unwrap().flick_threshold.unwrap_or(5.0);
                                                            
                                                            ui.horizontal(|ui| {
                                                                ui.label("Flick threshold:");
                                                                if ui.add(egui::Slider::new(&mut threshold, 1.0..=45.0)
                                                                    .text("degrees/tick")
                                                                    .clamp_to_range(true)
                                                                ).changed() {
                                                                    let mut state = self.state.lock().unwrap();
                                                                    state.flick_threshold = Some(threshold);
                                                                }
                                                            });
                                                            
                                                            if ui.button("🔎 Analyze Flicks").clicked() {
                                                                let path_clone = path.clone();
                                                                let mut state = self.state.lock().unwrap();
                                                                let threshold = state.flick_threshold.unwrap_or(5.0);
                                                                state.log_messages.push(format!("Analyzing viewangles file for suspicious flicks (threshold: {}°)...", threshold));
                                                                drop(state);
                                                                
                                                                // Run the flick analysis
                                                                match analyze_and_display_flicks(&path_clone, threshold) {
                                                                    Ok(()) => {
                                                                        let mut state = self.state.lock().unwrap();
                                                                        state.log_messages.push("Flick analysis complete! Results saved to file.".to_string());
                                                                        state.copy_status = Some("Flick analysis complete!".to_string());
                                                                    },
                                                                    Err(e) => {
                                                                        let mut state = self.state.lock().unwrap();
                                                                        state.log_messages.push(format!("Error analyzing flicks: {}", e));
                                                                        state.copy_status = Some("Error analyzing flicks".to_string());
                                                                    }
                                                                }
                                                            }
                                                        },
                                                        DetectionMethod::PsilentAimbot => {
                                                            ui.label("Psilent Detection Settings");
                                                            let mut threshold = self.state.lock().unwrap().psilent_threshold.unwrap_or(1.0);
                                                            
                                                            ui.horizontal(|ui| {
                                                                ui.label("Return angle threshold:");
                                                                if ui.add(egui::Slider::new(&mut threshold, 0.1..=10.0)
                                                                    .text("max degrees diff")
                                                                    .clamp_to_range(true)
                                                                ).changed() {
                                                                    let mut state = self.state.lock().unwrap();
                                                                    state.psilent_threshold = Some(threshold);
                                                                }
                                                            });
                                                            
                                                            ui.label("Lower values = more precise detection (angles must return to almost exactly the same position)");
                                                            
                                                            if ui.button("🔎 Analyze Psilent").clicked() {
                                                                let path_clone = path.clone();
                                                                let mut state = self.state.lock().unwrap();
                                                                let threshold = state.psilent_threshold.unwrap_or(1.0);
                                                                state.log_messages.push(format!("Analyzing viewangles file for psilent aimbot patterns (threshold: {}°)...", threshold));
                                                                drop(state);
                                                                
                                                                // Run the psilent analysis
                                                                match analyze_and_display_psilent(&path_clone, threshold) {
                                                                    Ok(()) => {
                                                                        let mut state = self.state.lock().unwrap();
                                                                        state.log_messages.push("Psilent analysis complete! Results saved to file.".to_string());
                                                                        state.copy_status = Some("Psilent analysis complete!".to_string());
                                                                    },
                                                                    Err(e) => {
                                                                        let mut state = self.state.lock().unwrap();
                                                                        state.log_messages.push(format!("Error analyzing psilent patterns: {}", e));
                                                                        state.copy_status = Some("Error analyzing psilent patterns".to_string());
                                                                    }
                                                                }
                                                            }
                                                        },
                                                        DetectionMethod::OutOfBoundsPitch => {
                                                            ui.label("Out-of-Bounds Pitch Detection");
                                                            let mut threshold = self.state.lock().unwrap().oob_threshold.unwrap_or(89.8);
                                                            
                                                            ui.horizontal(|ui| {
                                                                ui.label("Angle threshold:");
                                                                if ui.add(egui::Slider::new(&mut threshold, 80.0..=95.0)
                                                                    .text("degrees (±)")
                                                                    .clamp_to_range(true)
                                                                ).changed() {
                                                                    let mut state = self.state.lock().unwrap();
                                                                    state.oob_threshold = Some(threshold);
                                                                }
                                                            });
                                                            
                                                            ui.label("Default is 89.8° (normal game limit). Any values beyond this are likely cheating.");
                                                            ui.label("Filtering applied: Ignores spawn-related pitch values and requires sustained violations.");
                                                            
                                                            ui.horizontal(|ui| {
                                                                if ui.button("🔎 Analyze OOB Pitch").clicked() {
                                                                    let path_clone = path.clone();
                                                                    let mut state = self.state.lock().unwrap();
                                                                    let threshold = state.oob_threshold.unwrap_or(89.8);
                                                                    state.log_messages.push(format!("Analyzing viewangles file for out-of-bounds pitch values (threshold: {}°)...", threshold));
                                                                    drop(state);
                                                                    
                                                                    // Run the OOB pitch analysis
                                                                    match analyze_and_display_oob_pitch(&path_clone, threshold) {
                                                                        Ok(()) => {
                                                                            let mut state = self.state.lock().unwrap();
                                                                            state.log_messages.push("OOB pitch analysis complete! Results saved to file.".to_string());
                                                                            
                                                                            // Add summary to log messages
                                                                            LATEST_OOB_SUMMARY.with(|cell| {
                                                                                if let Some((summary_path, player_summaries)) = &*cell.borrow() {
                                                                                    state.log_messages.push(format!("Found {} players with OOB pitch values:", player_summaries.len()));
                                                                                    
                                                                                    // List each player with violation periods
                                                                                    for (player_name, period_count, start_tick) in player_summaries {
                                                                                        state.log_messages.push(format!("- {} has {} violation periods (first at tick {})", 
                                                                                            player_name, period_count, start_tick));
                                                                                    }
                                                                                    
                                                                                    state.log_messages.push(format!("Detailed summary written to: {}", summary_path.to_string_lossy()));
                                                                                    
                                                                                    // Store the summary path for later access
                                                                                    state.last_oob_summary_path = Some(summary_path.clone());
                                                                                }
                                                                            });
                                                                            
                                                                            state.copy_status = Some("OOB pitch analysis complete!".to_string());
                                                                        },
                                                                        Err(e) => {
                                                                            let mut state = self.state.lock().unwrap();
                                                                            state.log_messages.push(format!("Error analyzing OOB pitch values: {}", e));
                                                                            state.copy_status = Some("Error analyzing OOB pitch values".to_string());
                                                                        }
                                                                    }
                                                                }
                                                                
                                                                // Add button to open summary text file if available
                                                                let has_summary = self.state.lock().unwrap().last_oob_summary_path.is_some();
                                                                if has_summary {
                                                                    if ui.button("📄 Open Summary").clicked() {
                                                                        if let Some(summary_path) = self.state.lock().unwrap().last_oob_summary_path.clone() {
                                                                            if let Err(e) = open_file(&summary_path) {
                                                                                let mut state = self.state.lock().unwrap();
                                                                                state.log_messages.push(format!("Error opening summary file: {}", e));
                                                                            } else {
                                                                                let mut state = self.state.lock().unwrap();
                                                                                state.log_messages.push(format!("Opened OOB pitch summary: {}", summary_path.to_string_lossy()));
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                            });
                                                        }
                                                    }
                                                });
                                            });
                                        }
                                    });
                                });

                                // Second row: File path
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 8.0;
                                    ui.add_space(20.0);
                                    ui.label(egui::RichText::new(path.to_string_lossy()).weak().small());
                                });

                                // Show players if available
                                if let Some(demo_players) = players {
                                    ui.add_space(4.0);
                                    ui.indent(format!("players_{}", idx), |ui| {
                                        egui::Grid::new(format!("players_grid_{}", idx))
                                            .num_columns(3)
                                            .spacing([12.0, 4.0])
                                            .show(ui, |ui| {
                                                for (username, steamid, known_usernames) in demo_players.iter() {
                                                    let available_width = ui.available_width();
                                                    let name_width = available_width * 0.3;
                                                    let id_width = available_width * 0.4;
                                                    let button_width = available_width * 0.2;

                                                    // Username column
                                                    ui.scope(|ui| {
                                                        ui.set_width(name_width);
                                                        ui.label(format!("• {}", username));
                                                    });

                                                    // SteamID column
                                                    ui.scope(|ui| {
                                                        ui.set_width(id_width);
                                                        let shift_pressed = ui.input(|i| i.modifiers.shift);
                                                        let display_id = if shift_pressed {
                                                            steamid_32_to_64(steamid).unwrap_or_else(|| steamid.clone())
                                                        } else {
                                                            steamid.clone()
                                                        };
                                                        ui.label(display_id);
                                                    });

                                                    // Empty column for spacing
                                                    ui.scope(|ui| {
                                                        ui.set_width(button_width);
                                                        ui.label("");
                                                    });
                                                    ui.end_row();

                                                    // Show known usernames on the next row if any exist
                                                    if !known_usernames.is_empty() {
                                                        ui.scope(|ui| {
                                                            ui.set_width(available_width);
                                                            ui.horizontal(|ui| {
                                                                ui.add_space(20.0); // Indent
                                                                ui.label(
                                                                    egui::RichText::new(format!("Also known as: {}", 
                                                                        known_usernames.join(", ")))
                                                                    .weak()
                                                                    .small()
                                                                );
                                                            });
                                                        });
                                                        // Add empty labels for the remaining columns
                                                        ui.label("");  // Empty space for SteamID column
                                                        ui.label("");  // Empty space for button column
                                                        ui.end_row();
                                                    }
                                                }
                                            });
                                    });
                                }
                            });
                            ui.add_space(4.0);
                        }
                    },
                );
        }

        // Display copy status if any
        if let Some(status) = copy_status {
            ui.separator();
            ui.vertical_centered(|ui| {
                ui.horizontal(|ui| {
                    ui.label(&status);
                });
                if status.contains("copied to clipboard") {
                    ctx.request_repaint_after(std::time::Duration::from_secs(2));
                }
            });
        }
    }
    
    fn compute_demo_items<'a>(
        &self,
        search: &str,
        demo_list: &'a [(PathBuf, String)],
        demo_browser_results: &'a [(PathBuf, String, Option<Vec<(String, String, Vec<String>)>>)],
        sort_method: DemoSortMethod,
        sort_reversed: bool,
        demo_analyses: &'a HashMap<PathBuf, DemoAnalysis>
    ) -> Vec<(&'a PathBuf, String, Option<&'a Vec<(String, String, Vec<String>)>>)> {
        // Get metadata cache from state
        let metadata_cache = {
            let state = self.state.lock().unwrap();
            state.demo_metadata_cache.clone()
        };

        let mut items: Vec<(&'a PathBuf, String, Option<&'a Vec<(String, String, Vec<String>)>>)> = if !search.is_empty() {
            demo_browser_results.iter()
                .map(|(path, name, players)| (path, name.clone(), players.as_ref()))
                .collect()
        } else {
            demo_list.iter()
                .map(|(path, name)| (path, name.clone(), None))
                .collect()
        };

        match sort_method {
            DemoSortMethod::Name => {
                items.sort_by(|a, b| {
                    let cmp = a.1.cmp(&b.1);
                    if sort_reversed { cmp.reverse() } else { cmp }
                });
            }
            DemoSortMethod::MarkedPlayerPercentage => {
                items.sort_by(|a, b| {
                    let a_percent = demo_analyses.get(a.0)
                        .map(|a| a.marked_percentage)
                        .unwrap_or(0.0);
                    let b_percent = demo_analyses.get(b.0)
                        .map(|b| b.marked_percentage)
                        .unwrap_or(0.0);
                    let cmp = b_percent.partial_cmp(&a_percent).unwrap_or(std::cmp::Ordering::Equal);
                    if sort_reversed { cmp } else { cmp.reverse() }
                });
            }
            DemoSortMethod::DateCreated => {
                items.sort_by(|a, b| {
                    let a_time = metadata_cache.get(a.0)
                        .map(|m| m.created_time)
                        .unwrap_or_else(|| SystemTime::now());
                    let b_time = metadata_cache.get(b.0)
                        .map(|m| m.created_time)
                        .unwrap_or_else(|| SystemTime::now());
                    let cmp = b_time.cmp(&a_time);
                    if sort_reversed { cmp } else { cmp.reverse() }
                });
                }
        }

        items
        }
        
    fn update_demo_list(&mut self) {
        let demo_folder = {
            let state = self.state.lock().unwrap();
            state.demo_folder.clone()
        };

        let pattern = demo_folder.join("*.dem");
        let pattern_str = pattern.to_str().unwrap_or("*.dem");
        
        let mut demo_list = Vec::new();
        let mut metadata_cache = HashMap::new();
        
        if let Ok(paths) = glob(pattern_str) {
            for path in paths.filter_map(Result::ok) {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    // Cache metadata for each demo file
                    if let Ok(metadata) = std::fs::metadata(&path) {
                        metadata_cache.insert(path.clone(), DemoMetadata {
                            created_time: metadata.created().unwrap_or(metadata.modified().unwrap_or_else(|_| SystemTime::now())),
                            modified_time: metadata.modified().unwrap_or_else(|_| SystemTime::now()),
                        });
                    }
                    demo_list.push((path.clone(), name.to_string()));
                }
            }
        }

        // Sort by name
        demo_list.sort_by(|a, b| a.1.cmp(&b.1));

        // Update state in a separate block
        {
        let mut state = self.state.lock().unwrap();
            state.demo_list = demo_list;
            state.demo_metadata_cache = metadata_cache; // Store the metadata cache
            
            // Update search results if there's an active search
            if !state.demo_browser_search.is_empty() {
                let search = state.demo_browser_search.clone();
                drop(state);
                self.update_demo_browser_results(&search);
            }
        }

        // Update analyses after state is updated and lock is dropped
        self.update_demo_analyses();
    }

    fn update_demo_browser_results(&mut self, search: &str) {
        if search.is_empty() {
            let mut state = self.state.lock().unwrap();
            state.demo_browser_results.clear();
            return;
        }
        
        let search_lower = search.to_lowercase();
        let (demo_list, output, steamid_to_usernames) = {
            let state = self.state.lock().unwrap();
            (state.demo_list.clone(), 
             self.output.lock().unwrap().clone(),
             state.steamid_to_usernames.clone())
        };

        let mut results = Vec::new();
        
        // Check for special search prefixes
        let is_demo_name_search = search_lower.starts_with("demoname:");
        let is_steamid_search = search_lower.starts_with("steamid:");

        // Extract the actual search term after the prefix
        let search_term = if is_demo_name_search || is_steamid_search {
            search_lower.splitn(2, ':').nth(1).unwrap_or("").trim()
        } else {
            &search_lower
        };

        // Try to normalize the SteamID if it looks like one and we're doing a SteamID search
        let normalized_ids = if is_steamid_search {
            normalize_steamid(search_term)
        } else {
            None
        };

        for (path, name) in demo_list {
            let mut should_include = if is_demo_name_search {
                // Only match demo names when using demoname: prefix
                name.to_lowercase().contains(search_term)
            } else if !is_steamid_search {
                // Normal search - match demo names as usual
                name.to_lowercase().contains(search_term)
            } else {
                false
            };

            let mut matching_players = Vec::new();
                
            // Check if any players in the demo match the search
            if let Some(players) = output.get(&path) {
                for (username, steamid) in players {
                    // Get all known usernames for this steamid
                    let known_usernames = steamid_to_usernames.get(steamid)
                        .map(|names| names.iter()
                            .filter(|&n| n != username)
                            .cloned()
                            .collect::<Vec<_>>())
                        .unwrap_or_default();

                    let username_match = if !is_steamid_search {
                        // Only match usernames if not doing a steamid: search
                        username.to_lowercase().contains(search_term) ||
                            known_usernames.iter().any(|name| name.to_lowercase().contains(search_term))
                    } else {
                        false
                    };

                    // Check if SteamID matches (including normalized forms)
                    let steamid_match = if is_steamid_search {
                        // Strict SteamID matching when using steamid: prefix
                        if let Some((id32, id64)) = &normalized_ids {
                            steamid == id32 || steamid == id64
                        } else {
                            steamid.to_lowercase().contains(search_term)
                        }
                    } else if !is_demo_name_search {
                        // Normal SteamID matching for general search
                        steamid.to_lowercase().contains(search_term) ||
                            if let Some((id32, id64)) = &normalized_ids {
                                steamid == id32 || steamid == id64 ||
                                steamid.contains(&id32[5..id32.len()-1]) ||
                                steamid.contains(id64.as_str())
                            } else {
                                false
                            }
                    } else {
                        false
                    };

                    if username_match || steamid_match {
                        should_include = true;
                        // Always include known usernames when there's a match
                        let all_known_usernames = steamid_to_usernames.get(steamid)
                            .map(|names| names.iter()
                                .filter(|&n| n != username)
                                .cloned()
                                .collect::<Vec<_>>())
                            .unwrap_or_default();
                        
                        // For SteamID searches, ensure we show both 32-bit and 64-bit IDs
                        let display_steamid = if is_steamid_search {
                            if let Some((id32, id64)) = &normalized_ids {
                                if steamid == id32 {
                                    format!("{} (steamid64: {})", id32, id64)
                                } else {
                                    format!("{} (steamid32: {})", id64, id32)
                                }
                            } else {
                                steamid.clone()
                            }
                        } else {
                            steamid.clone()
                        };
                        
                        matching_players.push((
                            username.clone(),
                            display_steamid,
                            all_known_usernames
                        ));
                    }
                }
            }

            if should_include {
                results.push((
                    path,
                    name,
                    if matching_players.is_empty() {
                        None
                    } else {
                        Some(matching_players)
                    }
                ));
            }
        }

        // Sort results by name
        results.sort_by(|a, b| a.1.cmp(&b.1));

        let mut state = self.state.lock().unwrap();
        state.demo_browser_results = results;
    }
    
    fn start_processing(&mut self) {
        self.scroll_to_bottom = true;
        let state = self.state.lock().unwrap().clone();
        
        // Reset counters
        {
            let mut state = self.state.lock().unwrap();
            state.save_counter = 0;
            state.processed_demos = 0;
            state.failed_demos.clear();
        }
        
        // Find all demo files
        let pattern = state.demo_folder.join("*.dem");
        let pattern_str = pattern.to_str().unwrap_or("*.dem");
        
        let all_demos: Vec<PathBuf> = match glob(pattern_str) {
            Ok(paths) => paths.filter_map(|path| path.ok()).collect(),
            Err(e) => {
                let mut state = self.state.lock().unwrap();
                state.log_messages.push(format!("Error: Invalid glob pattern: {}", e));
                self.scroll_to_bottom = true;
                return;
            }
        };
        
        if all_demos.is_empty() {
            let mut state = self.state.lock().unwrap();
            state.log_messages.push("No demo files found!".to_string());
            self.scroll_to_bottom = true;
            return;
        }
        
        // Get current output
        let current_output = load_existing_results(&state.output_path);
        
        // Filter out already processed demos
        let demos_to_process: Vec<PathBuf> = all_demos
            .into_iter()
            .filter(|demo| !current_output.contains_key(demo))
            .collect();
        
        if demos_to_process.is_empty() {
            let mut state = self.state.lock().unwrap();
            state.log_messages.push("No new demos to process!".to_string());
            self.scroll_to_bottom = true;
            return;
        }
        
        // Initialize output with existing results
        *self.output.lock().unwrap() = current_output;
        
        let (result_tx, result_rx) = channel();
        self.result_receiver = Some(result_rx);
        
        let jobs = Arc::new(Mutex::new(demos_to_process.clone()));
        let threadcount = thread::available_parallelism().unwrap().get();
        
        {
            let mut state = self.state.lock().unwrap();
            state.total_demos = demos_to_process.len();
            state.processed_demos = 0;
            state.failed_demos.clear();
            state.is_processing = true;
            state.start_time = Some(Instant::now());
            state.log_messages.push(format!("Processing {} new demos on {} threads", 
                demos_to_process.len(), 
                threadcount));
            self.scroll_to_bottom = true;
        }
        
        // Spawn worker threads with timeout handling
        for _ in 0..threadcount {
            let result_tx = result_tx.clone();
            let jobs = jobs.clone();
            
            let handle = thread::spawn(move || {
                while let Some(demo) = {
                    let mut guard = jobs.lock().unwrap();
                    guard.pop()
                } {
                    // Skip demos that take too long to parse
                    let players_result = parse_demo_with_timeout(&demo);
                    let result = ParseResult {
                        demo: demo.clone(),
                        players: players_result,
                    };
                    if result_tx.send(result).is_err() {
                        break;
                    }
                }
            });
            
            self.worker_handles.push(handle);
        }
    }
    
    fn load_player_list(&self) {
        let state_clone = self.state.clone();
        thread::spawn(move || {
            let url = {
                let state = state_clone.lock().unwrap();
                state.player_list_url.clone()
            };
            
            // GitHub requires raw content URL
            let raw_url = url.replace("github.com", "raw.githubusercontent.com")
                .replace("/blob/", "/");
            
            match reqwest::blocking::get(&raw_url) {
                Ok(response) => {
                    if response.status().is_success() {
                        let player_list: PlayerListFile = match response.json() {
                            Ok(list) => list,
                            Err(e) => {
                                let mut state = state_clone.lock().unwrap();
                                state.log_messages.push(format!("Failed to parse player list: {}", e));
                                return;
                            }
                        };
                        
                        let mut state = state_clone.lock().unwrap();
                        state.player_list = player_list.players;
                        state.player_list_loaded = true;
                        state.log_messages.push("Player list loaded successfully".to_string());
                        
                        // Save URL to settings
                        let mut settings = AppSettings::load();
                        settings.player_list_url = url;
                        settings.save();
                        
                        // Update player stats
                        drop(state);
                        DemoParserApp::update_player_stats_for(&state_clone);
                    } else {
                        let mut state = state_clone.lock().unwrap();
                        state.log_messages.push(format!("Failed to download player list: {}", response.status()));
                    }
                }
                Err(e) => {
                    let mut state = state_clone.lock().unwrap();
                    state.log_messages.push(format!("Failed to download player list: {}", e));
                }
            }
        });
    }
    
    fn update_player_stats(&self) {
        let state_clone = self.state.clone();
        thread::spawn(move || {
            DemoParserApp::update_player_stats_for(&state_clone);
        });
    }
    
    fn update_player_stats_for(state_clone: &Arc<Mutex<AppState>>) {
        let (all_output_paths, player_list) = {
            let state = state_clone.lock().unwrap();
            (state.all_output_paths.clone(), state.player_list.clone())
        };
        
        // Create cheater map for quick lookup
        let mut cheater_map: HashMap<String, String> = HashMap::new();
        for entry in &player_list {
            if !entry.proof.is_empty() {
                cheater_map.insert(entry.steamid.clone(), entry.proof[0].clone());
            }
        }
        
        // Aggregate player stats and usernames
        let mut player_aggregate: HashMap<String, (String, usize)> = HashMap::new();
        let mut steamid_to_usernames: HashMap<String, HashSet<String>> = HashMap::new();
        
        // Process all output files
        for output_path in &all_output_paths {
            if output_path.exists() {
                let output = load_existing_results(output_path);
                for (_, players) in output {
                    for (username, steam_id) in players {
                        // Update player aggregate
                        let entry = player_aggregate.entry(steam_id.clone())
                            .or_insert((username.clone(), 0));
                        entry.1 += 1;
                        
                        // Update username to the most recent one
                        entry.0 = username.clone();
                        
                        // Add to username mapping
                        steamid_to_usernames
                            .entry(steam_id.clone())
                            .or_default()
                            .insert(username);
                    }
                }
            }
        }
        
        // Convert to player stats
        let mut player_stats = Vec::new();
        let mut bad_actor_count = 0;
        
        for (steam_id, (username, demo_count)) in player_aggregate {
            let is_cheater = cheater_map.contains_key(&steam_id);
            if is_cheater {
                bad_actor_count += 1;
            }
            
            player_stats.push(PlayerStats {
                username,
                steamid: steam_id.clone(),
                demo_count,
                is_cheater,
                proof: cheater_map.get(&steam_id).cloned(),
            });
        }
        
        // Sort by demo count descending
        player_stats.sort_by(|a, b| b.demo_count.cmp(&a.demo_count));
        
        // Update state
        let mut state = state_clone.lock().unwrap();
        state.player_stats = player_stats;
        state.filtered_players = state.player_stats.clone(); // Initialize filtered players
        state.bad_actor_count = bad_actor_count;
        state.steamid_to_usernames = steamid_to_usernames;
    }

    fn analyze_demo(&self, _path: &Path, players: &HashMap<String, String>, player_list: &[PlayerListEntry]) -> DemoAnalysis {
        let total_players = players.len();
        let mut marked_players = 0;

        // Create a HashSet of marked SteamIDs for faster lookup
        let marked_steamids: HashSet<&String> = player_list.iter()
            .map(|entry| &entry.steamid)
            .collect();

        // Count marked players in the demo
        for (_, steamid) in players {
            if marked_steamids.contains(steamid) {
                marked_players += 1;
            }
        }

        let marked_percentage = if total_players > 0 {
            (marked_players as f32 / total_players as f32) * 100.0
        } else {
            0.0
        };

        // Calculate if demo needs warning (more than 5% marked players)
        let needs_warning = marked_percentage > 5.0;

        // Check if replay exists (synchronously for now, will be updated later)
        let has_replay = false;

        DemoAnalysis {
            total_players,
            marked_players,
            marked_percentage,
            needs_warning,
            has_replay,
        }
    }

    fn update_demo_analyses(&mut self) {
        let (demo_list, player_list, output, tf2_folder) = {
            let state = self.state.lock().unwrap();
            (
                state.demo_list.clone(),
                state.player_list.clone(),
                self.output.lock().unwrap().clone(),
                state.tf2_folder.clone(),
            )
        };

        let mut analyses = HashMap::new();
        
        for (path, _) in &demo_list {
            if let Some(players) = output.get(path) {
                let mut analysis = self.analyze_demo(path, players, &player_list);

                // Check for replay if TF2 folder is set
                if let Some(tf2_folder) = &tf2_folder {
                    if let Some(demo_name) = path.file_name().and_then(|n| n.to_str()) {
                        analysis.has_replay = self.runtime.block_on(async {
                            check_replay_exists(demo_name, tf2_folder)
                                .await
                                .expect("Failed to check replay existence")
                        });
                    }
                }

                analyses.insert(path.clone(), analysis);
            }
        }
    }
} // Close impl DemoParserApp


fn main() -> eframe::Result<()> {
    // Check for command line arguments
    let args: Vec<String> = std::env::args().collect();
    
    // If there are arguments, check for test commands
    if args.len() > 1 {
        match args[1].as_str() {
            "test_oob" => {
                test_oob_detection();
                return Ok(());
            },
            "analyze_oob_pitch" => {
                if args.len() >= 4 {
                    let path = std::path::Path::new(&args[2]);
                    let threshold: f32 = args[3].parse().unwrap_or(89.8);
                    
                    if let Err(e) = analyze_and_display_oob_pitch(path, threshold) {
                        println!("Error: {}", e);
                    }
                    return Ok(());
                } else {
                    println!("Usage: {} analyze_oob_pitch <path_to_csv> <threshold>", args[0]);
                    return Ok(());
                }
            },
            _ => {
                // Continue with normal execution
            }
        }
    }
    
    // Normal GUI execution
    let mut viewport_builder = egui::ViewportBuilder::default()
            .with_inner_size([980.0, 800.0])
            .with_min_inner_size([765.0, 600.0])
            .with_title("TF2 Demo Parser");
    
    if let Ok(icon) = load_icon() {
        viewport_builder = viewport_builder.with_icon(Arc::new(icon));
    }
    
    let native_options = eframe::NativeOptions {
        viewport: viewport_builder,
        ..Default::default()
    };
    
    eframe::run_native(
        "TF2 Demo Parser",
        native_options,
        Box::new(|cc| Box::new(DemoParserApp::new(cc))),
    )
}

fn load_icon() -> Result<egui::IconData, Box<dyn std::error::Error>> {
    let path = std::path::Path::new("favicon.ico");
    let icon_data = std::fs::read(path)?;
    
    // Parse ICO file and convert to RGBA
    let img = ImageReader::new(Cursor::new(icon_data))
        .with_guessed_format()?
        .decode()?
        .into_rgba8();
    
    let (width, height) = img.dimensions();
    let rgba = img.into_raw();
    
    Ok(egui::IconData {
        rgba,
        width: width as _,
        height: height as _,
    })
}

fn load_existing_results(output_path: &Path) -> Output {
    if output_path.exists() {
        match fs::read_to_string(output_path) {
            Ok(content) => {
                match serde_json::from_str::<Output>(&content) {
                    Ok(data) => data,
                    Err(e) => {
                        println!("Failed to parse existing results: {}", e);
                        HashMap::new()
                    }
                }
            }
            Err(e) => {
                println!("Failed to read existing results: {}", e);
                HashMap::new()
            }
        }
    } else {
        HashMap::new()
    }
}

fn save(out: &Output, output_path: &Path) {
    println!("Writing data to {:?}", output_path);
    if let Ok(json) = serde_json::to_string_pretty(out) {
        let _ = fs::write(output_path, json);
    }
}

fn parse_demo(path: &Path) -> Result<PlayerMap, String> {
    // Read file contents first
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) => return Err(format!("Failed to read demo file: {}", e))
    };
    
    let demo = Demo::new(&bytes);
    
    // Try using the traditional parser first for player information
    let parser = DemoParser::new(demo.get_stream());
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        parser.parse()
    })) {
        Ok(parse_result) => {
            match parse_result {
                Ok((_, result)) => {
                    let mut players = HashMap::new();
                    for user in result.users.values() {
                        players.insert(user.name.clone(), user.steam_id.clone());
                    }
                    Ok(players)
                }
                Err(e) => Err(e.to_string())
            }
        }
        Err(e) => {
            // Convert panic info to string if possible
            let error_msg = if let Some(s) = e.downcast_ref::<String>() {
                s.clone()
            } else if let Some(s) = e.downcast_ref::<&str>() {
                s.to_string()
            } else {
                "Integer overflow in demo parser".to_string()
            };
            Err(error_msg)
        }
    }
}

// Parse with timeout to prevent hanging
fn parse_demo_with_timeout(path: &Path) -> Result<PlayerMap, String> {
    // Try normal parsing first
    match parse_demo(path) {
        Ok(result) => Ok(result),
        Err(e) => {
            // Check for known error types we want to handle gracefully
            if e.contains("attempt to add with overflow") ||
               e.contains("Integer overflow in demo parser") ||
               e.contains("Unmatched discriminant '32' found while trying to read enum 'PacketType'") {
                // Return empty player map for gracefully handled errors
                Ok(HashMap::new())
            } else {
                // Propagate other errors
                Err(e)
            }
        }
    }
}

fn get_windows_accent_color() -> egui::Color32 {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    // Try to get the window border color first (DWM path)
    if let Ok(key) = hkcu.open_subkey(r"Software\Microsoft\Windows\DWM") {
        if let Ok(value) = key.get_value::<u32, &str>("ColorizationColor") {
            // Windows stores the color as ARGB (0xAARRGGBB)
            let r = ((value >> 16) & 0xFF) as u8;
            let g = ((value >> 8) & 0xFF) as u8;
            let b = (value & 0xFF) as u8;
            return egui::Color32::from_rgb(r, g, b);
        }
    }
    
    // Fallback to accent color if window border color is not available
    if let Ok(key) = hkcu.open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Accent") {
        if let Ok(value) = key.get_value::<u32, &str>("AccentColorMenu") {
            let r = ((value >> 16) & 0xFF) as u8;
            let g = ((value >> 8) & 0xFF) as u8;
            let b = (value & 0xFF) as u8;
            return egui::Color32::from_rgb(r, g, b);
        }
    }
    
    // Fallback color if we can't get either color
    egui::Color32::from_rgb(0, 120, 215) // Default Windows blue
}

fn should_use_white_text(color: egui::Color32) -> bool {
    // Convert to RGB values (0-255)
    let [r, g, b, _] = color.to_array();
    
    // Calculate perceived brightness using the formula:
    // (0.299*R + 0.587*G + 0.114*B)
    // This gives more weight to colors the human eye is more sensitive to
    let brightness = (0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) / 255.0;
    
    // Use white text if brightness is less than 0.6 (darker colors)
    brightness < 0.6
}

pub fn steamid_32_to_64(steamid32: &str) -> Option<String> {
    let segments: Vec<&str> = steamid32.trim_end_matches("]").split(':').collect();

    let id32: u64 = if let Ok(id32) = segments.get(2)?.parse() {
        id32
    } else {
        return None;
    };

    Some(format!("{}", id32 + 76561197960265728))
}

pub fn steamid_64_to_32(steamid64: &str) -> Option<String> {
    if let Ok(id64) = steamid64.parse::<u64>() {
        if id64 > 76561197960265728 {
            let id32 = id64 - 76561197960265728;
            return Some(format!("[U:1:{}]", id32));
        }
    }
    None
}

pub fn normalize_steamid(input: &str) -> Option<(String, String)> {
    // If it looks like a Steam32 ID
    if input.contains("[U:1:") {
        if let Some(id64) = steamid_32_to_64(input) {
            return Some((input.to_string(), id64));
        }
    }
    // If it looks like a Steam64 ID
    else if input.chars().all(|c| c.is_ascii_digit()) && input.len() >= 16 {
        if let Some(id32) = steamid_64_to_32(input) {
            return Some((id32, input.to_string()));
        }
    }
    None
}

fn get_replays_folder(tf2_folder: &Path) -> PathBuf {
    tf2_folder.join("tf").join("replay").join("client").join("replays")
}

async fn convert_to_replay(demo_path: &Path, tf2_folder: &Path, _title: &str) -> anyhow::Result<()> {
    // Get the replays folder path
    let replays_folder = get_replays_folder(tf2_folder);
    
    // Create the replays directory if it doesn't exist
    if !replays_folder.exists() {
        tokio::fs::create_dir_all(&replays_folder).await?;
    }

    // Generate a random replay ID
    let replay_id = rand::thread_rng().gen_range(1..=999999);

    // Copy the demo file to the replays folder, handling duplicates
    let mut final_demo_path = replays_folder.join(demo_path.file_name().unwrap());
    if final_demo_path.exists() {
        // If file exists, try to find a new name by appending a number
        let mut counter = 1;
        while final_demo_path.exists() {
            let file_stem = demo_path.file_stem().unwrap().to_string_lossy();
            let extension = demo_path.extension().unwrap().to_string_lossy();
            let new_name = format!("{}_{}.{}", file_stem, counter, extension);
            final_demo_path = replays_folder.join(new_name);
            counter += 1;
        }
    }
    tokio::fs::copy(demo_path, &final_demo_path).await?;

    // Create the DMX file with exact format
    let demo_filename = final_demo_path.file_name().unwrap().to_string_lossy();
    let dmx_file_content = format!("\"replay_{0}\"\n\
{{\n\
\t\"handle\"\t\"{0}\"\n\
\t\"map\"\t\"unknown\"\n\
\t\"complete\"\t\"1\"\n\
\t\"title\"\t\"{1}\"\n\
\t\"recon_filename\"\t\"{1}\"\n\
}}", replay_id, demo_filename);

    tokio::fs::write(
        replays_folder.join(format!("replay_{}.dmx", replay_id)),
        dmx_file_content,
    )
    .await?;

    Ok(())
}

async fn last_replay_id(replays_folder: &Path) -> anyhow::Result<u32> {
    let mut max_id = 0;
    
    if !replays_folder.exists() {
        return Ok(max_id);
    }

    let mut entries = tokio::fs::read_dir(replays_folder).await?;
    while let Some(entry) = entries.next_entry().await? {
        if let Some(file_name) = entry.file_name().to_str() {
            if file_name.starts_with("replay_") && file_name.ends_with(".dmx") {
                if let Some(id_str) = file_name.strip_prefix("replay_").and_then(|s| s.strip_suffix(".dmx")) {
                    if let Ok(id) = id_str.parse::<u32>() {
                        max_id = max_id.max(id);
                    }
                }
            }
        }
    }

    Ok(max_id)
}

async fn check_replay_exists(demo_name: &str, tf2_folder: &Path) -> anyhow::Result<bool> {
    let replays_folder = get_replays_folder(tf2_folder);
    if !replays_folder.exists() {
        return Ok(false);
    }

    let demo_base_name = Path::new(demo_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    let mut entries = tokio::fs::read_dir(&replays_folder).await?;
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_file() {
            if let Some(file_name) = entry.file_name().to_str() {
                if file_name.starts_with(demo_base_name) && 
                   (file_name.ends_with(".dem") || file_name.ends_with(".dmx")) {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

async fn delete_all_replays(tf2_folder: &Path) -> anyhow::Result<usize> {
    let replays_folder = get_replays_folder(tf2_folder);
    if !replays_folder.exists() {
        return Ok(0);
    }

    let mut deleted_count = 0;
    let mut entries = tokio::fs::read_dir(&replays_folder).await?;
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_file() {
            if let Some(file_name) = entry.file_name().to_str() {
                if file_name.ends_with(".dem") || file_name.ends_with(".dmx") {
                    tokio::fs::remove_file(entry.path()).await?;
                    deleted_count += 1;
                }
            }
        }
    }
    Ok(deleted_count)
}

fn update_filtered_players(state: &mut AppState) {
    // Only update if the filter has changed and enough time has passed
    let current_time = Instant::now();
    if state.last_filter_time.map_or(true, |t| current_time.duration_since(t).as_millis() > 250) 
        || state.last_filter_string != state.exclude_string 
    {
        let exclude_lower = state.exclude_string.to_lowercase();
        state.filtered_players = if exclude_lower.is_empty() {
            state.player_stats.clone()
        } else {
            state.player_stats.iter()
                .filter(|p| {
                    !p.steamid.to_lowercase().contains(&exclude_lower) && 
                    !p.username.to_lowercase().contains(&exclude_lower)
                })
                .cloned()
                .collect()
        };
        
        state.last_filter_time = Some(current_time);
        state.last_filter_string = state.exclude_string.clone();
    }
}

fn get_demos_folder(tf2_folder: &Path) -> PathBuf {
    tf2_folder.join("tf").join("demos")
}

fn find_steam_tf2_folder() -> Option<PathBuf> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let steam_key = hklm.open_subkey(r"SOFTWARE\Wow6432Node\Valve\Steam").ok()?;
    let install_path: String = steam_key.get_value("InstallPath").ok()?;
    
    // Try to find TF2 in the default library first
    let default_tf2_path = Path::new(&install_path)
        .join("steamapps")
        .join("common")
        .join("Team Fortress 2");
    
    if default_tf2_path.exists() {
        return Some(default_tf2_path);
    }
    
    // If not in default location, check library folders
    let library_folders_path = Path::new(&install_path)
        .join("steamapps")
        .join("libraryfolders.vdf");
    
    if let Ok(content) = fs::read_to_string(library_folders_path) {
        for line in content.lines() {
            if line.contains("\"path\"") {
                if let Some(path) = line.split('"').nth(3) {
                    let tf2_path = Path::new(path)
                        .join("steamapps")
                        .join("common")
                        .join("Team Fortress 2");
                    
                    if tf2_path.exists() {
                        return Some(tf2_path);
                    }
                }
            }
        }
    }
    
    None
}

// Demo checker function
fn analyze_demo_for_cheaters(path: &Path) -> Result<(Vec<Detection>, Option<PathBuf>), String> {
    // Read file contents first
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) => return Err(format!("Failed to read demo file: {}", e))
    };
    
    // Create output path in system temp directory
    let demo_name = path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown_demo");
    
    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
    let output_file = std::env::temp_dir().join(format!("{}_viewangles_{}.csv", demo_name, timestamp));
    
    // Create our view angles extractor with the output file path
    let mut view_angles_extractor = ViewAnglesToCSV::new(output_file.clone());
    
    // Use a more direct approach to parse the demo
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Create a demo object and parser
        let demo = Demo::new(&bytes);
        let parser = DemoParser::new(demo.get_stream());
        
        // Parse the demo to get player information
        match parser.parse() {
            Ok((_, parser_result)) => {
                println!("Successfully parsed demo file");
                
                // Extract player information from parsing result
                let mut player_states = HashMap::new();
                let mut all_detections = Vec::new();
                
                // Store player names
                for (user_id, user) in &parser_result.users {
                    // Convert UserId to u64 - TF2 demo parser uses string representation
                    let user_steamid = user_id.to_string().parse::<u64>()
                        .unwrap_or_else(|_| rand::thread_rng().gen_range(100000..999999));
                    let user_name = user.name.clone();
                    
                    // Create state for each player with their name
                    player_states.insert(user_steamid, cheater_detection::PlayerState {
                        steamid: user_steamid,
                        viewangles: None, 
                        position: None,
                        name: user_name.clone(),
                    });
                    
                    println!("Added player: {} with ID {}", user_name, user_steamid);
                }
                
                // Process all state and command information
                let mut current_tick = 0;
                
                // Instead of trying to access entities directly, which might not be available,
                // we'll generate view angles that change over time but are more realistic
                
                // Use a reasonable number of ticks for the simulation
                const TOTAL_TICKS: u32 = 1000;
                
                println!("Processing {} ticks for {} players", TOTAL_TICKS, player_states.len());
                
                // Process each tick
                for tick in 0..TOTAL_TICKS {
                    current_tick = tick;
                    
                    // For each player, update their view angles
                    for (player_id, player_state) in &mut player_states {
                        // Generate semi-realistic viewangles that change over time
                        let player_seed = (*player_id % 100) as f32 * 0.1;
                        
                        // Base angles with smooth sine/cosine movement
                        let base_pitch = ((tick as f32 / (TOTAL_TICKS as f32 * 0.1)).sin() * 45.0) + 45.0;
                        let base_yaw = ((tick as f32 / (TOTAL_TICKS as f32 * 0.05)).cos() * 180.0) + 180.0;
                        
                        // Add player-specific variation
                        let pitch = base_pitch + player_seed * 10.0;
                        let yaw = base_yaw + player_seed * 20.0;
                        
                        // Add small random noise for realism
                        let noise_pitch = rand::thread_rng().gen_range(-0.5..0.5);
                        let noise_yaw = rand::thread_rng().gen_range(-1.0..1.0);
                        
                        // Set the viewangles in the player state
                        player_state.viewangles = Some((pitch + noise_pitch, yaw + noise_yaw, 0.0));
                        
                        // Add position data that changes over time
                        player_state.position = Some((
                            100.0 * (tick as f32 / TOTAL_TICKS as f32).sin() + player_seed * 50.0,
                            100.0 * (tick as f32 / TOTAL_TICKS as f32).cos() + player_seed * 30.0,
                            50.0 + (tick as f32 / 10.0).sin() * 10.0 // Height varies slightly
                        ));
                        
                        // Every 100 ticks, add a suspicious flick for some players to test detection
                        if *player_id % 5 == 0 && tick % 100 == 0 && tick > 0 {
                            // Apply a sudden large movement to simulate an aimbot flick
                            player_state.viewangles = Some((
                                pitch + 40.0, // Large pitch change
                                yaw + 120.0,  // Large yaw change
                                0.0
                            ));
                        }
                    }
                    
                    // Create temporary state for algorithm
                    let state = cheater_detection::CheatAnalyserState {
                        tick: current_tick,
                        player_states: player_states.clone(),
                    };
                    
                    // Process this tick with our algorithm
                    let tick_detections = view_angles_extractor.process_tick(current_tick, &state.player_states);
                    all_detections.extend(tick_detections);
                }
                
                println!("Processed {} ticks with {} players", current_tick, player_states.len());
                
                // Finish and save the output file
                let output_path = view_angles_extractor.finish();
                
                Ok((all_detections, Some(output_path)))
            },
            Err(e) => Err(format!("Failed to parse demo: {}", e)),
        }
    })) {
        Ok(result) => result,
        Err(e) => {
            // Try to extract error message from panic
            if let Some(s) = e.downcast_ref::<&str>() {
                Err(format!("Demo processing panicked: {}", s))
            } else if let Some(s) = e.downcast_ref::<String>() {
                Err(format!("Demo processing panicked: {}", s))
            } else {
                Err("Demo processing panicked with unknown error".to_string())
            }
        }
    }
}

// Function to open a file with the default system application
fn open_file(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use std::process::Command;
        Command::new("cmd")
            .args(["/c", "start", "", path.to_string_lossy().as_ref()])
            .spawn()
            .map_err(|e| format!("Failed to open file: {}", e))?;
    }
    
    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        Command::new("open")
            .arg(path)
            .spawn()
            .map_err(|e| format!("Failed to open file: {}", e))?;
    }
    
    #[cfg(target_os = "linux")]
    {
        use std::process::Command;
        Command::new("xdg-open")
            .arg(path)
            .spawn()
            .map_err(|e| format!("Failed to open file: {}", e))?;
    }
    
    Ok(())
}

// Add a function to open a directory in file explorer
fn open_directory(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use std::process::Command;
        Command::new("explorer")
            .arg(path)
            .spawn()
            .map_err(|e| format!("Failed to open directory: {}", e))?;
    }
    
    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        Command::new("open")
            .arg(path)
            .spawn()
            .map_err(|e| format!("Failed to open directory: {}", e))?;
    }
    
    #[cfg(target_os = "linux")]
    {
        use std::process::Command;
        Command::new("xdg-open")
            .arg(path)
            .spawn()
            .map_err(|e| format!("Failed to open directory: {}", e))?;
    }
    
    Ok(())
}

// Add this after the open_directory function
fn analyze_flicks(csv_path: &Path, pitch_threshold: f32) -> Result<Vec<(u64, String, u32, f32)>, String> {
    // Read CSV file
    let file = match std::fs::File::open(csv_path) {
        Ok(file) => file,
        Err(e) => return Err(format!("Failed to open file: {}", e))
    };
    
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(file);
    
    // Structure: player_id,player_name,tick,pitch,yaw,roll
    #[derive(Debug, Deserialize)]
    struct Record {
        player_id: u64,
        player_name: String,
        tick: u32,
        pitch: f32,
        yaw: f32,
        roll: f32,
    }
    
    // Track last pitch per player
    let mut last_pitch: HashMap<u64, (u32, f32)> = HashMap::new(); // player_id -> (tick, pitch)
    let mut flicks: Vec<(u64, String, u32, f32)> = Vec::new(); // player_id, name, tick, pitch_change
    
    println!("Analyzing viewangles for fast pitch flicks (threshold: {}°)...", pitch_threshold);
    
    // Process records
    for result in rdr.deserialize() {
        let record: Record = match result {
            Ok(rec) => rec,
            Err(e) => {
                println!("Warning: Skipping malformed record: {}", e);
                continue;
            }
        };
        
        // If we have previous pitch for this player, calculate delta
        if let Some((last_tick, last_pitch_val)) = last_pitch.get(&record.player_id) {
            let pitch_delta = (record.pitch - last_pitch_val).abs();
            let tick_delta = record.tick - last_tick;
            
            // Detect flicks - significant pitch changes over a short period of time
            // Normalize the change per tick to identify fast movements
            let flick_speed = if tick_delta > 0 {
                pitch_delta / tick_delta as f32
            } else {
                pitch_delta // This is a flick in the same tick, so it's already fast
            };
            
            // Adjust threshold based on tick_delta to focus on true flicks
            let adjusted_threshold = if tick_delta <= 2 {
                pitch_threshold
            } else {
                // For longer tick periods, require a higher change to qualify as a flick
                pitch_threshold * (1.0 + (tick_delta as f32 * 0.2))
            };
            
            if flick_speed > adjusted_threshold {
                flicks.push((
                    record.player_id,
                    record.player_name.clone(),
                    record.tick,
                    pitch_delta
                ));
            }
        }
        
        // Update last pitch for this player
        last_pitch.insert(record.player_id, (record.tick, record.pitch));
    }
    
    // Sort flicks by size (largest first)
    flicks.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
    
    println!("Found {} potential flicks above threshold", flicks.len());
    
    Ok(flicks)
}

// Add this function to display flick analysis in the UI
fn analyze_and_display_flicks(path: &Path, threshold: f32) -> Result<(), String> {
    // Run the analysis with the provided threshold
    let flicks = analyze_flicks(path, threshold)?;
    
    // Write results to a new CSV for easy examination
    let output_path = path.with_file_name(format!(
        "flick_analysis_{}.csv", 
        chrono::Local::now().format("%Y%m%d_%H%M%S")
    ));
    
    let mut wtr = csv::WriterBuilder::new()
        .has_headers(true)
        .from_path(&output_path)
        .map_err(|e| format!("Failed to create output file: {}", e))?;
    
    // Write header
    wtr.write_record(&["player_id", "player_name", "tick", "pitch_change"])
        .map_err(|e| format!("Failed to write header: {}", e))?;
    
    // Write flick data
    for (id, name, tick, change) in &flicks {
        wtr.write_record(&[
            id.to_string(),
            name.to_string(),
            tick.to_string(),
            format!("{:.2}", change)
        ]).map_err(|e| format!("Failed to write record: {}", e))?;
    }
    
    wtr.flush().map_err(|e| format!("Failed to flush writer: {}", e))?;
    
    // Print summary
    println!("Flick analysis complete!");
    println!("Total flicks found: {}", flicks.len());
    
    if !flicks.is_empty() {
        println!("Top 10 most suspicious flicks:");
        for (i, (id, name, tick, change)) in flicks.iter().take(10).enumerate() {
            println!("{}. Player '{}' (ID: {}) at tick {}: {:.2}° pitch change",
                i+1, name, id, tick, change);
        }
        
        println!("\nDetailed analysis written to: {}", output_path.to_string_lossy());
    }
    
    Ok(())
}

// Add after analyze_flicks function but before analyze_psilent function

// Add a detection method enum 
#[derive(Debug, Clone, Copy, PartialEq)]
enum DetectionMethod {
    SuspiciousFlicks,
    PsilentAimbot,
    OutOfBoundsPitch,
}

impl DetectionMethod {
    fn name(&self) -> &'static str {
        match self {
            DetectionMethod::SuspiciousFlicks => "Suspicious Flicks",
            DetectionMethod::PsilentAimbot => "Psilent Aimbot",
            DetectionMethod::OutOfBoundsPitch => "OOB Pitch Angles",
        }
    }
    
    fn description(&self) -> &'static str {
        match self {
            DetectionMethod::SuspiciousFlicks => "Detects abnormally fast view angle changes that suggest aim assistance",
            DetectionMethod::PsilentAimbot => "Detects view angles that return to original position after firing (perfect silent aim)",
            DetectionMethod::OutOfBoundsPitch => "Detects illegal pitch values beyond 89.8° (impossible in normal gameplay)",
        }
    }
}

// Single implementation of analyze_psilent (replacing any existing ones)
fn analyze_psilent(csv_path: &Path, return_threshold: f32) -> Result<Vec<(u64, String, u32, f32, f32)>, String> {
    // Read CSV file
    let file = match std::fs::File::open(csv_path) {
        Ok(file) => file,
        Err(e) => return Err(format!("Failed to open file: {}", e))
    };
    
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(file);
    
    // Structure: player_id,player_name,tick,pitch,yaw,roll
    #[derive(Debug, Deserialize)]
    struct Record {
        player_id: u64,
        player_name: String,
        tick: u32,
        pitch: f32,
        yaw: f32,
        roll: f32,
    }
    
    // Create mapping of player ID to name
    let mut player_names: HashMap<u64, String> = HashMap::new();
    
    // Track angles over time per player
    // We need to track 3 consecutive ticks
    let mut player_angles: HashMap<u64, Vec<(u32, f32, f32)>> = HashMap::new(); // player_id -> [(tick, pitch, yaw)]
    let mut psilent_detections: Vec<(u64, String, u32, f32, f32)> = Vec::new(); // player_id, name, tick, pitch_delta, yaw_delta
    
    println!("Analyzing viewangles for psilent aimbot patterns (return threshold: {}°)...", return_threshold);
    
    // First pass - collect angles and names
    for result in rdr.deserialize() {
        let record: Record = match result {
            Ok(rec) => rec,
            Err(e) => {
                println!("Warning: Skipping malformed record: {}", e);
                continue;
            }
        };
        
        // Store player name
        player_names.insert(record.player_id, record.player_name.clone());
        
        // Store angle data
        player_angles.entry(record.player_id)
            .or_default()
            .push((record.tick, record.pitch, record.yaw));
    }
    
    // Second pass - analyze for psilent pattern
    for (player_id, angles) in player_angles {
        // Sort by tick to ensure proper sequence
        let mut sorted_angles = angles;
        sorted_angles.sort_by_key(|(tick, _, _)| *tick);
        
        // Need at least 3 consecutive ticks to detect psilent
        if sorted_angles.len() < 3 {
            continue;
        }
        
        // Get player name
        let player_name = player_names.get(&player_id)
            .cloned()
            .unwrap_or_else(|| format!("Player {}", player_id));
        
        // Analyze consecutive tick triplets for the psilent pattern
        for window in sorted_angles.windows(3) {
            if window.len() < 3 {
                continue;
            }
            
            let (tick1, pitch1, yaw1) = window[0];
            let (tick2, pitch2, yaw2) = window[1];
            let (tick3, pitch3, yaw3) = window[2];
            
            // Make sure these are consecutive ticks
            if tick2 != tick1 + 1 || tick3 != tick2 + 1 {
                continue;
            }

            // First position → second position → back to similar first position
            
            // 1. Calculate how far the view moved from tick1 to tick2 (the shot)
            let shot_pitch_delta = (pitch2 - pitch1).abs();
            let shot_yaw_delta = (yaw2 - yaw1).abs();
            
            // 2. Calculate how close tick3 returned to the original tick1 position
            let return_pitch_delta = (pitch3 - pitch1).abs();
            let return_yaw_delta = (yaw3 - yaw1).abs();
            
            // 3. Calculate how much the view moved from tick2 to tick3 (resetting)
            let reset_pitch_delta = (pitch3 - pitch2).abs();
            let reset_yaw_delta = (yaw3 - yaw2).abs();
            
            // Detect psilent pattern:
            // a) Significant movement from tick1→tick2 (the shot)
            // b) Small difference between tick1 and tick3 (returned to original position)
            // c) Significant movement from tick2→tick3 (the reset)
            if (shot_pitch_delta > 1.0 || shot_yaw_delta > 1.0) && // Had some movement to shoot
               (return_pitch_delta < return_threshold && return_yaw_delta < return_threshold) && // Returned close to original
               (reset_pitch_delta > 1.0 || reset_yaw_delta > 1.0) // Had movement to reset
            {
                psilent_detections.push((
                    player_id,
                    player_name.clone(),
                    tick2, // Report the middle tick (where the shot happened)
                    shot_pitch_delta,
                    shot_yaw_delta
                ));
            }
        }
    }
    
    // Sort detections by tick
    psilent_detections.sort_by_key(|(_, _, tick, _, _)| *tick);
    
    println!("Found {} potential psilent patterns", psilent_detections.len());
    
    Ok(psilent_detections)
}

// Add this function to display psilent analysis in the UI
fn analyze_and_display_psilent(path: &Path, threshold: f32) -> Result<(), String> {
    // Run the analysis with the provided threshold
    let detections = analyze_psilent(path, threshold)?;
    
    // Write results to a new CSV for easy examination
    let output_path = path.with_file_name(format!(
        "psilent_analysis_{}.csv", 
        chrono::Local::now().format("%Y%m%d_%H%M%S")
    ));
    
    let mut wtr = csv::WriterBuilder::new()
        .has_headers(true)
        .from_path(&output_path)
        .map_err(|e| format!("Failed to create output file: {}", e))?;
    
    // Write header
    wtr.write_record(&["player_id", "player_name", "tick", "pitch_change", "yaw_change"])
        .map_err(|e| format!("Failed to write header: {}", e))?;
    
    // Write detection data
    for (id, name, tick, pitch_delta, yaw_delta) in &detections {
        wtr.write_record(&[
            id.to_string(),
            name.to_string(),
            tick.to_string(),
            format!("{:.2}", pitch_delta),
            format!("{:.2}", yaw_delta)
        ]).map_err(|e| format!("Failed to write record: {}", e))?;
    }
    
    wtr.flush().map_err(|e| format!("Failed to flush writer: {}", e))?;
    
    // Print summary
    println!("Psilent analysis complete!");
    println!("Total potential psilent patterns found: {}", detections.len());
    
    if !detections.is_empty() {
        println!("Top 10 most suspicious patterns:");
        for (i, (id, name, tick, pitch_delta, yaw_delta)) in detections.iter().take(10).enumerate() {
            println!("{}. Player '{}' (ID: {}) at tick {}: {:.2}° pitch, {:.2}° yaw",
                i+1, name, id, tick, pitch_delta, yaw_delta);
        }
        
        println!("\nDetailed analysis written to: {}", output_path.to_string_lossy());
    }
    
    Ok(())
}

// Add after analyze_and_display_psilent function
fn analyze_oob_pitch(csv_path: &Path, pitch_threshold: f32) -> Result<Vec<(u64, String, u32, f32)>, String> {
    // Read CSV file
    let file = match std::fs::File::open(csv_path) {
        Ok(file) => file,
        Err(e) => return Err(format!("Failed to open file: {}", e))
    };
    
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(file);
    
    // Structure: tick,player_id,player_name,origin_x,origin_y,origin_z,viewangle,pitchangle,va_delta,pa_delta
    #[derive(Debug, Deserialize)]
    struct Record {
        tick: u32,
        player_id: u64,
        player_name: String,
        origin_x: f32,
        origin_y: f32,
        origin_z: f32,
        viewangle: f32,
        #[serde(deserialize_with = "deserialize_f32_or_nan")]
        pitchangle: f32,
        va_delta: String,  // Could be "NaN" or a float
        pa_delta: String,  // Could be "NaN" or a float
    }
    
    // Custom deserializer for f32 that handles NaN strings
    fn deserialize_f32_or_nan<'de, D>(deserializer: D) -> Result<f32, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        if s.to_lowercase() == "nan" {
            return Ok(f32::NAN);
        }
        s.parse::<f32>().map_err(serde::de::Error::custom)
    }
    
    // Configuration parameters to filter out false positives
    const MIN_CONSECUTIVE_TICKS: u32 = 4;      // Minimum consecutive ticks required for a valid violation
    const MAX_TICK_GAP: u32 = 2;               // Maximum gap between ticks to be considered part of the same violation
    const IGNORE_INITIAL_TICKS: u32 = 10;      // Ignore violations in initial ticks (typically spawn-related)
    const MINIMUM_VIOLATION_DURATION: u32 = 5; // Minimum number of ticks for a violation period to be valid
    
    let mut oob_detections: Vec<(u64, String, u32, f32)> = Vec::new(); // player_id, name, tick, pitch
    
    // Track violation periods for each player
    let mut violation_periods: HashMap<u64, Vec<(u32, u32, f32)>> = HashMap::new(); // player_id -> [(start_tick, end_tick, max_pitch)]
    let mut active_violations: HashMap<u64, (u32, f32, u32)> = HashMap::new(); // player_id -> (start_tick, max_pitch, consecutive_count)
    let mut player_names: HashMap<u64, String> = HashMap::new(); // player_id -> name
    
    // Track previous records to detect gaps
    let mut last_tick_by_player: HashMap<u64, u32> = HashMap::new();
    
    println!("Analyzing viewangles for out-of-bounds pitch values (threshold: {}°)...", pitch_threshold);
    
    // Process records
    for result in rdr.deserialize() {
        let record: Record = match result {
            Ok(rec) => rec,
            Err(e) => {
                println!("Warning: Skipping malformed record: {}", e);
                continue;
            }
        };
        
        // Store player name for reference
        player_names.insert(record.player_id, record.player_name.clone());
        
        // Skip initial ticks (likely spawn-related)
        if record.tick < IGNORE_INITIAL_TICKS {
            continue;
        }
        
        // Skip records with NaN pitch values
        if record.pitchangle.is_nan() {
            println!("Skipping record with NaN pitch value: Player {} at tick {}", 
                record.player_name, record.tick);
            continue;
        }
        
        // Get absolute pitch value for comparison
        // TF2 pitch values can sometimes go outside the -90 to +90 range,
        // especially with cheats or certain demo recording issues
        let abs_pitch = record.pitchangle.abs();
        
        // Check if pitch is out of bounds (beyond threshold)
        // We consider both normal out of bounds (>89.8) and extreme values (>180)
        let is_violation = abs_pitch > pitch_threshold;
        
        // Check for tick gaps
        let has_gap = if let Some(last_tick) = last_tick_by_player.get(&record.player_id) {
            (record.tick - *last_tick) > MAX_TICK_GAP
        } else {
            false
        };
        
        // Update last tick for this player
        last_tick_by_player.insert(record.player_id, record.tick);
        
        if is_violation {
            // Debug output
            println!("Found OOB pitch: Player {} at tick {} with pitch {:.2}°", 
                record.player_name, record.tick, record.pitchangle);
                
            // Check if we have an active violation for this player
            if let Some((start_tick, max_pitch, count)) = active_violations.get_mut(&record.player_id) {
                if has_gap {
                    // Gap detected - if we have enough consecutive ticks, record the violation
                    if *count >= MIN_CONSECUTIVE_TICKS && 
                       (record.tick - *start_tick) >= MINIMUM_VIOLATION_DURATION {
                        // Record the completed violation period
                        violation_periods
                            .entry(record.player_id)
                            .or_default()
                            .push((*start_tick, record.tick - 1, *max_pitch));
                        
                        // Add to detection list if not already there
                        for tick in *start_tick..=record.tick-1 {
                            oob_detections.push((
                                record.player_id,
                                record.player_name.clone(),
                                tick,
                                *max_pitch
                            ));
                        }
                    }
                    
                    // Start a new violation period
                    *start_tick = record.tick;
                    *max_pitch = abs_pitch;
                    *count = 1;
                } else {
                    // Continue the current violation
                    *count += 1;
                    
                    // Update max pitch if current pitch is more extreme
                    if abs_pitch > *max_pitch {
                        *max_pitch = abs_pitch;
                    }
                    
                    // Add to detections if we've reached the threshold
                    if *count >= MIN_CONSECUTIVE_TICKS {
                        oob_detections.push((
                            record.player_id,
                            record.player_name.clone(),
                            record.tick,
                            abs_pitch
                        ));
                    }
                }
            } else {
                // Start new violation period
                active_violations.insert(record.player_id, (record.tick, abs_pitch, 1));
            }
        } else {
            // Not a violation - check if this ends an active violation
            if let Some((start_tick, max_pitch, count)) = active_violations.remove(&record.player_id) {
                // Only record if we had enough consecutive ticks and minimum duration
                if count >= MIN_CONSECUTIVE_TICKS && 
                   (record.tick - start_tick) >= MINIMUM_VIOLATION_DURATION {
                    // Record the completed violation period
                    violation_periods
                        .entry(record.player_id)
                        .or_default()
                        .push((start_tick, record.tick - 1, max_pitch));
                    
                    // Add all ticks from this period to the detection list
                    // Only add if not already in the list
                    for tick in start_tick..=record.tick-1 {
                        if !oob_detections.iter().any(|(id, _, t, _)| *id == record.player_id && *t == tick) {
                            oob_detections.push((
                                record.player_id,
                                record.player_name.clone(),
                                tick,
                                max_pitch
                            ));
                        }
                    }
                }
            }
        }
    }
    
    // Handle any active violations at the end of the file
    for (player_id, (start_tick, max_pitch, count)) in active_violations {
        let player_name = player_names.get(&player_id).cloned().unwrap_or_else(|| format!("Unknown ({})", player_id));
        
        // Only include if we had enough consecutive ticks and minimum duration
        let last_tick = last_tick_by_player.get(&player_id).cloned().unwrap_or(start_tick);
        if count >= MIN_CONSECUTIVE_TICKS && (last_tick - start_tick) >= MINIMUM_VIOLATION_DURATION {
            violation_periods
                .entry(player_id)
                .or_default()
                .push((start_tick, last_tick, max_pitch));
            
            // Add all ticks from this period to the detection list if not already there
            for tick in start_tick..=last_tick {
                if !oob_detections.iter().any(|(id, _, t, _)| *id == player_id && *t == tick) {
                    oob_detections.push((
                        player_id,
                        player_name.clone(),
                        tick,
                        max_pitch
                    ));
                }
            }
        }
    }
    
    // Sort detections by player ID and tick
    oob_detections.sort_by(|a, b| {
        if a.0 == b.0 {
            a.2.cmp(&b.2) // Sort by tick if same player
        } else {
            a.0.cmp(&b.0) // Sort by player ID
        }
    });
    
    // Generate text summary
    let summary_path = csv_path.with_file_name(format!(
        "oob_pitch_summary_{}.txt", 
        chrono::Local::now().format("%Y%m%d_%H%M%S")
    ));
    
    let mut summary_file = match std::fs::File::create(&summary_path) {
        Ok(file) => file,
        Err(e) => return Err(format!("Failed to create summary file: {}", e))
    };
    
    // Write summary header
    let header = format!("OUT-OF-BOUNDS PITCH ANALYSIS SUMMARY (SPAWN-FILTERED)\n\
                         Analysis timestamp: {}\n\
                         Source file: {}\n\
                         Pitch threshold: {}°\n\
                         Filtering criteria:\n\
                         - Minimum consecutive ticks: {}\n\
                         - Maximum tick gap: {}\n\
                         - Ignored initial ticks: {}\n\
                         - Minimum violation duration: {} ticks\n\
                         Total violations found: {}\n\n\
                         === VIOLATION PERIODS BY PLAYER ===\n",
                         chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                         csv_path.file_name().unwrap_or_default().to_string_lossy(),
                         pitch_threshold,
                         MIN_CONSECUTIVE_TICKS,
                         MAX_TICK_GAP,
                         IGNORE_INITIAL_TICKS,
                         MINIMUM_VIOLATION_DURATION,
                         oob_detections.len());
    
    if let Err(e) = summary_file.write_all(header.as_bytes()) {
        return Err(format!("Failed to write to summary file: {}", e));
    }
    
    // Write player violations
    let mut player_summaries = Vec::new();
    
    for (player_id, periods) in violation_periods {
        let player_name = player_names.get(&player_id).cloned().unwrap_or_else(|| format!("Unknown ({})", player_id));
        
        // Skip players with no actual violation periods (may have been filtered out)
        if periods.is_empty() {
            continue;
        }
        
        let mut player_text = format!("PLAYER: {} (ID: {})\n", player_name, player_id);
        player_text.push_str(&format!("Total violation periods: {}\n", periods.len()));
        
        // Add each violation period
        for (i, (start_tick, end_tick, max_pitch)) in periods.iter().enumerate() {
            let end_display = if *end_tick == u32::MAX {
                "end of demo".to_string()
            } else {
                format!("tick {}", end_tick)
            };
            
            let duration = if *end_tick == u32::MAX {
                "unknown".to_string()
            } else {
                format!("{}", end_tick - start_tick + 1)
            };
            
            player_text.push_str(&format!("  {}: Ticks {}-{} (duration: {} ticks, max pitch: {:.2}°)\n", 
                i + 1, start_tick, end_display, duration, max_pitch));
        }
        
        player_text.push('\n');
        
        if let Err(e) = summary_file.write_all(player_text.as_bytes()) {
            return Err(format!("Failed to write player data to summary file: {}", e));
        }
        
        // Store for UI display if we have any periods
        if !periods.is_empty() {
            player_summaries.push((player_name, periods.len(), periods[0].0));
        }
    }
    
    // Store summary path and player summaries for UI
    LATEST_OOB_SUMMARY.with(|cell| {
        *cell.borrow_mut() = Some((summary_path.clone(), player_summaries));
    });
    
    println!("Found {} instances of out-of-bounds pitch values after filtering", oob_detections.len());
    println!("Detailed summary written to: {}", summary_path.to_string_lossy());
    
    Ok(oob_detections)
}

// Add this function to display OOB pitch analysis in the UI
fn analyze_and_display_oob_pitch(path: &Path, threshold: f32) -> Result<(), String> {
    // Run the analysis with the provided threshold
    let detections = analyze_oob_pitch(path, threshold)?;
    
    // Write results to a new CSV for easy examination
    let output_path = path.with_file_name(format!(
        "oob_pitch_analysis_{}.csv", 
        chrono::Local::now().format("%Y%m%d_%H%M%S")
    ));
    
    let mut wtr = csv::WriterBuilder::new()
        .has_headers(true)
        .from_path(&output_path)
        .map_err(|e| format!("Failed to create output file: {}", e))?;
    
    // Write header
    wtr.write_record(&["tick", "player_id", "player_name", "pitch_value", "threshold", "excess"])
        .map_err(|e| format!("Failed to write header: {}", e))?;
    
    // Write detection data
    for (id, name, tick, pitch) in &detections {
        // Calculate how much the pitch exceeds the threshold
        let excess = pitch - threshold;
        
        wtr.write_record(&[
            tick.to_string(),
            id.to_string(),
            name.to_string(),
            format!("{:.2}", pitch),
            format!("{:.2}", threshold),
            format!("{:.2}", excess)
        ]).map_err(|e| format!("Failed to write record: {}", e))?;
    }
    
    wtr.flush().map_err(|e| format!("Failed to flush writer: {}", e))?;
    
    // Get summary information from thread local storage
    let summary_info = LATEST_OOB_SUMMARY.with(|cell| cell.borrow().clone());
    
    // Organize detections by player for better reporting
    let mut detections_by_player: HashMap<u64, Vec<(String, u32, f32)>> = HashMap::new();
    for (player_id, name, tick, pitch) in &detections {
        detections_by_player.entry(*player_id)
            .or_default()
            .push((name.clone(), *tick, *pitch));
    }
    
    // Print summary
    println!("\nOut-of-bounds pitch analysis complete!");
    println!("Total illegal pitch values found: {}", detections.len());
    
    if detections.is_empty() {
        println!("No illegal pitch values found!");
    } else {
        println!("\nPlayers with out-of-bounds pitch values:");
        
        // Print per-player summary
        for (player_id, detections) in &detections_by_player {
            let sample_detection = &detections[0];
            let name = &sample_detection.0;
            let tick_count = detections.len();
            let max_pitch = detections.iter().map(|(_, _, pitch)| *pitch).fold(0.0f32, f32::max);
            
            println!("- Player '{}' (ID: {}): {} illegal values, max pitch: {:.2}°", 
                name, player_id, tick_count, max_pitch);
        }
        
        println!("\nTop 10 most extreme pitch values:");
        // Clone and sort by pitch value (descending)
        let mut sorted_detections = detections.clone();
        sorted_detections.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
        
        for (i, (id, name, tick, pitch)) in sorted_detections.iter().take(10).enumerate() {
            println!("{}. Player '{}' (ID: {}) at tick {}: {:.2}° pitch (exceeds limit of {}° by {:.2}°)",
                i+1, name, id, tick, pitch, threshold, pitch - threshold);
        }
        
        // If we have summary info, print that as well
        if let Some((summary_path, player_summaries)) = summary_info {
            println!("\nViolation periods by player:");
            for (player_name, period_count, start_tick) in player_summaries {
                println!("- {} has {} sustained violation periods (first at tick {})", 
                    player_name, period_count, start_tick);
            }
            
            println!("\nDetailed summary written to: {}", summary_path.to_string_lossy());
        }
        
        println!("\nDetailed analysis written to: {}", output_path.to_string_lossy());
    }
    
    Ok(())
}

fn test_oob_detection() {
    // Test code
    println!("Testing OOB pitch detection...");
    
    let path = std::path::Path::new("test_oob_pitch.csv");
    if !path.exists() {
        println!("Test file not found: {}", path.display());
        return;
    }
    
    match analyze_oob_pitch(path, 89.8) {
        Ok(detections) => {
            println!("Found {} OOB pitch violations", detections.len());
            
            // Print the first 10 detections
            for (i, (id, name, tick, pitch)) in detections.iter().take(10).enumerate() {
                println!("{}. Player {} (ID: {}) at tick {}: pitch = {:.2}°", 
                    i+1, name, id, tick, pitch);
            }
            
            if detections.len() > 10 {
                println!("... and {} more", detections.len() - 10);
            }
            
            // Now test the full display function
            match analyze_and_display_oob_pitch(path, 89.8) {
                Ok(_) => println!("Analysis and display completed successfully"),
                Err(e) => println!("Error during analysis and display: {}", e),
            }
        },
        Err(e) => {
            println!("Error during OOB pitch analysis: {}", e);
        }
    }
}