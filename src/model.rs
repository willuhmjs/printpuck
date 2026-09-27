// Printer status model: what PrintPuck knows about the printer.
//
// The wire facts (topic names, JSON field names, gcode state strings) are
// Bambu's documented local protocol and are free to use; this data model and
// all code around it is original.

extern crate alloc;
use alloc::{format, string::String, vec::Vec};

use serde::Deserialize;

/// `print.gcode_state` values the printer reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcodeState {
    Idle,
    Running,
    Pause,
    Prepare,
    Finish,
    Failed,
    Other,
}

impl GcodeState {
    pub fn parse(s: &str) -> Self {
        match s {
            "IDLE" => Self::Idle,
            "RUNNING" => Self::Running,
            "PAUSE" => Self::Pause,
            "PREPARE" => Self::Prepare,
            "FINISH" => Self::Finish,
            "FAILED" => Self::Failed,
            _ => Self::Other,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::Running => "Printing",
            Self::Pause => "Paused",
            Self::Prepare => "Preparing",
            Self::Finish => "Finished",
            Self::Failed => "Failed",
            Self::Other => "Unknown",
        }
    }
}

/// One AMS tray as the puck needs it.
#[derive(Debug, Clone, Copy, Default)]
pub struct Tray {
    /// RGB565 color for drawing, or None if absent/unknown.
    pub color: Option<u16>,
    pub active: bool,
}

/// The distilled status the UI draws from. Updated from parsed reports.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub state: Option<GcodeState>,
    pub stage: Option<String>,
    pub progress: Option<f32>,
    pub remaining_min: Option<u32>,
    pub nozzle_temp: Option<f32>,
    pub nozzle_target: Option<f32>,
    pub bed_temp: Option<f32>,
    pub bed_target: Option<f32>,
    pub layer: Option<u32>,
    pub layer_total: Option<u32>,
    pub task_name: Option<String>,
    pub light_on: Option<bool>,
    pub error_code: Option<u32>,
    pub ams_trays: [Option<Tray>; 4],
    pub ams_active: bool,
    /// Report count, to drive "stale" detection.
    pub reports: u32,
    pub last_update: Option<u64>,
}

/// Serde view of the report JSON. Only the fields PrintPuck uses are listed;
/// everything else in the payload is skipped by the deserializer.
#[derive(Debug, Deserialize)]
pub struct Report {
    #[serde(default)]
    pub print: PrintBlock,
}

#[derive(Debug, Deserialize, Default)]
pub struct PrintBlock {
    #[serde(default)]
    pub gcode_state: Option<String>,
    #[serde(default)]
    pub stg_cur: Option<String>,
    #[serde(default)]
    pub mc_percent: Option<f32>,
    #[serde(default)]
    pub mc_remaining_time: Option<i64>,
    #[serde(default)]
    pub nozzle_temper: Option<f32>,
    #[serde(default)]
    pub nozzle_target_temper: Option<f32>,
    #[serde(default)]
    pub bed_temper: Option<f32>,
    #[serde(default)]
    pub bed_target_temper: Option<f32>,
    #[serde(default)]
    pub layer_num: Option<i64>,
    #[serde(default)]
    pub total_layer_num: Option<i64>,
    #[serde(default)]
    pub subtask_name: Option<String>,
    #[serde(default)]
    pub print_error: Option<u32>,
    #[serde(default)]
    pub lights_report: Option<Vec<LightEntry>>,
    #[serde(default)]
    pub ams: Option<AmsBlock>,
}

