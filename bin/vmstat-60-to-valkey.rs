// vmstat_60_to_valkey.rs
//
// Rust program that:
// - Runs `/usr/bin/vmstat -y -n -t 1` forever
// - Parses data lines into a JSON document (matching Raku field names)
// - Connects to Valkey/Redis using a hand-rolled RESP protocol (no external crates)
// - Creates a 60-element list initialized with zeros, then LSETs indexed by timestamp second
// - Reconnects to Valkey on failure
//
// Configuration via environment variables:
//   VALKEY_ADDR   -> default "172.19.2.254:6379"
//   VMSTAT_PATH   -> default "/usr/bin/vmstat"
//   HOSTNAME_CMD  -> default "hostname"
//
// Build (glibc, dynamic):
//   rustc -O vmstat_60_to_valkey.rs
//
// Build (static, musl target):
//   rustup target add x86_64-unknown-linux-musl
//   RUSTFLAGS="-C target-feature=+crt-static" rustc -O --target x86_64-unknown-linux-musl vmstat_60_to_valkey.rs
//
// Run:
//   ./vmstat_60_to_valkey
//
// Notes:
// - Designed for openSUSE MicroOS; uses external `/usr/bin/vmstat` and `hostname`.
// - No external Rust crates used.

use std::env;
use std::io::{self, BufRead, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

// Needed on Linux for ExitStatus::signal()
use std::os::unix::process::ExitStatusExt;

// ---------- Minimal RESP (Redis/Valkey) client ----------
#[allow(dead_code)]
enum Resp {
    SimpleString(String),           // +OK\r\n
    Error(String),                  // -ERR ...\r\n
    Integer(i64),                   // :123\r\n
    BulkString(Option<Vec<u8>>),    // $<len>\r\n<bytes>\r\n or $-1\r\n
    Array(Option<Vec<Resp>>),
}

struct Valkey {
    addr: String,
    stream: Option<TcpStream>,
}

impl Valkey {
    fn new(addr: String) -> Self {
        Valkey { addr, stream: None }
    }

    fn connect(&mut self) -> io::Result<()> {
        let mut last_err: Option<io::Error> = None;
        for _ in 0..3 {
            match self.addr.to_socket_addrs() {
                Ok(mut addrs) => {
                    if let Some(addr) = addrs.next() {
                        match TcpStream::connect(addr) {
                            Ok(stream) => {
                                let _ = stream.set_nodelay(true);
                                self.stream = Some(stream);
                                return Ok(());
                            }
                            Err(e) => {
                                last_err = Some(e);
                            }
                        }
                    } else {
                        last_err = Some(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "No socket addresses resolved",
                        ));
                    }
                }
                Err(e) => {
                    last_err = Some(e);
                }
            }
            thread::sleep(Duration::from_millis(250));
        }
        Err(last_err.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::Other, "Unknown connect error")
        }))
    }

    fn ensure_connected(&mut self) -> io::Result<()> {
        if self.stream.is_none() {
            self.connect()
        } else {
            Ok(())
        }
    }

    fn send_command(&mut self, args: &[&[u8]]) -> io::Result<Resp> {
        self.ensure_connected()?;

        // Build RESP array
        let mut buf: Vec<u8> = Vec::with_capacity(128);
        buf.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
        for a in args {
            buf.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
            buf.extend_from_slice(a);
            buf.extend_from_slice(b"\r\n");
        }

        // Write: let borrow end before modifying self.stream
        let write_result = {
            let stream = self.stream.as_mut().ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotConnected, "No stream")
            })?;
            stream.write_all(&buf)
        };

        if let Err(e) = write_result {
            eprintln!("[valkey] write failed: {e}. Reconnecting...");
            self.stream = None;
            self.ensure_connected()?;
            {
                let stream = self.stream.as_mut().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotConnected, "No stream after reconnect")
                })?;
                stream.write_all(&buf)?;
            }
        }

        // Read reply
        let stream = self.stream.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "No stream for read")
        })?;
        Self::read_resp(stream)
    }

    fn read_line_crlf(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
        let mut out: Vec<u8> = Vec::with_capacity(64);
        let mut byte = [0u8; 1];
        let mut last_was_cr = false;

        loop {
            let n = stream.read(&mut byte)?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "EOF"));
            }
            let b = byte[0];
            out.push(b);
            if last_was_cr && b == b'\n' {
                out.truncate(out.len() - 2); // strip CRLF
                return Ok(out);
            }
            last_was_cr = b == b'\r';
        }
    }

    fn read_exact_len(stream: &mut TcpStream, len: usize) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        let mut read_total = 0;
        while read_total < len {
            let n = stream.read(&mut buf[read_total..])?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "EOF"));
            }
            read_total += n;
        }
        Ok(buf)
    }

    fn read_resp(stream: &mut TcpStream) -> io::Result<Resp> {
        let mut lead = [0u8; 1];
        stream.read_exact(&mut lead)?;
        match lead[0] {
            b'+' => {
                let line = Self::read_line_crlf(stream)?;
                let s = String::from_utf8_lossy(&line).to_string();
                Ok(Resp::SimpleString(s))
            }
            b'-' => {
                let line = Self::read_line_crlf(stream)?;
                let s = String::from_utf8_lossy(&line).to_string();
                Ok(Resp::Error(s))
            }
            b':' => {
                let line = Self::read_line_crlf(stream)?;
                let s = String::from_utf8_lossy(&line);
                let val = s.parse::<i64>().unwrap_or(0);
                Ok(Resp::Integer(val))
            }
            b'$' => {
                let line = Self::read_line_crlf(stream)?;
                let s = String::from_utf8_lossy(&line);
                let len = s.parse::<isize>().unwrap_or(-1);
                if len < 0 {
                    Ok(Resp::BulkString(None))
                } else {
                    let data = Self::read_exact_len(stream, len as usize)?;
                    // consume trailing CRLF
                    let _ = Self::read_line_crlf(stream)?;
                    Ok(Resp::BulkString(Some(data)))
                }
            }
            b'*' => {
                let line = Self::read_line_crlf(stream)?;
                let s = String::from_utf8_lossy(&line);
                let count = s.parse::<isize>().unwrap_or(-1);
                if count < 0 {
                    Ok(Resp::Array(None))
                } else {
                    let mut elems: Vec<Resp> = Vec::with_capacity(count as usize);
                    for _ in 0..count {
                        elems.push(Self::read_resp(stream)?);
                    }
                    Ok(Resp::Array(Some(elems)))
                }
            }
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Unknown RESP lead byte: {other}"),
            )),
        }
    }

    // Convenience wrappers
    fn exists(&mut self, key: &str) -> io::Result<bool> {
        match self.send_command(&[b"EXISTS", key.as_bytes()])? {
            Resp::Integer(n) => Ok(n == 1),
            _ => Ok(false),
        }
    }

    fn del(&mut self, key: &str) -> io::Result<i64> {
        match self.send_command(&[b"DEL", key.as_bytes()])? {
            Resp::Integer(n) => Ok(n),
            _ => Ok(0),
        }
    }

    fn rpush_many(&mut self, key: &str, values: &[&str]) -> io::Result<i64> {
        let mut args: Vec<&[u8]> = Vec::with_capacity(2 + values.len());
        args.push(b"RPUSH");
        args.push(key.as_bytes());
        for v in values {
            args.push(v.as_bytes());
        }
        match self.send_command(&args)? {
            Resp::Integer(n) => Ok(n),
            _ => Ok(0),
        }
    }

    fn lset(&mut self, key: &str, index: i64, value: &[u8]) -> io::Result<bool> {
        let idx_s = index.to_string();
        match self.send_command(&[b"LSET", key.as_bytes(), idx_s.as_bytes(), value])? {
            Resp::SimpleString(s) => Ok(s == "OK"),
            Resp::Error(e) => {
                eprintln!("[valkey] LSET error: {e}");
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    #[allow(dead_code)]
    fn quit(&mut self) -> io::Result<()> {
        let _ = self.send_command(&[b"QUIT"])?;
        Ok(())
    }
}

// ---------- Utilities ----------

fn hostname() -> String {
    let cmd = env::var("HOSTNAME_CMD").unwrap_or_else(|_| "hostname".to_string());
    match Command::new(&cmd).stdout(Stdio::piped()).stderr(Stdio::null()).output() {
        Ok(out) => {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if s.is_empty() { "unknown-host".to_string() } else { s }
        }
        Err(_) => "unknown-host".to_string(),
    }
}

fn build_list_name(host: &str) -> String {
    format!("RSE^statistics^{host}^vmstat^rollingsixty")
}

// Escape minimal JSON for string values (date/time)
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out
}

// Parsed vmstat row -> JSON + index
struct ParsedVmstat {
    json: String,
    second: i64, // 0..59
}

fn parse_vmstat_line(line: &str) -> Option<ParsedVmstat> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || !trimmed.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }

    let toks: Vec<&str> = trimmed.split_whitespace().collect();
    if toks.len() < 20 {
        return None; // require 18 numeric + date + time
    }

    // Num fields = toks[0..18], date = toks[18], time = toks[19]
    let v_date = toks[18];
    let v_time = toks[19];

    // Parse seconds index from time "HH:MM:SS"
    let second = v_time.get(6..8).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
    let second = if (0..60).contains(&second) { second } else { 0 };

    // Build JSON with exact keys
    let keys = [
        "v-r","v-b","v-swpd","v-free","v-buff","v-cache","v-si","v-so","v-bi","v-bo",
        "v-in","v-cs","v-us","v-sy","v-id","v-wa","v-st","v-gu"
    ];

    let mut json = String::with_capacity(256);
    json.push('{');

    for i in 0..keys.len() {
        let val = toks[i].parse::<i64>().unwrap_or(0);
        if i > 0 { json.push(','); }
        json.push('"'); json.push_str(keys[i]); json.push_str("\":"); json.push_str(&val.to_string());
    }

    json.push(',');
    json.push_str("\"v-date\":\""); json.push_str(&json_escape(v_date)); json.push('"');
    json.push(',');
    json.push_str("\"v-time\":\""); json.push_str(&json_escape(v_time)); json.push('"');
    json.push('}');

    Some(ParsedVmstat { json, second })
}

