//! A gateway that is not a gateway.
//!
//! # Why hand-written rather than a framework
//!
//! The point of these tests is the **exact bytes** each adapter puts on the
//! wire — a field name, a header, whether a number is quoted. A framework
//! parses all of that away and then asserts against its own reconstruction,
//! which is the same mistake as testing a serializer with a deserializer.
//!
//! Lifted deliberately from `tax_sa::zatca::http`, where the same argument was
//! made about ZATCA. Not shared as a crate: it is thirty lines, and a test
//! helper that grows a dependency edge between two modules costs more than it
//! saves.

use std::net::SocketAddr;

/// Answers exactly one request, and reports what it was sent.
pub(crate) struct OneRequest {
    address: SocketAddr,
    handle: tokio::task::JoinHandle<String>,
}

impl OneRequest {
    /// Answers the first request with this status and body.
    pub(crate) async fn answering(status: u16, body: &'static str) -> Self {
        Self::sequence(vec![(status, body)]).await
    }

    /// Answers several requests in order, and returns all of them concatenated.
    ///
    /// Two are needed for anything that authenticates first: the token
    /// exchange, then the send.
    pub(crate) async fn sequence(answers: Vec<(u16, &'static str)>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binds");
        let address = listener.local_addr().expect("an address");

        let handle = tokio::spawn(async move {
            let mut seen = String::new();
            for (status, body) in answers {
                use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };

                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    let read = socket.read(&mut buffer).await.expect("reads");
                    request.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&request);
                    // Headers, then a body as long as `Content-Length` says.
                    if let Some(head) = text.find("\r\n\r\n") {
                        let length: usize = text
                            .to_lowercase()
                            .split("content-length:")
                            .nth(1)
                            .and_then(|rest| rest.split("\r\n").next())
                            .and_then(|value| value.trim().parse().ok())
                            .unwrap_or(0);
                        if request.len() >= head + 4 + length || read == 0 {
                            break;
                        }
                    }
                    if read == 0 {
                        break;
                    }
                }

                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.expect("writes");
                socket.flush().await.expect("flushes");

                seen.push_str(&String::from_utf8_lossy(&request));
                seen.push_str("\n===\n");
            }
            seen
        });

        Self { address, handle }
    }

    pub(crate) fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// Everything it was sent, requests separated by `===`.
    pub(crate) async fn seen(self) -> String {
        self.handle.await.expect("the server finished")
    }
}