#[derive(Debug, Deserialize)]
pub struct LightEntry {
    #[serde(default)]
    pub node: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct AmsBlock {
    #[serde(default)]
    pub tray_exist: Option<Vec<bool>>,
    #[serde(default)]
    pub tray_colors: Option<Vec<String>>,
    #[serde(default)]
    pub tray_now: Option<u32>,
    #[serde(default)]
    pub tray_type: Option<Vec<String>>,
}

/// Parses one `device/<serial>/report` payload into `status`.
pub fn apply_report(status: &mut Status, payload: &[u8], now_ms: u64) {
    let Ok(report) = serde_json::from_slice::<Report>(payload) else {
        return;
    };
    let p = report.print;

    if let Some(s) = p.gcode_state.as_deref() {
        status.state = Some(GcodeState::parse(s));
    }
    status.stage = p.stg_cur.filter(|s| !s.is_empty());
    if let Some(v) = p.mc_percent {
        status.progress = Some(v.clamp(0.0, 100.0));
    }
    if let Some(v) = p.mc_remaining_time {
        status.remaining_min = Some(v.max(0) as u32);
    }
    status.nozzle_temp = p.nozzle_temper;
    status.nozzle_target = p.nozzle_target_temper;
    status.bed_temp = p.bed_temper;
    status.bed_target = p.bed_target_temper;
    if let Some(v) = p.layer_num {
        status.layer = Some(v.max(0) as u32);
    }
    if let Some(v) = p.total_layer_num {
        status.layer_total = Some(v.max(0) as u32);
    }
    status.task_name = p.subtask_name.filter(|s| !s.is_empty());
    status.error_code = p.print_error.filter(|&c| c != 0);

    if let Some(lights) = &p.lights_report {
        for l in lights {
            if l.node.as_deref() == Some("chamber_light") {
                status.light_on = Some(l.mode.as_deref() == Some("on"));
            }
        }
    }

    if let Some(ams) = &p.ams {
        let exist = ams.tray_exist.as_deref().unwrap_or(&[]);
        let colors = ams.tray_colors.as_deref().unwrap_or(&[]);
        let now = ams.tray_now.unwrap_or(u32::MAX);
        for i in 0..4 {
            let present = exist.get(i).copied().unwrap_or(false);
            let color = colors
                .get(i)
                .and_then(|c| parse_hex_color(c))
                .filter(|_| present);
            let active = present && now != u32::MAX && (now as usize) < 4 && (now as usize) == i;
            status.ams_trays[i] = color.map(|c| Tray { color: Some(c), active });
        }
        status.ams_active = status.ams_trays.iter().any(|t| t.is_some());
    }

    status.reports = status.reports.wrapping_add(1);
    status.last_update = Some(now_ms);
}

/// "#RRGGBB" (6 hex digits) -> RGB565. Returns None for anything else
/// (Bambu also uses 8-digit "AARRGGBB" on some models; the trailing-2 form
/// is handled too).
fn parse_hex_color(s: &str) -> Option<u16> {
    let h = s.strip_prefix('#')?;
    let (r, g, b) = match h.len() {
        6 => (hex(&h[0..2])?, hex(&h[2..4])?, hex(&h[4..6])?),
        8 => (hex(&h[2..4])?, hex(&h[4..6])?, hex(&h[6..8])?),
        _ => return None,
    };
    Some(((r as u16) << 8) | ((g as u16) << 3) | (b as u16 >> 3))
}

fn hex(s: &str) -> Option<u8> {
    u8::from_str_radix(s, 16).ok()
}

/// `{"pushing":{"sequence_id":"N","command":"pushall"}}` request payload.
pub fn pushall_payload(seq: u32) -> String {
    alloc::format!(
        "{{\"pushing\":{{\"sequence_id\":\"{}\",\"command\":\"pushall\"}}}}",
        seq
    )
}

/// Chamber-light command payload (protocol fact, JSON built fresh).
pub fn light_payload(seq: u32, on: bool) -> String {
    alloc::format!(
        "{{\"system\":{{\"sequence_id\":\"{}\",\"command\":\"ledctrl\",\
          \"led_node\":\"chamber_light\",\"led_mode\":\"{}\"}}}}",
        seq,
        if on { "on" } else { "off" }
    )
}

/// Time remaining as "H:MM" (or "M min" under an hour).
pub fn fmt_remaining(min: u32) -> String {
    if min >= 60 {
        alloc::format!("{}h{:02}", min / 60, min % 60)
    } else {
        alloc::format!("{}m", min)
    }
}
