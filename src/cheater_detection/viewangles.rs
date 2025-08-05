use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Write};
use std::path::PathBuf;
use anyhow::Error;
use serde_json::json;
use tf_demo_parser::{
    demo::{data::DemoTick, message::Message},
    MessageType, ParserState,
};

use super::{CheatAlgorithm, CheatAnalyserState, Detection};

/// Structure to record viewangle data over time
#[derive(Debug)]
struct ViewAngleRecord {
    tick: u32,
    pitch: f32,
    yaw: f32,
    roll: f32,
    position: Option<(f32, f32, f32)>,  // Optional position data
    va_delta: Option<f32>,  // Yaw delta from previous tick
    pa_delta: Option<f32>,  // Pitch delta from previous tick
}

pub struct ViewAnglesAnalyzer {
    previous_angles: HashMap<u64, (f32, f32, f32)>,
    detections: Vec<Detection>,
    // Store all viewangles for each player
    player_viewangles: HashMap<u64, Vec<ViewAngleRecord>>,
    // Map player IDs to names for better labeling
    player_names: HashMap<u64, String>,
    output_path: Option<PathBuf>,
    debug_mode: bool,
}

impl ViewAnglesAnalyzer {
    pub fn new() -> Self {
        Self {
            previous_angles: HashMap::new(),
            detections: Vec::new(),
            player_viewangles: HashMap::new(),
            player_names: HashMap::new(),
            output_path: None,
            debug_mode: false,  // Disable debug mode by default
        }
    }
    
    /// Set the output file path for viewangles data
    pub fn set_output_path(&mut self, path: PathBuf) {
        self.output_path = Some(path);
    }
    
    /// Calculate angle delta properly handling wraparound at 360 degrees
    fn calculate_angle_delta(&self, current: f32, previous: f32) -> f32 {
        let diff = (current - previous).rem_euclid(360.0);
        if diff > 180.0 {
            diff - 360.0
        } else {
            diff
        }
    }
    
    fn check_angle_change(&mut self, player_id: u64, current: (f32, f32, f32), position: Option<(f32, f32, f32)>, tick: u32) {
        // Record the viewangle data
        let (curr_pitch, curr_yaw, curr_roll) = current;
        
        if self.debug_mode {
            println!("Recording viewangles for player {}: Pitch={:.2}, Yaw={:.2}, Roll={:.2} at tick {}",
                player_id, curr_pitch, curr_yaw, curr_roll, tick);
        }
        
        // Calculate deltas if we have previous angles
        let (va_delta, pa_delta) = if let Some(previous) = self.previous_angles.get(&player_id) {
            let (prev_pitch, prev_yaw, _) = *previous;
            
            // Calculate proper angle deltas accounting for wraparound
            let yaw_delta = self.calculate_angle_delta(curr_yaw, prev_yaw);
            let pitch_delta = curr_pitch - prev_pitch;  // Pitch doesn't wrap around
            
            (Some(yaw_delta), Some(pitch_delta))
        } else {
            (None, None)
        };
        
        // Store this viewangle record for the player
        self.player_viewangles
            .entry(player_id)
            .or_default()
            .push(ViewAngleRecord {
                tick,
                pitch: curr_pitch,
                yaw: curr_yaw,
                roll: curr_roll,
                position,
                va_delta,
                pa_delta,
            });
        
        // Check for suspicious behavior in the angle changes
        if let (Some(yaw_delta), Some(pitch_delta)) = (va_delta, pa_delta) {
            // Detect suspicious flicks - large angle changes in a single tick
            let yaw_delta_abs = yaw_delta.abs();
            let pitch_delta_abs = pitch_delta.abs();
            
            // Thresholds for detection
            const SUSPICIOUS_YAW_CHANGE: f32 = 30.0;    // Large horizontal flick
            const SUSPICIOUS_PITCH_CHANGE: f32 = 20.0;  // Large vertical flick
            
            if yaw_delta_abs > SUSPICIOUS_YAW_CHANGE || pitch_delta_abs > SUSPICIOUS_PITCH_CHANGE {
                self.detections.push(Detection {
                    tick,
                    algorithm: self.algorithm_name().to_string(),
                    player: player_id,
                    data: json!({
                        "type": "suspicious_angle_change",
                        "pitch_delta": pitch_delta,
                        "yaw_delta": yaw_delta,
                        "previous": {
                            "pitch": curr_pitch - pitch_delta,
                            "yaw": curr_yaw - yaw_delta,
                        },
                        "current": {
                            "pitch": curr_pitch,
                            "yaw": curr_yaw,
                        },
                        "magnitude": (pitch_delta_abs.powi(2) + yaw_delta_abs.powi(2)).sqrt(),
                    }),
                });
            }
            
            // Detect out-of-bounds pitch values
            const MAX_PITCH_ANGLE: f32 = 89.8;  // Maximum normal pitch angle in TF2
            if curr_pitch.abs() > MAX_PITCH_ANGLE {
                self.detections.push(Detection {
                    tick,
                    algorithm: self.algorithm_name().to_string(),
                    player: player_id,
                    data: json!({
                        "type": "out_of_bounds_pitch",
                        "pitch": curr_pitch,
                        "limit": MAX_PITCH_ANGLE,
                        "excess": curr_pitch.abs() - MAX_PITCH_ANGLE,
                    }),
                });
            }
        }

        // Update previous angles
        self.previous_angles.insert(player_id, current);
    }
    
