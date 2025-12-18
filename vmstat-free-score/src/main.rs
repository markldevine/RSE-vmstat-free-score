use anyhow::Result;
use chrono::{Local, NaiveTime, Timelike};
use redis::Commands;
use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::process::Command;
use std::thread;
use std::time::Duration;

// --- RSE Configuration ---
const REDIS_HOST: &str = "redis://172.19.2.254/";
const STATIC_MEM_MAX_KB: f64 = 2048.0 * 1024.0;
const DRAIN_WINDOW_SEC: i64 = 900;
const EMA_ALPHA: f64 = 0.30;

// Set to false for production silence
const VERBOSE: bool = false;

// --- Data Structures ---
#[derive(Debug, Deserialize)]
struct VmstatSample {
    #[serde(rename = "v-r")] r: u64,
    #[serde(rename = "v-b")] _b: u64,
    #[serde(rename = "v-free")] free: u64,
    #[serde(rename = "v-buff")] buff: u64,
    #[serde(rename = "v-cache")] cache: u64,
    #[serde(rename = "v-so")] so: u64,
    #[serde(rename = "v-cs")] cs: u64,
    #[serde(rename = "v-in")] input_in: u64,
    #[serde(rename = "v-id")] id: u64,
    #[serde(rename = "v-wa")] wa: u64,
    #[serde(rename = "v-st")] st: u64,
}

struct Ceilings {
    max_r: u64,
    max_cs: u64,
    max_in: u64,
}

#[derive(Debug)]
struct DebugFrame {
    mem_mode: String,
    friction_display: String,
}

// --- Main Daemon Loop ---
fn main() {
    // This is the only routine message you will see on success
    println!("Starting RSE Freedom Score Daemon (Quiet Mode)...");
    
    let mut moving_score: Option<f64> = None;

    loop {
        match process_cycle(&mut moving_score) {
            Ok(_) => {},
            // Errors are still printed to stderr (journald priority: err)
            Err(e) => eprintln!("Error in cycle: {:#}", e),
        }

        sleep_until_next_second();
    }
}

// --- The Cycle Logic ---
fn process_cycle(moving_score: &mut Option<f64>) -> Result<()> {
    let hostname = get_hostname()?;
    let history_key = format!("RSE^statistics^{}^vmstat^rollingsixty", hostname);
    let candidate_set = "RSE^worker-node-candidates";

    // 1. Maintenance Check
    if is_maintenance_imminent(DRAIN_WINDOW_SEC)? {
        update_valkey(candidate_set, &hostname, 0.0)?;
        // Quiet return during maintenance window unless logging is needed
        return Ok(());
    }

    // 2. Connect to Valkey
    let client = redis::Client::open(REDIS_HOST)?;
    let mut con = client.get_connection()?;

    // 3. Fetch Data
    let mut ceilings = get_ceilings(&mut con)?;
    let raw_history: Vec<String> = con.lrange(&history_key, 0, 59)?;

    if raw_history.is_empty() {
        return Ok(());
    }

    let samples: Vec<VmstatSample> = raw_history
        .iter()
        .filter_map(|json| serde_json::from_str(json).ok())
        .collect();

    // 4. Calculate Score (Reverse EMA)
    let mut last_debug = DebugFrame { mem_mode: "Init".into(), friction_display: "0%".into() };

    for sample in samples.iter().rev() {
        let (instant, debug) = calculate_instant_score(sample, &ceilings);
        
        *moving_score = match *moving_score {
            Some(prev) => Some((instant * EMA_ALPHA) + (prev * (1.0 - EMA_ALPHA))),
            None => Some(instant),
        };
        last_debug = debug;
    }

    let final_score = moving_score.unwrap_or(0.0);

    // 5. Self-Tuning
    if final_score > 40.0 && final_score < 95.0 {
        update_dynamic_ceilings(&mut con, &samples, &mut ceilings)?;
    }

    // 6. Output (Silenced)
    let rounded_score = (final_score * 100.0).round() / 100.0;
    
    if VERBOSE {
        println!("[{}] Score: {:.2} | Mode: {} | Friction: {}", 
            hostname, rounded_score, last_debug.mem_mode, last_debug.friction_display);
    }

    // 7. Commit to Valkey
    let _ : () = con.zadd(candidate_set, &hostname, rounded_score)?;

    Ok(())
}

// --- Timing Helper ---
fn sleep_until_next_second() {
    let now = Local::now();
    let now_ns = now.nanosecond();
    let sleep_ns = 1_000_000_000 - now_ns;
    // Add 1ms buffer to ensure we land just after the boundary
    let duration = Duration::from_nanos(sleep_ns as u64) + Duration::from_millis(1);
    thread::sleep(duration);
}

