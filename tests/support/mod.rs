//! The stub portal the integration tests share.
//!
//! It speaks the same three shapes the real portal does — an HTML configuration
//! page, JSONP `/cgi-bin` answers, plain JSON `/v1`/`/v2` answers — and records
//! the raw query string of every request, which is what the wire assertions
//! compare. Two properties matter for the background tests:
//!
//! * the listener stops on drop (the accept thread is woken and joined), so a
//!   suite that starts several stubs leaves no thread behind;
//! * behaviour is selectable: already online, login marks online, login always
//!   answers `ip_already_online_error`, or every answer is delayed.
//!
//! Nothing here is compiled into the library.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::json;

pub const CHALLENGE: &str = "0123456789abcdef0123456789abcdef";
pub const IP: &str = "10.9.9.9";
pub const MAC: &str = "aa:bb:cc:dd:ee:ff";

/// How the stub answers.
#[derive(Debug, Clone, Default)]
pub struct Behaviour {
    /// Start with the device already online.
    pub online: bool,
    /// A successful `login` marks the device online.
    pub login_marks_online: bool,
    /// Every `login` answer is `ip_already_online_error`, for the bounded retry.
    pub login_conflicts: bool,
    /// Delay every answer this long, for slow-response tests.
    pub delay: Option<Duration>,
    /// Send the body this slowly, one byte per interval, so no single read
    /// exceeds the HTTP read timeout even though the whole response does. That
    /// is the case only the supervisor's deadline can catch.
    pub dribble: Option<Duration>,
}

impl Behaviour {
    pub fn offline() -> Behaviour {
        Behaviour {
            login_marks_online: true,
            ..Behaviour::default()
        }
    }

    pub fn already_online() -> Behaviour {
        Behaviour {
            online: true,
            ..Behaviour::offline()
        }
    }

    pub fn conflicting() -> Behaviour {
        Behaviour {
            login_conflicts: true,
            ..Behaviour::offline()
        }
    }
}

