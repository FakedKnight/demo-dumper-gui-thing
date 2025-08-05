pub mod viewangles;
pub mod base;

use std::sync::atomic::AtomicBool;
use serde::{Serialize, Deserialize};
use serde_json::Value;
use tf_demo_parser::{demo::{message::Message, data::DemoTick}, MessageType, ParserState};
use anyhow::Error;

pub static SILENT: AtomicBool = AtomicBool::new(false);

#[derive(Serialize, Deserialize, Clone)]
pub struct Detection {
    pub tick: u32,
    pub algorithm: String,
    pub player: u64,
    pub data: Value
}

pub trait CheatAlgorithm {
    fn default(&self) -> bool {
        true
    }

    fn algorithm_name(&self) -> &str;

    fn does_handle(&self, message_type: MessageType) -> bool {
        match self.handled_messages() {
            Ok(types) => types.contains(&message_type),
            Err(parse_all) => parse_all,
        }
    }

    fn init(&mut self) -> Result<(), Error> {
        Ok(())
    }

    fn on_tick(&mut self, state: &CheatAnalyserState, parser_state: &ParserState) -> Result<Vec<Detection>, Error>;

    fn handled_messages(&self) -> Result<Vec<MessageType>, bool>;

    fn on_message(&mut self, message: &Message, state: &CheatAnalyserState, parser_state: &ParserState, tick: DemoTick) -> Result<Vec<Detection>, Error>;

    fn finish(&mut self) -> Result<Vec<Detection>, Error> {
        Ok(vec![])
    }
}

#[derive(Clone)]
pub struct CheatAnalyserState {
    pub tick: u32,
    pub player_states: std::collections::HashMap<u64, PlayerState>,
}

#[derive(Clone)]
pub struct PlayerState {
    pub steamid: u64,
    pub viewangles: Option<(f32, f32, f32)>, // pitch, yaw, roll
    pub position: Option<(f32, f32, f32)>,    // x, y, z
    pub name: String,
} 