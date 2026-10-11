//! Offline harness for crystal-loader: plays the launcher with DUMMY session
//! values, starts the loader in `--selftest-offline` mode (the lobby host name
//! resolves to 127.0.0.1), listens on loopback for the lobby connection, then
//! kills the game process. It never signs in and never contacts Square Enix.
//!
//! Usage: crystal-harness [--loader <pol.exe>] [--timeout <secs>]

use std::io::Read;
use std::net::{Ipv4Addr, TcpListener};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crystal_loader::pipe::{random_pipe_path, LineReader, Pipe, PipeError};
use crystal_loader::{b64, json, proto, registry};

/// FFXI lobby ports (view, auth, data). FFXiMain picks one itself.
const LOBBY_PORTS: [u16; 3] = [54001, 54230, 54231];

enum Event {
    Lobby { port: u16, bytes: usize, head: String },
    Line(String),
    PipeClosed,
    Fatal(String),
}

fn random_bytes(n: usize) -> Vec<u8> {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut out = Vec::with_capacity(n + 8);
    let mut i = 0u64;
    while out.len() < n {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(i);
        out.extend_from_slice(&h.finish().to_le_bytes());
        i += 1;
    }
    out.truncate(n);
    out
}

fn main() {
    let mut loader: Option<PathBuf> = None;
    let mut timeout = Duration::from_secs(90);
    let mut allow_display_change = false;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--loader" => loader = it.next().map(PathBuf::from),
            "--timeout" => timeout = Duration::from_secs(it.next().and_then(|s| s.parse().ok()).unwrap_or(90)),
            "--allow-display-change" => allow_display_change = true,
            _ => {}
        }
    }
    if !allow_display_change {
        eprintln!(
            "harness: refusing to start the game. It uses this machine's FFXI display settings \
             (FinalFantasyXI\\0034 etc.); in full-screen mode it switches the monitor's resolution, \
             and without an injector it cannot be forced windowed. Set windowed mode first, or pass \
             --allow-display-change on a machine where that is acceptable."
        );
        std::process::exit(2);
    }
    let loader = loader.unwrap_or_else(|| std::env::current_exe().unwrap().with_file_name("pol.exe"));
    if !loader.is_file() {
        eprintln!("harness: loader not found at {}", loader.display());
        std::process::exit(2);
    }
    std::process::exit(run(loader, timeout));
}

fn run(loader: PathBuf, timeout: Duration) -> i32 {
    let (tx, rx) = mpsc::channel::<Event>();

    // Loopback lobby listeners: where FFXiMain's lobby connection lands.
    for port in LOBBY_PORTS {
        match TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
            Ok(l) => {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    for conn in l.incoming() {
                        let Ok(mut s) = conn else { continue };
                        let _ = s.set_read_timeout(Some(Duration::from_secs(3)));
                        let mut buf = vec![0u8; 4096];
                        let n = s.read(&mut buf).unwrap_or(0);
                        let head = buf[..n.min(16)].iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ");
                        let _ = tx.send(Event::Lobby { port, bytes: n, head });
                    }
                });
                println!("harness: listening on 127.0.0.1:{port}");
            }
            Err(e) => println!("harness: cannot listen on 127.0.0.1:{port}: {e}"),
        }
    }

    let path = random_pipe_path();
    let server = match Pipe::create_server(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("harness: {e}");
            return 2;
        }
    };
    let name = path.trim_start_matches(r"\\.\pipe\").to_string();
    let mut child = match Command::new(&loader)
        .args(["--crystal-pipe", &name, "--crystal-launcher-pid", &std::process::id().to_string(), "--selftest-offline", "--lang", "US"])
        .stdin(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("harness: cannot start {}: {e}", loader.display());
            return 2;
        }
    };
    let child_pid = child.id();
    println!("harness: started {} (pid {child_pid}) with pipe {name}", loader.display());

    std::thread::spawn(move || watch_windows(child_pid));

    let install_dir = registry::install_folder(proto::Region::Us, "0001").map(|b| String::from_utf8_lossy(&b).into_owned());
    {
        let tx = tx.clone();
        std::thread::spawn(move || serve(server, child_pid, install_dir, tx));
    }

    let deadline = Instant::now() + timeout;
    let mut lobby_seen: Option<Instant> = None;
    let mut started = false;
    let mut presence = 0;
    let mut outcome = "timed out";
    loop {
        if let Some(t) = lobby_seen {
            if t.elapsed() > Duration::from_secs(3) {
                outcome = "lobby connection reached";
                break;
            }
        }
        if let Ok(Some(status)) = child.try_wait() {
            println!("harness: loader exited by itself with {status}");
            outcome = "loader exited";
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Event::Lobby { port, bytes, head }) => {
                println!("harness: LOBBY CONNECTION on 127.0.0.1:{port}, first read {bytes} bytes [{head}]");
                lobby_seen.get_or_insert_with(Instant::now);
            }
            Ok(Event::Line(l)) => {
                started |= l.contains("\"started\"");
                if l.contains("\"presence\"") {
                    presence += 1;
                    println!("harness: event presence args {}", describe_presence(&l));
                } else {
                    println!("harness: event {l}");
                }
            }
            Ok(Event::PipeClosed) => println!("harness: loader closed the pipe"),
            Ok(Event::Fatal(e)) => {
                println!("harness: {e}");
                outcome = "handshake failed";
                break;
            }
            Err(_) => {}
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    println!("harness: loader process {child_pid} terminated");
    println!("harness: RESULT: {outcome}; started={started}; presence events={presence}");
    println!("harness: loader log: {}", loader.with_file_name("crystal-loader.log").display());
    if lobby_seen.is_some() { 0 } else { 1 }
}

