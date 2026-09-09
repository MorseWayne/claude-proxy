use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::{Duration, timeout};

#[tokio::test]
async fn dropping_responses_consumer_closes_idle_upstream_socket() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let upstream = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let length = socket.read(&mut buffer).await.unwrap();
            assert_ne!(length, 0);
            request.extend_from_slice(&buffer[..length]);
        }
        let event = "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-cancel\",\"output\":[]}}\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n{:x}\r\n{event}\r\n",
            event.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.read(&mut buffer).await.unwrap()
    });
    let response = reqwest::Client::new()
        .get(format!("http://{address}/"))
        .send()
        .await
        .unwrap();
    let mut stream =
        stream_native_responses_response_with_provider_observer(response, None, 1024 * 1024);
    let event = timeout(Duration::from_secs(2), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(event.native_event().is_some());
    drop(stream);
    assert_eq!(
        timeout(Duration::from_secs(2), upstream)
            .await
            .expect("dropping the consumer must close an idle upstream")
            .unwrap(),
        0
    );
}
