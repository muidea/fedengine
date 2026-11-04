use std::net::SocketAddr;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
pub async fn packet<S: tokio::io::AsyncWrite + Unpin>(stream: &mut S, tag: u8, body: &[u8]) {
    stream.write_u8(tag).await.unwrap();
    stream.write_i32((body.len() + 4) as i32).await.unwrap();
    stream.write_all(body).await.unwrap();
}
pub async fn message<S: tokio::io::AsyncRead + Unpin>(stream: &mut S) -> (u8, Vec<u8>) {
    let tag = stream.read_u8().await.unwrap();
    let n = stream.read_i32().await.unwrap();
    assert!((4..1_000_000).contains(&n));
    let mut b = vec![0; n as usize - 4];
    stream.read_exact(&mut b).await.unwrap();
    (tag, b)
}
pub async fn raw_start(address: SocketAddr, database: &str) -> TcpStream {
    let mut stream = TcpStream::connect(address).await.unwrap();
    authenticate(&mut stream, database).await;
    stream
}
pub async fn authenticate<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    stream: &mut S,
    database: &str,
) {
    authenticate_as(stream, "fedengine", Some(database)).await;
}
pub async fn authenticate_as<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    mut stream: &mut S,
    user: &str,
    database: Option<&str>,
) {
    use postgres_protocol::authentication::sasl::{ChannelBinding, ScramSha256};
    let mut body = format!("\0\x03\0\0user\0{user}\0").into_bytes();
    if let Some(database) = database {
        body.extend_from_slice(format!("database\0{database}\0").as_bytes());
    }
    body.push(0);
    stream.write_i32((body.len() + 4) as i32).await.unwrap();
    stream.write_all(&body).await.unwrap();
    let mut scram = ScramSha256::new(
        std::env::var("USER").unwrap().as_bytes(),
        ChannelBinding::unrequested(),
    );
    loop {
        let (tag, b) = message(&mut stream).await;
        match tag {
            b'R' => {
                let code = i32::from_be_bytes(b[..4].try_into().unwrap());
                match code {
                    10 => {
                        let mut p = b"SCRAM-SHA-256\0".to_vec();
                        p.extend_from_slice(&(scram.message().len() as i32).to_be_bytes());
                        p.extend_from_slice(scram.message());
                        packet(&mut stream, b'p', &p).await
                    }
                    11 => {
                        scram.update(&b[4..]).unwrap();
                        packet(&mut stream, b'p', scram.message()).await
                    }
                    12 => scram.finish(&b[4..]).unwrap(),
                    0 => {}
                    _ => panic!("unsupported authentication"),
                }
            }
            b'Z' => break,
            b'E' => panic!("auth error: {:?}", String::from_utf8_lossy(&b)),
            _ => {}
        }
    }
}
pub async fn until_boundary<S: tokio::io::AsyncRead + Unpin>(stream: &mut S) -> (Vec<String>, u8) {
    let mut rows = vec![];
    loop {
        let (tag, b) = message(stream).await;
        match tag {
            b'D' => {
                let n = i32::from_be_bytes(b[2..6].try_into().unwrap());
                rows.push(String::from_utf8(b[6..6 + n as usize].to_vec()).unwrap());
            }
            b's' | b'C' => return (rows, tag),
            b'E' => panic!("protocol error: {:?}", String::from_utf8_lossy(&b)),
            _ => {}
        }
    }
}
