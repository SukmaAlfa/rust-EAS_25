use actix_cors::Cors;
use actix_files::Files;
use actix_web::{web, App, HttpServer, Responder, HttpResponse, get, post};
use serde::{Serialize, Deserialize};
use futures::stream::TryStreamExt;

use mongodb::{
    bson::{doc, oid::ObjectId},
    options::FindOptions,
    Client, Collection,
};

// --- Serial Port Imports ---
use tokio_serial::SerialPortBuilderExt;
use tokio::io::{AsyncBufReadExt as TokioAsyncBufReadExt, BufReader as TokioBufReader};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task;

// --- WebSocket & Actor Imports ---
use actix::{Actor, StreamHandler, AsyncContext, Addr, ActorContext};
use actix_web_actors::ws;
use std::collections::HashSet;
use tokio::sync::Mutex;
use std::sync::Arc;


#[derive(Serialize, Deserialize, Debug, Clone)]
struct LogEntry {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    id: Option<ObjectId>,
    timestamp: String,
    #[serde(rename = "type")]
    log_type: String, 
    description: String,
}

#[post("/api/log")]
async fn add_log_entry(
    collection: web::Data<Collection<LogEntry>>,
    mut new_entry: web::Json<LogEntry>,
) -> impl Responder {
    new_entry.id = None; 
    let result = collection.insert_one(new_entry.into_inner(), None).await;
    match result {
        Ok(res) => HttpResponse::Ok().json(res),
        Err(e) => HttpResponse::InternalServerError().body(e.to_string()),
    }
}

#[get("/api/logs")]
async fn get_log_entries(collection: web::Data<Collection<LogEntry>>) -> impl Responder {
    let find_options = FindOptions::builder()
        .sort(doc! { "_id": -1 })
        .limit(100)
        .build();

    match collection.find(None, find_options).await {
        Ok(mut cursor) => {
            let mut logs = Vec::new();
            while let Ok(Some(log)) = cursor.try_next().await {
                logs.push(log);
            }
            HttpResponse::Ok().json(logs)
        }
        Err(e) => HttpResponse::InternalServerError().body(e.to_string()),
    }
}

// --- WebSocket Actor (WsBroker) ---
struct WsBroker {
    sessions: Arc<Mutex<HashSet<Addr<WsConn>>>>,
}

impl Actor for WsBroker {
    type Context = actix::Context<Self>;
}

impl actix::Handler<Message> for WsBroker {
    type Result = ();

    fn handle(&mut self, msg: Message, _ctx: &mut Self::Context) { 
        if let Ok(sessions_guard) = self.sessions.try_lock() {
            for addr in sessions_guard.iter() {
                addr.do_send(msg.clone());
            }
        } else {
            eprintln!("WsBroker: Could not acquire lock for sessions, skipping broadcast.");
        }
    }
}

// Pesan yang akan dikirim antar Actor
#[derive(actix::Message, Clone)]
#[rtype(result = "()")]
struct Message(pub String);


// WebSocket Connection Actor (WsConn)
struct WsConn {
    broker_addr: Addr<WsBroker>, 
}

impl Actor for WsConn {
    type Context = ws::WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        let self_addr = ctx.address();
        let broker_addr_clone = self.broker_addr.clone();
        tokio::spawn(async move { 
            if let Err(e) = broker_addr_clone.send(Register(self_addr)).await {
                eprintln!("WsConn: Failed to send Register message to broker: {}", e);
            } else {
                println!("WsConn: Registered with broker.");
            }
        });
    }

    fn stopped(&mut self, ctx: &mut Self::Context) {
        let self_addr = ctx.address();
        let broker_addr_clone = self.broker_addr.clone();
        tokio::spawn(async move {
            if let Err(e) = broker_addr_clone.send(Unregister(self_addr)).await {
                eprintln!("WsConn: Failed to send Unregister message to broker: {}", e);
            } else {
                println!("WsConn: Unregistered from broker.");
            }
        });
    }
}

impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for WsConn {
    fn handle(&mut self, msg: Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        match msg {
            Ok(ws::Message::Ping(msg)) => ctx.pong(&msg),
            Ok(ws::Message::Text(text)) => {
                println!("WsConn: Received from client: {:?}", text);
            },
            Ok(ws::Message::Binary(bin)) => ctx.binary(bin),
            Ok(ws::Message::Close(reason)) => {
                println!("WsConn: Received Close message: {:?}", reason);
                ctx.close(reason);
                ctx.stop();
            },
            Err(e) => {
                eprintln!("WsConn: WebSocket Protocol Error: {}", e);
                ctx.stop();
            },
            _ => ctx.stop(),
        }
    }
}

impl actix::Handler<Message> for WsConn {
    type Result = ();

    fn handle(&mut self, msg: Message, ctx: &mut Self::Context) {
        ctx.text(msg.0);
    }
}

// Pesan untuk WsBroker untuk mendaftar sesi WsConn
#[derive(actix::Message)]
#[rtype(result = "()")]
struct Register(Addr<WsConn>);

impl actix::Handler<Register> for WsBroker {
    type Result = ();
    fn handle(&mut self, msg: Register, _ctx: &mut Self::Context) {
        let sessions_clone = self.sessions.clone(); 
        tokio::spawn(async move {
            let mut sessions_guard = sessions_clone.lock().await; 
            sessions_guard.insert(msg.0);
            println!("WsBroker: New WebSocket connection registered! Current active sessions: {}", sessions_guard.len());
        });
    }
}

