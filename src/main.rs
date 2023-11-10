use base64::engine::general_purpose;
use base64::Engine;
use futures::lock::Mutex;
use futures::FutureExt;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Response, Server};
use serde_json::{Result as SerdeResult, Value};
use std::collections::HashMap;
use std::result::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use unique_id::{string::StringGenerator, Generator};

#[derive(Debug)]
struct Session {
    socket_tx: mpsc::Sender<String>,
    responses_rx: mpsc::Receiver<(String, String)>,
}

impl Session {
    fn new(
        socket_tx: mpsc::Sender<String>,
        responses_rx: mpsc::Receiver<(String, String)>,
    ) -> Self {
        Session {
            socket_tx,
            responses_rx,
        }
    }
}

type Sessions = Arc<Mutex<HashMap<String, Session>>>;

#[tokio::main]
async fn main() {
    println!("[SUIRO] Starting service");
    ctrlc::set_handler(move || {
        println!("[SUIRO] Stopping service");
        std::process::exit(0);
    })
    .expect("[SUIRO](ERROR) setting Ctrl-C handler");

    let http_port = 8080;
    let tcp_port = 3040;

    let sessions = Sessions::default();

    let sessions_tcp = sessions.clone();
    let tcp = async move {
        tcp_server(tcp_port, sessions_tcp).await;
    };

    let http = async move {
        http_server(http_port, sessions).await;
    };

    futures::join!(tcp, http);
}

async fn tcp_server(port: u16, sessions: Sessions) {
    let listener = TcpListener::bind(("0.0.0.0", port)).await.unwrap();
    println!("[TCP] Waiting connections on {port}");

    loop {
        let Ok((socket, _addr)) = listener.accept().await else {
            continue;
        };
        let sessions_clone = sessions.clone(); // avoid sessions_ref being moved

        tokio::spawn(async move {
            // spawn a task for each inbound socket
            tcp_connection_handler(socket, sessions_clone).await;
        });
    }
}

async fn tcp_connection_handler(mut socket: TcpStream, sessions: Sessions) {
    let session_id = StringGenerator.next_id();
    let session_endpoint = StringGenerator.next_id();

    println!("[TCP] New connection {session_id}: /{session_endpoint}");
    socket // write request to the agent socket
        .write_all(format!("connection\n{session_endpoint}").as_bytes())
        .await
        .unwrap(); // handle unwrap...

    // Add session to hashmap
    let hashmap_key = session_endpoint.clone();
    let (socket_tx, mut rx) = mpsc::channel(100); // 100 message queue
    let (tx, responses_rx) = mpsc::channel(100); // 100 message queue
    let session = Session::new(socket_tx, responses_rx);
    {
        sessions.lock().await.insert(hashmap_key, session); // create a block to avoid infinite lock
    }

    // Handle incoming data
    let mut packet_request_id = "".to_string();
    let mut packet_acc_data = "".to_string();
    let mut packet_total_size = 0;
    let mut packet_acc_size = 0;

    let mut buffer = [0; 31250]; // 32 Kb
    loop {
        // Write data to socket on request
        if let Some(Some(request)) = rx.recv().now_or_never() {
            socket.write_all(request.as_bytes()).await.unwrap();
        }

        if let Some(sock) = socket.read(&mut buffer).now_or_never() {
            match sock {
                Ok(0) => {
                    // connection closed
                    println!("[TCP] Connection closed: {session_id}");
                    break;
                }
                Ok(n) => {
                    // data received
                    let data = &buffer[..n];

                    // check packet integrity
                    let cur_packet_data = String::from_utf8(data.to_vec());
                    let cur_packet_data = match cur_packet_data {
                        Ok(cur_packet_data) => cur_packet_data,
                        Err(_) => {
                            eprintln!("[TCP] EPACKGRAG: Not valid utf8");
                            // Add data to responses hashmap
                            // TODO: if sending fails is because there is no receiver; this likely means we can even close the tcp connection
                            let _ = tx.send((packet_request_id, "EPACKFRAG".to_string())).await;

                            packet_acc_size = 0;
                            packet_total_size = 0;
                            packet_acc_data = "".to_string();
                            packet_request_id = "".to_string();

                            continue;
                        }
                    };

                    // Packet fragmentation?
                    if packet_request_id != *"" {
                        packet_acc_data = format!("{}{}", packet_acc_data, cur_packet_data);
                        packet_acc_size += cur_packet_data.as_bytes().len();

                        if packet_acc_size == packet_total_size {
                            println!("[TCP] Data on: {session_id}");

                            // Add data to responses hashmap
                            let _ = tx
                                .send((packet_request_id, packet_acc_data.to_string()))
                                .await;

                            packet_acc_size = 0;
                            packet_total_size = 0;
                            packet_acc_data = "".to_string();
                            packet_request_id = "".to_string();
                        }
                        continue;
                    }

                    let mut packet_split = cur_packet_data.split("\n\n\n");
                    let packet_header = packet_split.next().unwrap();
                    let packet_data = packet_split.next().unwrap();

                    let mut packet_header_split = packet_header.split(":::");
                    let request_id = packet_header_split.next().unwrap();
                    let packet_size = packet_header_split.next().unwrap();
                    let packet_size = packet_size.parse::<usize>().unwrap();

                    // First packet appear, is complete?
                    if packet_size == packet_data.as_bytes().len() {
                        println!("[TCP] Data on: {session_id}");

                        // Add data to responses hashmap
                        let _ = tx
                            .send((request_id.to_string(), packet_data.to_string()))
                            .await;
                    } else {
                        // Packet is not complete
                        packet_request_id = request_id.to_string();
                        packet_acc_data = packet_data.to_string();
                        packet_acc_size = packet_data.as_bytes().len();
                        packet_total_size = packet_size;
                    }
                }
                Err(e) => {
                    // error
                    eprintln!("[TCP] Error on socket connection: {session_id} \n\n {e}",);
                    break;
                }
            }
        }
    }
}

