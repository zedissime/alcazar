use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    List,
    Start { name: String },
    Restart { name: String },
    Stop { name: String },
    /// Stops every process and the daemon itself (mirrors `pm2 kill`).
    StopAllAndExit,
    LogPath { name: String },
    Ping,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Ok,
    Error { message: String },
    Statuses(Vec<ProcessStatusDto>),
    Path(String),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProcessStatusDto {
    pub name: String,
    pub state: String,
    pub pid: Option<u32>,
    pub restarts: u32,
    pub uptime_secs: Option<u64>,
    pub port: Option<u16>,
}
