fn diagnostic_text(value: &str) -> String {
    value.chars().take(2048).map(|c| if c.is_control() { ' ' } else { c }).collect()
}

fn denial_identifier(body: &str) -> String {
    let pattern = regex::Regex::new(r#"(?is)<pre\s+class=["']ident["'][^>]*>\s*([0-9a-f]{8})\s+([0-9a-f]{32})"#).unwrap();
    pattern.captures(body).map(|m| format!("{} {}", &m[1], &m[2]))
        .unwrap_or_else(|| "not supplied".to_string())
}

#[cfg(test)]
mod http_diagnostics_tests {
    use super::*;
    use std::io::{Read, Write};

    #[tokio::test]
    async fn forbidden_probe_uses_configured_agent_and_preserves_failure() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            assert!(String::from_utf8_lossy(&request).to_lowercase()
                .contains("user-agent: synora-pypi-yukina"));
            stream.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 19\r\nX-Request-ID: test-denial\r\nConnection: close\r\n\r\nblocked: rate-limit").unwrap();
        });
        let client = reqwest::Client::builder().no_proxy().user_agent("Synora-pypi-yukina").build().unwrap();
        let mut response = client.get(format!("http://{address}/package.whl")).send().await.unwrap();
        report_http_failure("GET", "Synora-pypi-yukina", &mut response).await;
        assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
        assert_eq!(response.error_for_status().unwrap_err().status(), Some(reqwest::StatusCode::FORBIDDEN));
        server.join().unwrap();
    }

    #[test]
    fn diagnostic_output_is_bounded_and_has_no_terminal_controls() {
        assert_eq!(diagnostic_text("denied\n\u{1b}[31m"), "denied  [31m");
        assert_eq!(diagnostic_text(&"x".repeat(5000)).len(), 2048);
    }

    #[test]
    fn extracts_denial_code_after_long_multilingual_body() {
        let body = format!("{}<pre class=\"ident\">00000004 5355981b1656b4e0bde8b99dfb8ec1ab<!--footer--></pre>", "x".repeat(14000));
        assert_eq!(denial_identifier(&body), "00000004 5355981b1656b4e0bde8b99dfb8ec1ab");
        assert_eq!(denial_identifier("Forbidden"), "not supplied");
    }
}

async fn report_http_failure(method: &str, agent: &str, response: &mut reqwest::Response) {
    if !response.status().is_client_error() && !response.status().is_server_error() {
        return;
    }
    // Only allowlisted response headers: never log cookies or authorization.
    let names = ["server", "x-request-id", "x-tuna-mirror-id", "retry-after",
                 "x-tuna-block-reason", "cf-ray"];
    let headers: Vec<String> = names.iter().filter_map(|name| {
        response.headers().get(*name).and_then(|v| v.to_str().ok())
            .map(|v| format!("{}={}", name, diagnostic_text(v)))
    }).collect();
    let mut url = response.url().clone();
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    let status = response.status();
    let mut body = Vec::new();
    if method != "HEAD" {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while body.len() < 32768 {
                match response.chunk().await {
                    Ok(Some(chunk)) => {
                        let n = (32768 - body.len()).min(chunk.len());
                        body.extend_from_slice(&chunk[..n]);
                    }
                    _ => break,
                }
            }
        }).await;
    }
    let body = String::from_utf8_lossy(&body);
    let identifier = denial_identifier(&body);
    let reason = if body.is_empty() {
        "upstream supplied no response body; see response headers".to_string()
    } else {
        diagnostic_text(&body)
    };
    tracing::error!("HTTP sync rejected: method={} status={} url={} User-Agent={} headers=[{}] denial_identifier={} reason={}",
        method, status, url, diagnostic_text(agent), headers.join("; "), identifier, reason);
}