async fn http_server(port: u16, sessions: Sessions) {
    // The address we'll bind to.
    let addr = ([0, 0, 0, 0], port).into();

    // This is our service handler. It receives a Request, processes it, and returns a Response.
    let make_service = make_service_fn(|_conn| {
        let sessions_ref = sessions.clone();
        async {
            Ok::<_, hyper::Error>(service_fn(move |req| {
                let sessions_clone = sessions_ref.clone();
                http_connection_handler(req, sessions_clone)
            }))
        }
    });

    //let server = ;
    println!("[HTTP] Waiting connections on {port}");

    if let Err(e) = Server::bind(&addr).serve(make_service).await {
        eprintln!("[HTTP] server error: {e}");
    }
}

async fn http_connection_handler(
    req: hyper::Request<Body>,
    sessions: Sessions,
) -> Result<Response<Body>, hyper::Error> {
    // TODO: looks like we're having only 1 session per endpoint, are we not expecting browsers to send multiple parallel requests for the same session/endpoint?
    let (session_endpoint, agent_request_path) = get_request_url(&req);
    let request_path = req.uri().path().to_string();
    println!("[HTTP] {request_path}");

    // avoid websocket
    if req.headers().contains_key("upgrade") {
        println!("[HTTP](ws) 403 Status on {request_path}");
        let response = Response::builder()
            .status(404)
            .header("Content-type", "text/html")
            .body(Body::from("<h1>404 Not found</h1>"))
            .unwrap();
        return Ok(response);
    }

    if request_path == "/" {
        let response = Response::builder()
            .status(200)
            .header("Content-type", "text/html")
            .body(Body::from("<h1>Home</h1>"))
            .unwrap();
        return Ok(response);
    }

    let (socket_tx, mut responses_rx) = {
        let mut sessions = sessions.lock().await; // get access to hashmap - limit scope to the absolute minimum
                                                  // TODO: before you were getting (not removing) the session from sessions. Perhaps I'm not fully following the semanthics/ownership here but I think you should be removing the session from the hashmap, otherwise you're not really removing it and you're not allowing (or we're mixing) other requests with the same session
        let Some(session) = sessions.remove(session_endpoint.as_str()) else {
            let response = Response::builder()
                .status(404)
                .header("Content-type", "text/html")
                .body(Body::from("<h1>404 Not found</h1>"))
                .unwrap();
            return Ok(response);
        };
        (session.socket_tx.clone(), session.responses_rx)
    };

    // Create raw http from request
    // ----------------------------
    let request_id = StringGenerator.next_id();
    // headers
    let http_request_info = format!(
        "{} {} {:?}\n",
        req.method().as_str(),
        agent_request_path,
        req.version()
    );
    let mut request = request_id.clone() + "\n" + http_request_info.as_str();

    for (key, value) in req.headers() {
        match value.to_str() {
            Ok(value) => request += &format!("{}: {}\n", capitilize(key.as_str()), value),
            Err(_) => request += &format!("{}: {:?}\n", capitilize(key.as_str()), value.as_bytes()),
        }
    }

    // body
    let body = hyper::body::to_bytes(req.into_body()).await;
    if let Ok(body) = body {
        if !body.is_empty() {
            request += &format!("\n{}", String::from_utf8(body.to_vec()).unwrap());
        }
    }

    // Send raw http to tcp socket
    if let Err(err) = socket_tx.send(request).await {
        println!("[HTTP] 500 Status on {}: {err}", session_endpoint);
        let response = Response::builder()
            .status(500)
            .header("Content-type", "text/html")
            .body(Body::from("<h1>500 Internal server error</h1>"))
            .unwrap();
        return Ok(response);
    }

    // Wait for response
    let max_time = 100_000; // 100 seconds
    let mut time = 0;
    let mut http_raw_response = String::from("");
    loop {
        // Check if response is ready
        // TODO: look at tokio's timeout function which you can use to await something for a certain amount of time and avoid looping
        if let Some((agent_response_id, agent_response_body)) = responses_rx.recv().await {
            // TODO: when would request_id not be equal to agent_response_id? this sounds like we may be mixing connections/sessions and also related to concurrent endpoints/session comment above
            if request_id == agent_response_id {
                http_raw_response = agent_response_body;
                break;
            }
        }

        // Check if timeout
        if time >= max_time {
            break;
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
        time += 100;
    }

    if time >= max_time {
        let response = Response::builder()
            .status(524)
            .header("Content-type", "text/html")
            .body(Body::from("<h1>524 A timeout error ocurred</h1>"))
            .unwrap();
        return Ok(response);
    }

    // Check integrity of response [Packet fragmentation error]
    if http_raw_response == *"EPACKFRAG" {
        println!("[HTTP] 500 Status on {session_endpoint}");
        let response = Response::builder()
            .status(500)
            .header("Content-type", "text/html")
            .body(Body::from("<h1>500 Internal server error</h1>"))
            .unwrap();
        return Ok(response);
    }

    let http_response_result: SerdeResult<Value> = serde_json::from_str(http_raw_response.as_str());
    let http_response = http_response_result.unwrap();

    // Build response
    let status_code = match http_response["statusCode"].as_i64() {
        Some(status_code) => status_code as u16,
        None => 0,
    };
    let default_headers = serde_json::Map::new();
    let headers = match http_response["headers"].as_object() {
        Some(headers) => headers,
        None => &default_headers,
    };
    let body = http_response["body"].as_str();

    if status_code == 0 {
        println!("[HTTP] 570 Status on {session_endpoint}");
        let response = Response::builder()
            .status(570)
            .header("Content-type", "text/html")
            .body(Body::from("<h1>570 Agent bad response</h1>"))
            .unwrap();
        return Ok(response);
    }

    if headers.keys().len() < 1 {
        println!("[HTTP] 570 Status on {session_endpoint}");
        let response = Response::builder()
            .status(570)
            .header("Content-type", "text/html")
            .body(Body::from("<h1>570 Agent bad response</h1>"))
            .unwrap();
        return Ok(response);
    }

    let mut response_builder = Response::builder().status(status_code);
    for (key, value) in headers {
        if (key != "Content-Length") && (key != "content-length") {
            response_builder = response_builder.header(
                key,
                hyper::header::HeaderValue::from_str(value.as_str().unwrap()).unwrap(),
            );
        }
    }

    let response = if let Some(body) = body {
        let body = general_purpose::STANDARD.decode(body);
        let body = match body {
            Ok(body) => match String::from_utf8(body) {
                Ok(body) => body,
                Err(err) => {
                    println!("Error converting body: {err}");
                    let response = Response::builder()
                        .status(500)
                        .header("Content-type", "text/html")
                        .body(Body::from("<h1>500 Internal server error</h1>"))
                        .unwrap();
                    return Ok(response);
                }
            },
            Err(err) => {
                println!("Error decoding body: {err}");
                let response = Response::builder()
                    .status(500)
                    .header("Content-type", "text/html")
                    .body(Body::from("<h1>500 Internal server error</h1>"))
                    .unwrap();
                return Ok(response);
            }
        };
        let hyper_body = Body::from(body);
        response_builder.body(hyper_body).unwrap()
    } else {
        response_builder.body(Body::empty()).unwrap()
    };

    println!("[HTTP] 200 Status on {request_path}");
    Ok(response)
}

fn get_request_url(request: &hyper::Request<Body>) -> (String, String) {
    let referer = request.headers().get("referer");

    // [no referer]
    let Some(referer) = referer else {
        let mut segments = request.uri().path().split('/').collect::<Vec<&str>>(); // /abc/paco -> ["", "abc", "paco"]
        let session_endpoint = segments[1].to_string();
        segments.drain(0..2); // request path
        return (session_endpoint, "/".to_string() + &segments.join("/"));
    };

    let referer = referer.to_str().unwrap(); // https://localhost:8080/abc/paco
    let mut referer = referer.split('/').map(|r| r.to_string());

    let referer = referer.nth(4).unwrap_or_default(); // drop -> "https:" + "" + "localhost:8080"

    let mut url = request
        .uri()
        .path()
        .split('/')
        .map(|r| r.to_string())
        .filter(|r| !r.is_empty())
        .collect::<Vec<String>>();

    // [different session-endpoint]
    if referer != url[0] {
        return (referer, "/".to_string() + &url.join("/"));
    }

    // [same session-endpoint]
    url.drain(0..1);
    (referer, "/".to_string() + &url.join("/"))
}

fn capitilize(string: &str) -> String {
    let segments = string.split('-');
    let mut result = Vec::new();

    for segment in segments {
        let mut chars = segment.chars();
        let mut capitalized = String::new();
        if let Some(first_char) = chars.next() {
            capitalized.push(first_char.to_ascii_uppercase());
        }
        for c in chars {
            capitalized.push(c);
        }

        result.push(capitalized);
    }

    result.join("-")
}
