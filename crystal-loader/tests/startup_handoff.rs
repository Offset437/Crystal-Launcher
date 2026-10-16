//! Runs the shipped reply reader in its own test process, away from unit
//! tests' native-owner globals. No game or network login is started; all
//! credentials are synthetic.

use crystal_loader::{b64, events, pipe::{random_pipe_path, LineReader, Pipe}, proto};
use std::{sync::mpsc, time::Duration};

#[test]
fn buffered_startup_sign_out_reaches_native_control_and_its_ready_writer() {
    let path = random_pipe_path();
    let server = Pipe::create_server(&path).unwrap();
    let (sent, written) = mpsc::channel();
    let writer = std::thread::spawn(move || {
        server.accept().unwrap();
        let session = format!("{{\"type\":\"session\",\"v\":1,\"region\":\"US\",\"lang\":\"US\",\"start_game\":false,\"native_auth\":{{\"pol_id\":\"{}\",\"pol_password\":\"{}\",\"se_id\":\"{}\",\"se_secret\":\"{}\",\"otp\":null,\"profile_key\":\"{}\"}}}}\n",
            b64::encode(b"ABCD1234"), b64::encode(b"fake password"), b64::encode(b"fakeuser"), b64::encode(&[0x51; 20]), "a".repeat(40));
        server.write_all(format!("{session}{{\"type\":\"native_sign_out\"}}\n").as_bytes()).unwrap();
        sent.send(()).unwrap();
        let mut replies = LineReader::default();
        assert_eq!(replies.read_line(&server, Some(Duration::from_secs(2))).unwrap(),
            b"{\"type\":\"native_control_result\",\"action\":\"sign_out\",\"code\":0}");
    });
    let client = Pipe::connect(&path, std::process::id(), Duration::from_secs(5)).unwrap();
    written.recv_timeout(Duration::from_secs(5)).unwrap();
    let mut reader = LineReader::default();
    assert!(matches!(reader.read_parsed(&client, Some(Duration::from_secs(2)), proto::parse_first_line).unwrap().unwrap(), proto::FirstLine::Session(_)));
    events::start(client, reader);
    let result = writer.join();
    events::finish(Duration::from_secs(2));
    result.unwrap();
}