pub struct Stub {
    pub origin: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl Stub {
    pub fn start(behaviour: Behaviour) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
        let addr = listener.local_addr().expect("stub addr");
        listener
            .set_nonblocking(true)
            .expect("a non-blocking listener can be stopped");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let online = Arc::new(AtomicBool::new(behaviour.online));
        let stop = Arc::new(AtomicBool::new(false));
        let (log, state, flag) = (
            Arc::clone(&requests),
            Arc::clone(&online),
            Arc::clone(&stop),
        );
        let behaviour = behaviour.clone();
        let accept = thread::spawn(move || {
            while !flag.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let (log, state, behaviour) =
                            (Arc::clone(&log), Arc::clone(&state), behaviour.clone());
                        thread::spawn(move || serve(stream, log, state, behaviour));
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Stub {
            origin: format!("http://{addr}"),
            requests,
            stop,
            accept: Some(accept),
        }
    }

    pub fn slug(&self) -> String {
        format!("{}/srun_portal_pc?ac_id=1", self.origin)
    }

    pub fn queries(&self) -> Vec<String> {
        self.requests.lock().expect("log").clone()
    }

    pub fn request_count(&self) -> usize {
        self.queries().len()
    }

    pub fn queries_for(&self, path: &str) -> Vec<String> {
        self.queries()
            .into_iter()
            .filter(|line| line.starts_with(&format!("{path} ")))
            .filter_map(|line| line.split_once(' ').map(|(_, query)| query.to_string()))
            .collect()
    }

    pub fn count_for(&self, path: &str) -> usize {
        self.queries_for(path).len()
    }

    pub fn last_query_for(&self, path: &str) -> Option<String> {
        self.queries_for(path).pop()
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.accept.take() {
            let _ = handle.join();
        }
    }
}

fn page() -> String {
    let portal = json!({
        "AuthIP": "127.0.0.1",
        "AuthIP6": "127.0.0.1",
        "ServiceIP": "",
        "DoubleStackPC": false,
        "DoubleStackMobile": false,
        "MacAuth": false,
        "TrafficCarry": 1024,
        "AccountFilter": ""
    });
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>stub</title></head><body>\n\
         <span id=\"cliVersion\">1.9.15</span>\n\
         <span id=\"acid\">\"1\"</span>\n\
         <span id=\"ip\">\"{IP}\"</span>\n\
         <span id=\"nas\">\"stub-nas\"</span>\n\
         <span id=\"mac\">\"{MAC}\"</span>\n\
         <span id=\"lang\">\"en-US\"</span>\n\
         <span id=\"isIPv6\">false</span>\n\
         <span id=\"domain\">{{}}</span>\n\
         <span id=\"portal\">{portal}</span>\n\
         <span id=\"project\">stub</span>\n\
         <span id=\"color\">#1976D2</span>\n\
         <span id=\"useLogo\">false</span>\n\
         <span id=\"showInfoList\">username,ip</span>\n\
         </body></html>\n"
    )
}

fn jsonp(params: &HashMap<String, String>, value: serde_json::Value) -> String {
    let callback = params
        .get("callback")
        .map(String::as_str)
        .unwrap_or("jsonp");
    format!("{callback}({value})")
}

fn serve(
    mut stream: TcpStream,
    log: Arc<Mutex<Vec<String>>>,
    online: Arc<AtomicBool>,
    behaviour: Behaviour,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    loop {
        let mut header = String::new();
        match reader.read_line(&mut header) {
            Ok(0) => break,
            Ok(_) if header == "\r\n" || header == "\n" => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let mut parts = request_line.split_whitespace();
    let _method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    log.lock().expect("log").push(format!("{path} {query}"));
    let params = parse_query(query);

    if let Some(delay) = behaviour.delay {
        thread::sleep(delay);
    }

    let body = match path {
        "/srun_portal_pc" => page(),
        "/cgi-bin/get_challenge" => jsonp(&params, json!({"error": "ok", "challenge": CHALLENGE})),
        "/cgi-bin/rad_user_info" => {
            let is_online = online.load(Ordering::SeqCst);
            let mut info = json!({
                "error": if is_online { "ok" } else { "auth" },
                "online_ip": IP,
                "sysver": "1.9.15"
            });
            if is_online {
                info["user_name"] = json!("testuser");
                info["domain"] = json!("");
                info["real_name"] = json!("Test User");
                info["user_mac"] = json!(MAC);
            }
            jsonp(&params, info)
        }
        "/cgi-bin/srun_portal" => {
            if params.get("action").map(String::as_str) == Some("login") {
                if behaviour.login_conflicts {
                    jsonp(
                        &params,
                        json!({"error": "ok", "suc_msg": "ip_already_online_error"}),
                    )
                } else {
                    if behaviour.login_marks_online {
                        online.store(true, Ordering::SeqCst);
                    }
                    jsonp(&params, json!({"error": "ok"}))
                }
            } else {
                jsonp(&params, json!({"error": "ok"}))
            }
        }
        "/cgi-bin/rad_user_dm" => {
            online.store(false, Ordering::SeqCst);
            jsonp(&params, json!({"error": "ok"}))
        }
        "/v2/srun_portal_message" => json!({"code": 0, "data": []}).to_string(),
        "/v1/srun_portal_agree_new" => {
            json!({"code": 0, "data": {"data": {"id": 1, "title": "t", "content": "c"}}})
                .to_string()
        }
        _ => "not found".to_string(),
    };

    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/javascript\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    match behaviour.dribble {
        Some(interval) => {
            for byte in body.as_bytes() {
                if stream.write_all(&[*byte]).is_err() {
                    return;
                }
                let _ = stream.flush();
                thread::sleep(interval);
            }
        }
        None => {
            let _ = stream.write_all(body.as_bytes());
        }
    }
    let _ = stream.flush();
}

pub fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(pair), String::new()),
        })
        .collect()
}

pub fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match u8::from_str_radix(&value[i + 1..i + 3], 16) {
                Ok(byte) => {
                    out.push(byte);
                    i += 3;
                }
                Err(_) => {
                    out.push(bytes[i]);
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A uniquely named scratch directory, removed on drop.
pub struct TempDir {
    pub path: std::path::PathBuf,
}

impl TempDir {
    pub fn new(tag: &str) -> TempDir {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "srun-portal-it-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("scratch dir");
        TempDir { path }
    }

    pub fn join(&self, name: &str) -> std::path::PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
