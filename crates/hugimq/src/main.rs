use std::collections::HashMap;
use std::net::UdpSocket;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let socket = UdpSocket::bind("0.0.0.0:6379")?;
    let mut directory: HashMap<String, String> = HashMap::new();
    let mut buf = [0u8; 1024];

    println!("HugiMQ MDC Directory Server listening on 0.0.0.0:6379");

    loop {
        let (amt, src) = socket.recv_from(&mut buf)?;
        let msg = String::from_utf8_lossy(&buf[..amt]);
        let parts: Vec<&str> = msg.split_whitespace().collect();

        if parts.len() < 2 { continue; }

        match parts[0] {
            "PUB" => {
                if parts.len() == 3 {
                    let topic = parts[1].to_string();
                    let uri = parts[2].to_string();
                    println!("Registered {} at {}", topic, uri);
                    directory.insert(topic, uri);
                    socket.send_to(b"OK", src)?;
                }
            }
            "SUB" => {
                let topic = parts[1];
                if let Some(uri) = directory.get(topic) {
                    socket.send_to(uri.as_bytes(), src)?;
                } else {
                    socket.send_to(b"NOT_FOUND", src)?;
                }
            }
            _ => {
                socket.send_to(b"UNKNOWN_COMMAND", src)?;
            }
        }
    }
}