/// The launcher side of the contract, with dummy values.
fn serve(server: Pipe, child_pid: u32, install_dir: Option<String>, tx: mpsc::Sender<Event>) {
    let fatal = |m: String| {
        let _ = tx.send(Event::Fatal(m));
    };
    if let Err(e) = server.accept() {
        return fatal(format!("accept: {e}"));
    }
    match server.client_pid() {
        Ok(pid) if pid == child_pid => {}
        Ok(pid) => return fatal(format!("pipe client is pid {pid}, not the loader {child_pid}")),
        Err(e) => return fatal(format!("client pid: {e}")),
    }
    let mut reader = LineReader::default();
    match reader.read_line(&server, Some(Duration::from_secs(30))).map(proto::parse_hello) {
        Ok(Ok((1, pid))) if pid == child_pid => println!("harness: hello ok (v1, pid {pid})"),
        Ok(Ok((v, pid))) => return fatal(format!("bad hello v{v} pid {pid}")),
        Ok(Err(e)) => return fatal(format!("bad hello: {e}")),
        Err(e) => return fatal(format!("reading hello: {e}")),
    }
    // DUMMY session: random bytes, never a real sign-in.
    let v_key = random_bytes(proto::V_KEY_LEN);
    let auth = random_bytes(proto::AUTH_CODE_LEN);
    let dir = install_dir.map(|d| proto::json_string(&d)).unwrap_or_else(|| "null".into());
    let line = format!(
        "{{\"type\":\"session\",\"v\":1,\"region\":\"US\",\"lang\":\"US\",\"install_dir\":{dir},\"v_key\":\"{}\",\"auth_code\":\"{}\",\"pol_records\":null,\"content_ids\":[]}}\n",
        b64::encode(&v_key),
        b64::encode(&auth)
    );
    if let Err(e) = server.write_all(line.as_bytes()) {
        return fatal(format!("sending session: {e}"));
    }
    println!("harness: sent dummy session (random V/authCode)");
    loop {
        match reader.read_line(&server, None) {
            Ok(l) => {
                let text = String::from_utf8_lossy(l).into_owned();
                // Stand-in for Authenticated::send_presence: acknowledge without sending anything.
                if text.contains("\"presence\"") {
                    let reply = proto::presence_result_line(&proto::PresenceResult::Ok { changed: true });
                    if let Err(e) = server.write_all(reply.as_bytes()) {
                        return fatal(format!("answering presence: {e}"));
                    }
                }
                let _ = tx.send(Event::Line(text));
            }
            Err(PipeError::Closed) => {
                let _ = tx.send(Event::PipeClosed);
                return;
            }
            Err(e) => return fatal(format!("reading events: {e}")),
        }
    }
}

/// Reports the game's top-level windows as they appear (observation only).
fn watch_windows(pid: u32) {
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GetClassNameW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible};

    unsafe extern "system" fn collect(hwnd: HWND, lp: LPARAM) -> i32 {
        let (pid, out) = &mut *(lp as *mut (u32, Vec<HWND>));
        let mut owner = 0;
        GetWindowThreadProcessId(hwnd, &mut owner);
        if owner == *pid {
            out.push(hwnd);
        }
        1
    }
    let text = |hwnd: HWND, class: bool| -> String {
        let mut buf = [0u16; 128];
        // SAFETY: buf is valid for its length.
        let n = unsafe { if class { GetClassNameW(hwnd, buf.as_mut_ptr(), 128) } else { GetWindowTextW(hwnd, buf.as_mut_ptr(), 128) } };
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    };
    let mut known: Vec<(usize, bool)> = Vec::new();
    loop {
        let mut state: (u32, Vec<HWND>) = (pid, Vec::new());
        // SAFETY: the callback only touches `state` for the duration of the call.
        unsafe { EnumWindows(Some(collect), &mut state as *mut _ as LPARAM) };
        for hwnd in state.1 {
            // SAFETY: plain window queries on a window that may vanish (calls then fail harmlessly).
            let visible = unsafe { IsWindowVisible(hwnd) } != 0;
            let key = hwnd as usize;
            match known.iter_mut().find(|(h, _)| *h == key) {
                Some((_, v)) if *v == visible => continue,
                Some((_, v)) => *v = visible,
                None => known.push((key, visible)),
            }
            println!("harness: game window {key:#x} class \"{}\" title \"{}\" visible={visible}", text(hwnd, true), text(hwnd, false));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn describe_presence(line: &str) -> String {
    let Ok(v) = json::parse(line.as_bytes()) else { return "(bad json)".into() };
    let Some(raw) = v.get("payload").and_then(json::Value::as_jstr).and_then(|s| s.plain()) else { return "(no payload)".into() };
    let mut p = [0u8; 24];
    if b64::decode_into(raw, &mut p).is_err() {
        return "(payload is not 24 bytes)".into();
    }
    let a: Vec<i32> = p.chunks(4).map(|c| i32::from_le_bytes(c.try_into().unwrap())).collect();
    format!("handle={} chr={} mode={} openstat={} friendauth={} class={}", a[0], a[1], a[2], a[3], a[4], a[5])
}
