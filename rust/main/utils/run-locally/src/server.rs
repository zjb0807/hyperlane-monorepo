use std::io;

use reqwest::Url;

use crate::RELAYER_METRICS_PORT;

/// create tokio runtime to send a retry request to
/// relayer to retry all existing messages in the queues
pub fn run_retry_request() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();
    runtime.unwrap().block_on(async {
        for i in 0..100 {
            eprintln!("retry #{i}");
            let f1 = call_retry_request().await;
            println!("============================\nCalling Retry Request");
            eprintln!("~~~ RESs:\n{:#?}", f1);
        }
        eprintln!("Done\n============================\n");
    });
}

/// sends a request to relayer to retry all existing messages
/// in the queues
async fn call_retry_request() {
    let client = reqwest::Client::new();

    let url = Url::parse(&format!(
        "http://0.0.0.0:{RELAYER_METRICS_PORT}/message_retry"
    ))
    .map_err(|err| {
        eprintln!("Failed to parse url: {err}");
        io::Error::new(io::ErrorKind::InvalidInput, err.to_string())
    })
    .unwrap();

    let body = vec![serde_json::json!({
        "message_id": "*"
    })];
    let retry_response = client
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|err| {
            eprintln!("Failed to send request: {err}");
            io::Error::new(io::ErrorKind::InvalidData, err.to_string())
        })
        .unwrap();

    let response_text = retry_response
        .text()
        .await
        .map_err(|err| {
            eprintln!("Failed to parse response body: {err}");
            io::Error::new(io::ErrorKind::InvalidData, err.to_string())
        })
        .unwrap();

    println!("Retry Request Response: {:?}", response_text);
}
