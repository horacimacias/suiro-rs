use base64::engine::general_purpose;
use base64::Engine;
use futures::FutureExt;
use hyper::server::conn::Http;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Response, Server};
use serde_json::{Result as SerdeResult, Value};
use std::collections::HashMap;
use std::error::Error;
use std::io;
use std::result::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot::Sender;
use tokio::sync::{mpsc, oneshot, Mutex};
use unique_id::{string::StringGenerator, Generator};

struct Port {
    num: u16,
}

impl Port {
    fn new(port: u16) -> Self {
        if port < 1024 {
            println!("Port number must be greater than 1024 or run as root");
        }
        Port { num: port }
    }
}

#[derive(Debug)]
struct HttpResponse(String);
#[derive(Debug)]
struct HttpRequest(String);

type HttpRequestTask = (HttpRequest, Sender<HttpResponse>);

type Sessions = Arc<Mutex<HashMap<String, Arc<Session>>>>;

struct Session {
    session_id: String,
    session_endpoint: String,
    incoming_requests: mpsc::Sender<HttpRequestTask>,
}

impl Session {
    async fn add_request_task(&self, task: HttpRequestTask) -> Result<(), Box<dyn Error>> {
        self.incoming_requests.send(task).await?;
        Ok(())
    }
}

impl Session {
    fn new(
        session_id: String,
        session_endpoint: String,
        incoming_requests: mpsc::Sender<(HttpRequest, Sender<HttpResponse>)>,
    ) -> Self {
        Session {
            session_id,
            session_endpoint,
            incoming_requests,
        }
    }

