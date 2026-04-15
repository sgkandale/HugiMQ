mod protocol;
mod ring_buffer;
mod server;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let listen_addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:6380".to_string());

    server::run_server(&listen_addr).await?;
    Ok(())
}