// --- Calculation Engine ---
fn calculate_instant_score(v: &VmstatSample, c: &Ceilings) -> (f64, DebugFrame) {
    let proc_ratio = v.r as f64 / c.max_r as f64;
    let proc_score = clamp(100.0 - (proc_ratio * 40.0), 0.0, 100.0);

    let cpu_score = clamp(v.id as f64 - (v.st as f64 * 2.0), 0.0, 100.0);

    let available = (v.free + v.cache + v.buff) as f64;
    let mem_raw = (available / STATIC_MEM_MAX_KB) * 100.0;
    
    let mut mem_score = mem_raw;
    let mut mem_mode = "Raw";

    if v.r < c.max_r && v.wa < 1 && v.so == 0 {
        mem_score = cpu_score;
        mem_mode = "Trusted-Idle";
    }

    let cs_ratio = v.cs as f64 / c.max_cs as f64;
    let in_ratio = v.input_in as f64 / c.max_in as f64;

    let cs_penalty = if cs_ratio > 0.5 { (cs_ratio - 0.5) * 0.1 } else { 0.0 };
    let in_penalty = if in_ratio > 0.5 { (in_ratio - 0.5) * 0.1 } else { 0.0 };

    let friction_drag = 1.0 - cs_penalty - in_penalty;

    let base = (proc_score * 0.25) + (mem_score * 0.25) + (cpu_score * 0.50);
    let mut score = base * friction_drag;

    if v.so > 0 && v.wa > 5 {
        score *= 0.1;
    }

    let debug = DebugFrame {
        mem_mode: mem_mode.to_string(),
        friction_display: format!("{:.1}%", friction_drag * 100.0),
    };

    (clamp(score, 0.0, 100.0), debug)
}

// --- Helpers ---
fn get_ceilings(con: &mut redis::Connection) -> Result<Ceilings> {
    let key = "RSE^vmstat^ceilings";
    let hash: HashMap<String, u64> = con.hgetall(key).unwrap_or_default();
    let max_r = *hash.get("max-r").unwrap_or(&get_nproc());
    let max_cs = *hash.get("max-cs").unwrap_or(&15000);
    let max_in = *hash.get("max-in").unwrap_or(&15000);
    Ok(Ceilings { max_r, max_cs, max_in })
}

fn update_dynamic_ceilings(con: &mut redis::Connection, samples: &[VmstatSample], c: &mut Ceilings) -> Result<()> {
    let avg_id: f64 = samples.iter().map(|s| s.id).sum::<u64>() as f64 / samples.len() as f64;
    if avg_id < 20.0 { return Ok(()); }

    let key = "RSE^vmstat^ceilings";
    
    let mut check_update = |metric_name: &str, peak: u64, current_limit: u64| -> Option<u64> {
        if peak > current_limit {
            let new_limit = (peak as f64 * 1.2) as u64;
            let _ : redis::RedisResult<()> = con.hset(key, metric_name, new_limit);
            if VERBOSE { println!("Tuning: Raised {} from {} to {}", metric_name, current_limit, new_limit); }
            return Some(new_limit);
        }
        None
    };

    let peak_cs = samples.iter().map(|s| s.cs).max().unwrap_or(0);
    if let Some(new) = check_update("max-cs", peak_cs, c.max_cs) { c.max_cs = new; }

    let peak_in = samples.iter().map(|s| s.input_in).max().unwrap_or(0);
    if let Some(new) = check_update("max-in", peak_in, c.max_in) { c.max_in = new; }

    Ok(())
}

fn is_maintenance_imminent(drain_sec: i64) -> Result<bool> {
    let output = Command::new("rebootmgrctl").arg("get-window").output();
    if output.is_err() { return Ok(false); }
    let stdout = String::from_utf8(output.unwrap().stdout)?;
    let re = Regex::new(r"set to .+ (\d+:\d+:\d+) .+, lasting (\d+):(\d+)")?;
    if let Some(caps) = re.captures(&stdout) {
        let time_str = &caps[1];
        let dur_h: i64 = caps[2].parse()?;
        let dur_m: i64 = caps[3].parse()?;
        let now = Local::now();
        let start_time = NaiveTime::parse_from_str(time_str, "%H:%M:%S")?;
        let win_start = now.date_naive().and_time(start_time).and_local_timezone(Local).unwrap();
        let win_end = win_start + chrono::Duration::hours(dur_h) + chrono::Duration::minutes(dur_m);
        let drain_start = win_start - chrono::Duration::seconds(drain_sec);
        if now >= drain_start && now <= win_end { return Ok(true); }
    }
    Ok(false)
}

fn update_valkey(key: &str, member: &str, score: f64) -> Result<()> {
    let client = redis::Client::open(REDIS_HOST)?;
    let mut con = client.get_connection()?;
    let _ : () = con.zadd(key, member, score)?;
    Ok(())
}

fn get_nproc() -> u64 {
    let output = Command::new("nproc").output();
    if let Ok(o) = output { String::from_utf8_lossy(&o.stdout).trim().parse().unwrap_or(4) } else { 4 }
}

fn get_hostname() -> Result<String> {
    let output = Command::new("hostname").output()?;
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

fn clamp(v: f64, min: f64, max: f64) -> f64 { if v < min { min } else if v > max { max } else { v } }