    async fn tcp_connection_handler(
        &self,
        mut socket: TcpStream,
        mut rx: mpsc::Receiver<(HttpRequest, Sender<HttpResponse>)>,
    ) {
        let session_id = &self.session_id;
        let session_endpoint = &self.session_endpoint;
        println!("[TCP] New connection {session_id}: /{session_endpoint}");
        socket // write request to the agent socket
            .write_all(format!("connection\n{session_endpoint}").as_bytes())
            .await
            .unwrap(); // handle unwrap...
        let request_id_generator = StringGenerator;
        let mut pending_requests = HashMap::new();
        // TODO: do not loop between receiving data that needs to be sent to socket and receiving from socket that needs to be sent on http handlers; select instead of loop
        loop {
            // Write data to socket on request
            if let Some((request, reply)) = rx.recv().await {
                let request_id = request_id_generator.next_id();
                pending_requests.insert(request_id, reply);
                let raw_request = request.0;
                socket.write_all(raw_request.as_bytes()).await.unwrap();
            }

            match read_next_packet(&mut socket).await {
                Ok(packet) => {
                    let (request_id, response) = packet.into();
                    let reply = pending_requests.remove(&request_id);
                    match reply {
                        Some(reply) => {
                            reply.send(response).unwrap();
                        }
                        None => {
                            println!("[TCP] Error: no reply found for request_id {request_id}");
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[TCP] Error reading packet {e}");
                    break;
                }
            }
        }
    }
}

#[tokio::main]
async fn main() {
    println!("[SUIRO] Starting service");
    ctrlc::set_handler(move || {
        println!("[SUIRO] Stopping service");
        std::process::exit(0);
    })
    .expect("[SUIRO](ERROR) setting Ctrl-C handler");

    let http_port = Port::new(8080);
    let tcp_port = Port::new(3040);

    let mutex = Mutex::new(HashMap::new());
    let sessions = Arc::new(mutex);

    let tcp = async {
        tcp_server(tcp_port, sessions.clone())
            .await
            .expect("tcp server error");
    };

    let http = async {
        http_server(http_port, sessions.clone()).await;
    };

    futures::join!(tcp, http);
}

async fn tcp_server(port: Port, sessions: Sessions) -> io::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", port.num)).await.unwrap();
    println!("[TCP] Waiting connections on {}", port.num);
    let session_generator = StringGenerator;
    let session_endpoint_generator = StringGenerator;
    loop {
        let (socket, _addr) = listener.accept().await?;

        let session_id = session_generator.next_id();
        let session_endpoint = session_endpoint_generator.next_id();
        let (socket_tx, socket_rx) = mpsc::channel(100);
        let session = Session::new(session_id.clone(), session_endpoint, socket_tx);
        let session = Arc::new(session);
        {
            let mut locked_sessions = sessions.lock().await;
            if let Some(_old_value) = locked_sessions.insert(session_id.clone(), session.clone()) {
                println!("duplicated uuid generated!?: {session_id}");
            };
        }
        let task_sessions = sessions.clone();
        tokio::spawn(async move {
            // spawn a task for each inbound socket
            session.tcp_connection_handler(socket, socket_rx).await;
            // tcp connection loop finished; remove session from sessions otherwise it will be kept forever
            task_sessions.lock().await.remove(&session_id);
        });
    }
}

struct SuiroResponse {
    request_id: String,
    response: HttpResponse,
}

impl From<SuiroResponse> for (String, HttpResponse) {
    fn from(suiro_response: SuiroResponse) -> (String, HttpResponse) {
        (suiro_response.request_id, suiro_response.response)
    }
}

async fn read_next_packet(_socket: &mut TcpStream) -> io::Result<SuiroResponse> {
    todo!()
}

async fn http_server(port: Port, sessions: Sessions) {
    // The address we'll bind to.
    let addr = ([0, 0, 0, 0], port.num).into();

    // This is our service handler. It receives a Request, processes it, and returns a Response.
    let make_service = make_service_fn(|_conn| {
        let sessions_ref = Arc::clone(&sessions);
        async {
            Ok::<_, hyper::Error>(service_fn(move |req| {
                let sessions_clone = sessions_ref.clone();
                http_connection_handler(req, sessions_clone)
            }))
        }
    });

    let server = Server::bind(&addr).serve(make_service);
    println!("[HTTP] Waiting connections on {}", port.num);

    if let Err(e) = server.await {
        eprintln!("[HTTP] server error: {}", e);
    }
}

async fn http_connection_handler(
    request: hyper::Request<Body>,
    sessions: Sessions,
) -> Result<Response<Body>, hyper::Error> {
    let (session_endpoint, agent_request_path) = get_request_url(&request);
    let uri = request.uri().clone();
    let request_path = uri.path();
    println!("[HTTP] {request_path}");

    // avoid websocket
    if request.headers().contains_key("upgrade") {
        println!("[HTTP](ws) 403 Status on {}", request_path);
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

    let Some(session) = sessions.lock().await.get(&session_endpoint).cloned() else {
        // get access to hashmap - very dangerous

        let response = Response::builder()
            .status(404)
            .header("Content-type", "text/html")
            .body(Body::from("<h1>404 Not found</h1>"))
            .unwrap();
        return Ok(response);
    };

    // Create raw http from request
    // ----------------------------
    // headers
    let http_request_info = format!(
        "{} {} {:?}\n",
        request.method().as_str(),
        agent_request_path,
        request.version()
    );

    // Send raw http to tcp socket
    let (tx, rx) = oneshot::channel();
    let http_request = HttpRequest(http_request_info);
    let task = (http_request, tx);
    if let Err(err) = session.add_request_task(task).await {
        println!("Error adding request to session: {err}");
        let response = Response::builder()
            .status(500)
            .header("Content-type", "text/html")
            .body(Body::from("<h1>500 Internal server error</h1>"))
            .unwrap();
        return Ok(response);
    }

    // Check if response is ready
    let Ok(http_raw_response) = rx.await else {
        let response = Response::builder()
            .status(524)
            .header("Content-type", "text/html")
            .body(Body::from("<h1>524 A timeout error ocurred</h1>"))
            .unwrap();
        return Ok(response);
    };

    let http_response = http_raw_response.0;
    let http_response_result: SerdeResult<Value> = serde_json::from_str(http_response.as_str());
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
        println!("[HTTP] 570 Status on {}", session_endpoint);
        let response = Response::builder()
            .status(570)
            .header("Content-type", "text/html")
            .body(Body::from("<h1>570 Agent bad response</h1>"))
            .unwrap();
        return Ok(response);
    }

    if headers.keys().len() < 1 {
        println!("[HTTP] 570 Status on {}", session_endpoint);
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

    let response: Response<Body>;
    if body.is_some() {
        let _body = body.unwrap().to_string();
        let _body = general_purpose::STANDARD.decode(_body);
        let _body = match _body {
            Ok(_body) => match String::from_utf8(_body) {
                Ok(_body) => _body,
                Err(_) => {
                    println!("Error converting body");
                    let response = Response::builder()
                        .status(500)
                        .header("Content-type", "text/html")
                        .body(Body::from("<h1>500 Internal server error</h1>"))
                        .unwrap();
                    return Ok(response);
                }
            },
            Err(_) => {
                println!("Error decoding body");
                let response = Response::builder()
                    .status(500)
                    .header("Content-type", "text/html")
                    .body(Body::from("<h1>500 Internal server error</h1>"))
                    .unwrap();
                return Ok(response);
            }
        };
        let hyper_body = Body::from(_body);
        response = response_builder.body(hyper_body).unwrap();
    } else {
        response = response_builder.body(Body::empty()).unwrap();
    }

    println!("[HTTP] 200 Status on {}", request_path);
    Ok(response)
}

fn get_request_url(_req: &hyper::Request<Body>) -> (String, String) {
    let referer = _req.headers().get("referer");

    // [no referer]
    if referer.is_none() {
        let mut segments = _req.uri().path().split('/').collect::<Vec<&str>>(); // /abc/paco -> ["", "abc", "paco"]
        let session_endpoint = segments[1].to_string();
        segments.drain(0..2); // request path
        return (session_endpoint, "/".to_string() + &segments.join("/"));
    }

    let referer = referer.unwrap().to_str().unwrap(); // https://localhost:8080/abc/paco
    let mut referer = referer
        .split('/')
        .map(|r| r.to_string())
        .collect::<Vec<String>>();
    referer.drain(0..3); // drop -> "https:" + "" + "localhost:8080"

    let mut url = _req
        .uri()
        .path()
        .split('/')
        .map(|r| r.to_string())
        .filter(|r| !r.is_empty())
        .collect::<Vec<String>>();

    // [different session-endpoint]
    if referer[0] != url[0] {
        return (referer[0].clone(), "/".to_string() + &url.join("/"));
    }

    // [same session-endpoint]
    url.drain(0..1);
    (referer[0].clone(), "/".to_string() + &url.join("/"))
}

fn capitilize(string: &str) -> String {
    let segments = string.split("-");
    let mut result: Vec<String> = Vec::new();

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
