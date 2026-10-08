use srun_portal::config::{parse_portal_target, PortalTarget};

#[test]
fn recognizes_srun_and_drcom_urls() {
    assert!(matches!(
        parse_portal_target("https://net.szu.edu.cn/srun_portal_pc?ac_id=1"),
        Ok(PortalTarget::Srun(_))
    ));
    assert!(matches!(
        parse_portal_target("http://172.30.255.42/"),
        Ok(PortalTarget::Drcom(_))
    ));
}

#[test]
fn rejects_unknown_root_path() {
    assert!(parse_portal_target("http://172.30.255.42/not-a-portal").is_err());
}

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;

use serde_json::json;
use srun_portal::drcom::DrcomClient;

struct Stub {
    origin: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Stub {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let online = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let state = Arc::clone(&online);
        let log = Arc::clone(&requests);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                serve(stream, Arc::clone(&state), Arc::clone(&log), port);
            }
        });
        Self {
            origin: format!("http://127.0.0.1:{port}"),
            requests,
        }
    }
}

fn serve(
    mut stream: TcpStream,
    online: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<String>>>,
    port: u16,
) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut line = String::new();
    reader.read_line(&mut line).expect("request");
    let request_line = line.clone();
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }
    let mut parts = request_line.split_whitespace();
    let _method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    requests
        .lock()
        .expect("log")
        .push(format!("{path}?{query}"));
    let params = parse_query(query);
    let body = match path {
        "/" => format!("v4ip='127.0.0.1';v6ip='';authloginport={port};"),
        "/eportal/portal/page/loadConfig" => jsonp(
            &params,
            json!({"code":1,"data":{"login_method":"1","account_prefix":"1","account_suffix":"","check_online_method":"0","cvlan_id":"4095","ac_logout":"0","register_mode":"1","ipv6_state":"0","ep_http_port":port.to_string()}}),
        ),
        "/drcom/chkstatus" => jsonp(
            &params,
            json!({"result": online.load(Ordering::SeqCst) as u8}),
        ),
        "/eportal/portal/login" => {
            online.store(true, Ordering::SeqCst);
            jsonp(&params, json!({"result": 1}))
        }
        "/eportal/portal/logout" => {
            online.store(false, Ordering::SeqCst);
            jsonp(&params, json!({"result": "ok"}))
        }
        _ => jsonp(&params, json!({"result": 0, "msg": "not found"})),
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/javascript\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).expect("write");
}

fn jsonp(params: &std::collections::HashMap<String, String>, value: serde_json::Value) -> String {
    format!(
        "{}({value});",
        params
            .get("callback")
            .map(String::as_str)
            .unwrap_or("dr1001")
    )
}

fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    query
        .split('&')
        .filter_map(|part| part.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

#[test]
fn drcom_client_logs_in_and_out() {
    let stub = Stub::start();
    let PortalTarget::Drcom(portal) = parse_portal_target(&stub.origin).expect("drcom url") else {
        panic!("expected DrCOM URL");
    };
    let client = DrcomClient::new(&portal).expect("client");
    assert!(!client.check_online().expect("offline check"));
    client.login("testuser", "testpass", "").expect("login");
    assert!(client.check_online().expect("online check"));
    client.logout().expect("logout");
    assert!(!client.check_online().expect("offline check"));
    let target = srun_portal::reconnect::Target {
        portal: PortalTarget::Drcom(portal.clone()),
        username: "testuser".to_string(),
        domain: "@office".to_string(),
        callback: "jsonp".to_string(),
    };
    assert_eq!(
        srun_portal::reconnect::reconnect_once(&target, "testpass").expect("reconnect"),
        srun_portal::reconnect::Outcome::Reconnected
    );
    assert_eq!(
        srun_portal::reconnect::reconnect_once(&target, "testpass").expect("online"),
        srun_portal::reconnect::Outcome::AlreadyOnline
    );

    let requests = stub.requests.lock().expect("log");
    assert!(requests
        .iter()
        .any(|request| request.starts_with("/eportal/portal/login?")));
    assert!(requests
        .iter()
        .any(|request| request.starts_with("/eportal/portal/logout?")));
    assert!(requests
        .iter()
        .any(|request| request.contains("user_account=%2C0%2Ctestuser")));
    assert!(requests
        .iter()
        .any(|request| request.contains("user_password=testpass")));
}