// Initialize the rolling-60 list: delete existing, push 60 zeros
fn init_rolling_sixty(valkey: &mut Valkey, list_name: &str) -> io::Result<()> {
    if valkey.exists(list_name)? {
        let _ = valkey.del(list_name)?;
    }
    let zeros: Vec<String> = (0..60).map(|_| "0".to_string()).collect();
    let zero_refs: Vec<&str> = zeros.iter().map(|s| s.as_str()).collect();
    let len_after = valkey.rpush_many(list_name, &zero_refs)?;
    if len_after < 60 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("RPUSH length {} < 60", len_after),
        ));
    }
    Ok(())
}

fn main() {
    // Config
//  let valkey_addr = env::var("VALKEY_ADDR").unwrap_or_else(|_| "172.19.2.254:6379".to_string());
    let valkey_addr = env::var("VALKEY_ADDR").unwrap_or_else(|_| "valkey-vip.rse.local:6379".to_string());
    let vmstat_path = env::var("VMSTAT_PATH").unwrap_or_else(|_| "/usr/bin/vmstat".to_string());

    // Hostname & list name
    let host = hostname();
    let list_name = build_list_name(&host);

    eprintln!("[info] Hostname: {host}");
    eprintln!("[info] Valkey addr: {valkey_addr}");
    eprintln!("[info] List name: {list_name}");
    eprintln!("[info] vmstat path: {vmstat_path}");

    // Connect & initialize list
    let mut valkey = Valkey::new(valkey_addr.clone());
    loop {
        match valkey.connect() {
            Ok(()) => break,
            Err(e) => {
                eprintln!("[valkey] connect failed: {e}. Retrying in 2s...");
                thread::sleep(Duration::from_secs(2));
            }
        }
    }

    if let Err(e) = init_rolling_sixty(&mut valkey, &list_name) {
        eprintln!("[valkey] init rolling sixty failed: {e}. Retrying...");
        valkey.stream = None; // force reconnect
        if let Err(e2) = valkey.connect().and_then(|_| init_rolling_sixty(&mut valkey, &list_name)) {
            eprintln!("[fatal] cannot initialize list: {e2}");
            // Continue anyway; LSETs may fail until list exists
        }
    }

    // Run vmstat forever; restart on exit
    loop {
        eprintln!("[info] starting vmstat ...]");
        let mut child = match Command::new(&vmstat_path)
            .arg("-y").arg("-n").arg("-t").arg("1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[fatal] failed to spawn vmstat: {e}. Retrying in 5s...");
                thread::sleep(Duration::from_secs(5));
                continue;
            }
        };

        // Drain stderr in a thread
        if let Some(stderr) = child.stderr.take() {
            thread::spawn(move || {
                let mut buf = String::new();
                let mut reader = io::BufReader::new(stderr);
                loop {
                    buf.clear();
                    let n = reader.read_line(&mut buf).unwrap_or(0);
                    if n == 0 { break; }
                    let s = buf.trim_end();
                    if !s.is_empty() {
                        eprintln!("[vmstat][stderr] {s}");
                    }
                }
            });
        }

        // Read stdout
        if let Some(stdout) = child.stdout.take() {
            let mut reader = io::BufReader::new(stdout);
            let mut line = String::new();

            loop {
                line.clear();
                let n = reader.read_line(&mut line).unwrap_or(0);
                if n == 0 {
                    eprintln!("[warn] vmstat ended. Restarting in 2s...");
                    thread::sleep(Duration::from_secs(2));
                    break;
                }

                if let Some(parsed) = parse_vmstat_line(&line) {
                    let idx = parsed.second;
                    let value = parsed.json.as_bytes();
                    match valkey.lset(&list_name, idx, value) {
                        Ok(true) => { /* success */ }
                        Ok(false) => {
                            eprintln!("[valkey] LSET index {idx} failed. Attempting reconnect...");
                            valkey.stream = None;
                            if let Err(e) = valkey.connect() {
                                eprintln!("[valkey] reconnect failed: {e}");
                            } else {
                                let _ = valkey.lset(&list_name, idx, value);
                            }
                        }
                        Err(e) => {
                            eprintln!("[valkey] LSET error: {e}. Attempting reconnect...");
                            valkey.stream = None;
                            if let Err(e2) = valkey.connect() {
                                eprintln!("[valkey] reconnect failed: {e2}");
                            } else {
                                let _ = valkey.lset(&list_name, idx, value);
                            }
                        }
                    }
                }
            }
        }

        // Report exit status
        match child.wait() {
            Ok(status) => {
                if let Some(sig) = status.signal() {
                    eprintln!("[vmstat] killed by signal: {sig}");
                } else if let Some(code) = status.code() {
                    eprintln!("[vmstat] exited with code: {code}");
                } else {
                    eprintln!("[vmstat] ended unexpectedly");
                }
            }
            Err(e) => eprintln!("[vmstat] wait failed: {e}"),
        }
        // Loop restarts vmstat
    }

    // let _ = valkey.quit(); // unreachable
}