// Pesan untuk WsBroker untuk membatalkan pendaftaran sesi WsConn
#[derive(actix::Message)]
#[rtype(result = "()")]
struct Unregister(Addr<WsConn>);

impl actix::Handler<Unregister> for WsBroker {
    type Result = ();
    fn handle(&mut self, msg: Unregister, _ctx: &mut Self::Context) {
        let sessions_clone = self.sessions.clone(); 
        tokio::spawn(async move {
            let mut sessions_guard = sessions_clone.lock().await; 
            sessions_guard.remove(&msg.0);
            println!("WsBroker: WebSocket connection unregistered! Current active sessions: {}", sessions_guard.len());
        });
    }
}

// WebSocket HTTP handler
async fn ws_route(
    req: actix_web::HttpRequest,
    stream: web::Payload,
    broker_data: web::Data<Addr<WsBroker>>, 
) -> Result<HttpResponse, actix_web::Error> {
    ws::start(WsConn { broker_addr: broker_data.get_ref().clone() }, &req, stream)
}


#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let mongo_uri = "mongodb://localhost:27017";

    let client = Client::with_uri_str(mongo_uri).await.expect("Gagal terhubung ke MongoDB");
    client.database("monitoring_db").run_command(doc! {"ping": 1}, None).await.expect("Gagal ping ke MongoDB monitoring_db"); 
    
    let db = client.database("monitoring_db");
    let log_collection: Collection<LogEntry> = db.collection("logs");
    
    println!("✅ Berhasil terhubung ke MongoDB!");
    println!("🚀 Server starting at http://localhost:8080");

    let broker_addr = WsBroker {
        sessions: Arc::new(Mutex::new(HashSet::new())),
    }.start();
    println!("WsBroker Actor started.");

    // Channel untuk mengirim data dari serial reader ke broker WebSocket
    let (tx_serial_data, rx_serial_data) = mpsc::channel::<String>(100);

    // --- Task pembaca serial port ---
    let broker_addr_for_serial_error = broker_addr.clone(); 
    task::spawn(async move {
        // Ganti dengan port serial Arduino Anda, misal "/dev/ttyACM0" di Linux, "COM3" di Windows
        let port_name = "COM16"; // SESUAIKAN DENGAN PORT ARDUINO ANDA!
        let baud_rate = 9600;

        println!("Serial: Attempting to open serial port {} at {} baud...", port_name, baud_rate);

        match tokio_serial::new(port_name, baud_rate)
            .timeout(Duration::from_millis(100)) 
            .open_native_async()
        {
            Ok(port) => {
                println!("Serial: Port {} opened successfully!", port_name);
                let mut reader = TokioBufReader::new(port); 
                let mut line = String::new();

                loop {
                    line.clear();
                    match reader.read_line(&mut line).await { 
                        Ok(0) => { 
                            eprintln!("Serial: End of stream, port closed unexpectedly.");
                            broker_addr_for_serial_error.do_send(Message("ERROR: Serial port closed.".to_string()));
                            break;
                        }
                        Ok(_n) => { 
                            let trimmed_line = line.trim();
                            if trimmed_line.starts_with("Moisture:") {
                                let data_str = trimmed_line.replace("Moisture:", "");
                                println!("Serial: Received from Arduino: {}", data_str); 
                                if let Err(e) = tx_serial_data.send(data_str.clone()).await { 
                                    eprintln!("Serial: Failed to send data to internal channel: {}", e);
                                    broker_addr_for_serial_error.do_send(Message(format!("ERROR: Failed to send data to channel: {}", e)));
                                    break; 
                                }
                            }
                        }
                        Err(e) => {
                            if e.kind() != std::io::ErrorKind::TimedOut {
                                eprintln!("Serial: Error reading from serial port: {}", e);
                                broker_addr_for_serial_error.do_send(Message(format!("ERROR: Error reading serial: {}", e)));
                                break; 
                            }
                            tokio::time::sleep(Duration::from_millis(100)).await; 
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("Serial: Failed to open serial port {}: {}", port_name, e);
                eprintln!("Serial: Pastikan Arduino terhubung dan driver terinstal, serta tidak digunakan oleh aplikasi lain.");
                broker_addr_for_serial_error.do_send(Message(format!("ERROR: Failed to open serial port: {}", e)));
            }
        }
    });
    println!("Serial port reader task spawned.");


    // Spawn task untuk mengirim data dari channel ke WebSocket broker
    let broker_addr_clone_for_rx = broker_addr.clone(); 
    task::spawn(async move {
        let mut rx_serial_data = rx_serial_data; 
        while let Some(data) = rx_serial_data.recv().await {
            broker_addr_clone_for_rx.do_send(Message(data));
        }
        println!("Channel: Receiver task finished."); 
    });
    println!("Channel receiver task spawned.");

    HttpServer::new(move || {
        App::new()
            .wrap(Cors::permissive())
            .app_data(web::Data::new(log_collection.clone())) 
            .app_data(web::Data::new(broker_addr.clone())) 
            .service(add_log_entry)
            .service(get_log_entries)
            .service(web::resource("/ws").to(ws_route)) 
            .service(Files::new("/", "./").index_file("index.html"))
    })
    .bind(("127.0.0.1", 8080))?
    .run()
    .await
}