    /// Save all recorded viewangles to a file
    fn write_viewangles_to_file(&self) -> io::Result<PathBuf> {
        // Create a temporary file
        let temp_dir = std::env::temp_dir();
        let output_path = self.output_path.clone().unwrap_or_else(|| {
            let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
            temp_dir.join(format!("viewangles_{}.csv", timestamp))
        });
        
        // Print diagnostic info
        println!("Writing viewangles to file: {}", output_path.display());
        println!("Number of players with viewangles: {}", self.player_viewangles.len());
        for (player_id, angles) in &self.player_viewangles {
            println!("Player {} has {} viewangle records", player_id, angles.len());
            let name = self.player_names.get(player_id).cloned().unwrap_or_else(|| format!("Player {}", player_id));
            println!("  Name: {}", name);
        }
        
        let mut file = File::create(&output_path)?;
        
        // Write CSV header with extended information
        writeln!(file, "tick,player_id,player_name,origin_x,origin_y,origin_z,viewangle,pitchangle,va_delta,pa_delta")?;
        
        // If we have no records, add some sample data to avoid empty file
        if self.player_viewangles.is_empty() {
            writeln!(file, "0,0,No_Players_Found,0.00,0.00,0.00,0.00,0.00,NaN,NaN")?;
            println!("WARNING: No viewangle data was collected!");
        }
        
        // Write all viewangle records
        for (player_id, records) in &self.player_viewangles {
            let player_name = self.player_names
                .get(player_id)
                .map_or_else(|| format!("Player {}", player_id), |name| name.clone())
                .replace(",", ""); // Sanitize commas for CSV format
                
            for record in records {
                // Extract position or use 0,0,0 if none available
                let (origin_x, origin_y, origin_z) = record.position.unwrap_or((0.0, 0.0, 0.0));
                
                // Format delta values, using "NaN" for None
                let va_delta_str = record.va_delta.map_or_else(|| "NaN".to_string(), |v| format!("{:.2}", v));
                let pa_delta_str = record.pa_delta.map_or_else(|| "NaN".to_string(), |v| format!("{:.2}", v));
                
                writeln!(
                    file,
                    "{},{},{},{:.2},{:.2},{:.2},{:.2},{:.2},{},{}",
                    record.tick,
                    player_id,
                    player_name,
                    origin_x, origin_y, origin_z,
                    record.yaw,
                    record.pitch,
                    va_delta_str,
                    pa_delta_str,
                )?;
            }
        }
        
        println!("Successfully wrote viewangles data to: {}", output_path.display());
        println!("File size: {} bytes", std::fs::metadata(&output_path)?.len());
        
        Ok(output_path)
    }
}

impl CheatAlgorithm for ViewAnglesAnalyzer {
    fn algorithm_name(&self) -> &str {
        "viewangles_analyzer"
    }

    fn handled_messages(&self) -> Result<Vec<MessageType>, bool> {
        Ok(vec![MessageType::UserMessage])
    }

    fn on_tick(&mut self, state: &CheatAnalyserState, _parser_state: &ParserState) -> Result<Vec<Detection>, Error> {
        // Process each player's viewangles
        for (player_id, player_state) in &state.player_states {
            // Store player name for better labeling
            if !player_state.name.is_empty() {
                self.player_names.insert(*player_id, player_state.name.clone());
                if self.debug_mode {
                    println!("Found player name: {} (ID: {})", player_state.name, player_id);
                }
            }
            
            // Process viewangles and position if available
            if let Some(angles) = player_state.viewangles {
                if self.debug_mode {
                    println!("Processing angles for player {}: {:?}", player_id, angles);
                }
                self.check_angle_change(*player_id, angles, player_state.position, state.tick);
            }
        }

        // Return any detections from this tick
        let detections = self.detections.clone();
        self.detections.clear();
        Ok(detections)
    }

    fn on_message(&mut self, _message: &Message, _state: &CheatAnalyserState, _parser_state: &ParserState, _tick: DemoTick) -> Result<Vec<Detection>, Error> {
        Ok(vec![])
    }

    fn finish(&mut self) -> Result<Vec<Detection>, Error> {
        println!("ViewAnglesAnalyzer finishing...");
        println!("Total players tracked: {}", self.player_names.len());
        println!("Total angle records: {}", self.player_viewangles.values().map(|v| v.len()).sum::<usize>());
        
        // Write all collected viewangles to file
        match self.write_viewangles_to_file() {
            Ok(path) => println!("Viewangles data written to: {}", path.display()),
            Err(e) => eprintln!("Failed to write viewangles data to file: {}", e),
        }
        
        self.previous_angles.clear();
        Ok(vec![])
    }
} 