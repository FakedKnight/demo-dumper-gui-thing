use std::collections::HashMap;
use anyhow::Error;
use serde_json::json;
use tf_demo_parser::{
    demo::{
        header::Header,
        packet::{Packet, message::MessagePacket},
    },
    ParserState,
};

use crate::cheater_detection::{CheatAlgorithm, CheatAnalyserState, Detection, SILENT};

pub struct CheatAnalyser {
    algorithms: Vec<Box<dyn CheatAlgorithm>>,
    state: CheatAnalyserState,
    detections: Vec<Detection>,
    tick_count: u32,
}

impl CheatAnalyser {
    pub fn new(algorithms: Vec<Box<dyn CheatAlgorithm>>) -> Self {
        Self {
            algorithms,
            state: CheatAnalyserState {
                tick: 0,
                player_states: HashMap::new(),
            },
            detections: Vec::new(),
            tick_count: 0,
        }
    }

    pub fn init(&mut self) -> Result<(), Error> {
        for algorithm in &mut self.algorithms {
            algorithm.init()?;
        }
        Ok(())
    }

    pub fn handle_message(&mut self, message: &MessagePacket, parser_state: &ParserState, tick: u32) -> Result<(), Error> {
        self.tick_count = tick;
        self.state.tick = tick;
        
        for algorithm in &mut self.algorithms {
            // Always process all messages for simplicity
            if !message.messages.is_empty() {
                // Simply pass the tick value directly in the handle_tick call
                let detections = algorithm.on_tick(&self.state, parser_state)?;
                self.detections.extend(detections);
            }
        }
        Ok(())
    }

    pub fn handle_tick(&mut self, parser_state: &ParserState) -> Result<(), Error> {
        self.tick_count += 1;
        self.state.tick = self.tick_count;

        for algorithm in &mut self.algorithms {
            let detections = algorithm.on_tick(&self.state, parser_state)?;
            self.detections.extend(detections);
        }
        Ok(())
    }

    pub fn finish(&mut self) -> Result<(), Error> {
        for algorithm in &mut self.algorithms {
            let detections = algorithm.finish()?;
            self.detections.extend(detections);
        }
        Ok(())
    }

    pub fn print_detection_json(&self, pretty: bool) {
        let json = json!({
            "detections": self.detections,
            "metadata": {
                "total_ticks": self.tick_count,
            }
        });

        if pretty {
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
        } else {
            println!("{}", serde_json::to_string(&json).unwrap());
        }
    }

    pub fn get_detections(&self) -> &Vec<Detection> {
        &self.detections
    }

    pub fn get_tick_count_u32(&self) -> u32 {
        self.tick_count
    }

    pub fn print_metadata(&self) {
        if !SILENT.load(std::sync::atomic::Ordering::Relaxed) {
            println!("Total ticks processed: {}", self.tick_count);
        }
    }
}

pub struct CheatDemoHandler {
    pub analyser: CheatAnalyser,
    pub state_handler: ParserState,
}

impl CheatDemoHandler {
    pub fn with_analyser(analyser: CheatAnalyser) -> Self {
        Self {
            analyser,
            state_handler: ParserState::new(0, |_| true, false),
        }
    }

    pub fn handle_header(&mut self, _header: &Header) {
        // Just record that we've processed a header
    }

    pub fn handle_packet(&mut self, packet: Packet) -> Result<(), Error> {
        match packet {
            Packet::Message(message) => {
                // Use the current tick count
                let tick = self.analyser.get_tick_count_u32();
                self.analyser.handle_message(&message, &self.state_handler, tick)?;
            }
            Packet::Signon(_) | Packet::SyncTick(_) => {
                // Handle sync packets
                self.analyser.handle_tick(&self.state_handler)?;
            }
            _ => {}
        }
        Ok(())
    }
} 