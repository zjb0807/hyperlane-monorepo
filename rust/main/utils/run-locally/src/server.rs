use std::io;

use reqwest::Url;

use relayer::server::MessageRetryResponse;

use crate::RELAYER_METRICS_PORT;

/// create tokio runtime to send a retry request to
/// relayer to retry all existing messages in the queues
pub fn run_retry_request() -> io::Result<MessageRetryResponse> {
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
        Ok(MessageRetryResponse {
            uuid: "0".to_string(),
            evaluated: 100,
            matched: 100,
        })
    })
}

/// sends a request to relayer to retry all existing messages
/// in the queues
async fn call_retry_request() -> io::Result<MessageRetryResponse> {
    let client = reqwest::Client::new();

    let url = Url::parse(&format!(
        "http://0.0.0.0:{RELAYER_METRICS_PORT}/message_retry"
    ))
    .map_err(|err| {
        println!("Failed to parse url: {err}");
        io::Error::new(io::ErrorKind::InvalidInput, err.to_string())
    })?;

    let body = vec![serde_json::json!({
        "message_id": "*"
    })];
    let retry_response = client.post(url).json(&body).send().await.map_err(|err| {
        println!("Failed to send request: {err}");
        io::Error::new(io::ErrorKind::InvalidData, err.to_string())
    })?;

    let response_text = retry_response.text().await.map_err(|err| {
        println!("Failed to parse response body: {err}");
        io::Error::new(io::ErrorKind::InvalidData, err.to_string())
    })?;

    println!("Retry Request Response: {:?}", response_text);

    let response_json: MessageRetryResponse =
        serde_json::from_str(&response_text).map_err(|err| {
            println!("Failed to parse response body to json: {err}");
            io::Error::new(io::ErrorKind::InvalidData, err.to_string())
        })?;

    Ok(response_json)
}
