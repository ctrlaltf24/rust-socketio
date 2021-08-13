pub use super::{event::Event, payload::Payload};
use crate::error::Error;
use native_tls::TlsConnector;
pub use reqwest::header::{HeaderMap, HeaderValue, IntoHeaderName};

use crate::engineio::socket::SocketBuilder as EngineIoSocketBuilder;
use crate::engineio::{
    packet::{Packet as EnginePacket, PacketId as EnginePacketId},
    socket::Socket as EngineIoSocket,
};
use crate::error::Result;
use crate::socketio::packet::{Packet as SocketPacket, PacketId as SocketPacketId};
use bytes::Bytes;
use rand::{thread_rng, Rng};
use std::convert::TryFrom;
use std::thread;
use std::{
    fmt::Debug,
    sync::{atomic::Ordering, RwLock},
};
use std::{
    sync::{atomic::AtomicBool, Arc},
    time::{Duration, Instant},
};
use url::Url;

/// The type of a callback function.
pub(crate) type Callback<I> = RwLock<Box<dyn FnMut(I, Socket) + 'static + Sync + Send>>;

pub(crate) type EventCallback = (Event, Callback<Payload>);

/// Represents an `Ack` as given back to the caller. Holds the internal `id` as
/// well as the current ack'ed state. Holds data which will be accessible as
/// soon as the ack'ed state is set to true. An `Ack` that didn't get ack'ed
/// won't contain data.
#[derive(Clone)]
pub struct Ack {
    pub id: i32,
    timeout: Duration,
    time_started: Instant,
    callback: Arc<Callback<Payload>>,
}

/// A socket which handles communication with the server. It's initialized with
/// a specific address as well as an optional namespace to connect to. If `None`
/// is given the server will connect to the default namespace `"/"`.
#[derive(Clone)]
pub struct Socket {
    engine_socket: Arc<RwLock<EngineIoSocket>>,
    host: Arc<Url>,
    connected: Arc<AtomicBool>,
    on: Arc<Vec<EventCallback>>,
    outstanding_acks: Arc<RwLock<Vec<Ack>>>,
    // used to detect unfinished binary events as, as the attachments
    // gets send in a separate packet
    unfinished_packet: Arc<RwLock<Option<SocketPacket>>>,
    // namespace, for multiplexing messages
    pub(crate) nsp: Arc<Option<String>>,
}

type SocketCallback = dyn FnMut(Payload, Socket) + 'static + Sync + Send;

/// A builder class for a `socket.io` socket. This handles setting up the client and
/// configuring the callback, the namespace and metadata of the socket. If no
/// namespace is specified, the default namespace `/` is taken. The `connect` method
/// acts the `build` method and returns a connected [`Socket`].
pub struct SocketBuilder {
    address: String,
    on: Option<Vec<(Event, Box<SocketCallback>)>>,
    namespace: Option<String>,
    tls_config: Option<TlsConnector>,
    opening_headers: Option<HeaderMap>,
}

impl SocketBuilder {
    /// Create as client builder from a URL. URLs must be in the form
    /// `[ws or wss or http or https]://[domain]:[port]/[path]`. The
    /// path of the URL is optional and if no port is given, port 80
    /// will be used.
    /// # Example
    /// ```rust
    /// use rust_socketio::{SocketBuilder, Payload};
    /// use serde_json::json;
    ///
    ///
    /// let callback = |payload: Payload, _| {
    ///            match payload {
    ///                Payload::String(str) => println!("Received: {}", str),
    ///                Payload::Binary(bin_data) => println!("Received bytes: {:#?}", bin_data),
    ///            }
    /// };
    ///
    /// let mut socket = SocketBuilder::new("http://localhost:4200")
    ///     .namespace("/admin")
    ///     .expect("illegal namespace")
    ///     .on("test", callback)
    ///     .connect()
    ///     .expect("error while connecting");
    ///
    /// // use the socket
    /// let json_payload = json!({"token": 123});
    ///
    /// let result = socket.emit("foo", json_payload);
    ///
    /// assert!(result.is_ok());
    /// ```
    pub fn new<T: Into<String>>(address: T) -> Self {
        Self {
            address: address.into(),
            on: None,
            namespace: None,
            tls_config: None,
            opening_headers: None,
        }
    }

    /// Sets the target namespace of the client. The namespace should start
    /// with a leading `/`. Valid examples are e.g. `/admin`, `/foo`.
    pub fn namespace<T: Into<String>>(mut self, namespace: T) -> Result<Self> {
        let mut nsp = namespace.into();
        if !nsp.starts_with('/') {
            nsp = "/".to_owned() + &nsp;
        }
        self.namespace = Some(nsp);
        Ok(self)
    }

    /// Registers a new callback for a certain [`socketio::event::Event`]. The event could either be
    /// one of the common events like `message`, `error`, `connect`, `close` or a custom
    /// event defined by a string, e.g. `onPayment` or `foo`.
    /// # Example
    /// ```rust
    /// use rust_socketio::{SocketBuilder, Payload};
    ///
    /// let callback = |payload: Payload, _| {
    ///            match payload {
    ///                Payload::String(str) => println!("Received: {}", str),
    ///                Payload::Binary(bin_data) => println!("Received bytes: {:#?}", bin_data),
    ///            }
    /// };
    ///
    /// let socket = SocketBuilder::new("http://localhost:4200/")
    ///     .namespace("/admin")
    ///     .expect("illegal namespace")
    ///     .on("test", callback)
    ///     .on("error", |err, _| eprintln!("Error: {:#?}", err))
    ///     .connect();
    ///
    /// ```
    pub fn on<F>(mut self, event: &str, callback: F) -> Self
    where
        F: FnMut(Payload, Socket) + 'static + Sync + Send,
    {
        match self.on {
            Some(ref mut vector) => vector.push((event.into(), Box::new(callback))),
            None => self.on = Some(vec![(event.into(), Box::new(callback))]),
        }
        self
    }

    /// Uses a preconfigured TLS connector for secure cummunication. This configures
    /// both the `polling` as well as the `websocket` transport type.
    /// # Example
    /// ```rust
    /// use rust_socketio::{SocketBuilder, Payload};
    /// use native_tls::TlsConnector;
    ///
    /// let tls_connector =  TlsConnector::builder()
    ///            .use_sni(true)
    ///            .build()
    ///            .expect("Found illegal configuration");
    ///
    /// let socket = SocketBuilder::new("http://localhost:4200/")
    ///     .namespace("/admin")
    ///     .expect("illegal namespace")
    ///     .on("error", |err, _| eprintln!("Error: {:#?}", err))
    ///     .tls_config(tls_connector)
    ///     .connect();
    ///
    /// ```
    pub fn tls_config(mut self, tls_config: TlsConnector) -> Self {
        self.tls_config = Some(tls_config);
        self
    }

    /// Sets custom http headers for the opening request. The headers will be passed to the underlying
    /// transport type (either websockets or polling) and then get passed with every request thats made.
    /// via the transport layer.
    /// # Example
    /// ```rust
    /// use rust_socketio::{SocketBuilder, Payload};
    /// use reqwest::header::{ACCEPT_ENCODING};
    ///
    ///
    /// let socket = SocketBuilder::new("http://localhost:4200/")
    ///     .namespace("/admin")
    ///     .expect("illegal namespace")
    ///     .on("error", |err, _| eprintln!("Error: {:#?}", err))
    ///     .opening_header(ACCEPT_ENCODING, "application/json".parse().unwrap())
    ///     .connect();
    ///
    /// ```
    pub fn opening_header<K: IntoHeaderName>(mut self, key: K, val: HeaderValue) -> Self {
        match self.opening_headers {
            Some(ref mut map) => {
                map.insert(key, val);
            }
            None => {
                let mut map = HeaderMap::new();
                map.insert(key, val);
                self.opening_headers = Some(map);
            }
        }
        self
    }

    /// Connects the socket to a certain endpoint. This returns a connected
    /// [`Socket`] instance. This method returns an [`std::result::Result::Err`]
    /// value if something goes wrong during connection.
    /// # Example
    /// ```rust
    /// use rust_socketio::{SocketBuilder, Payload};
    /// use serde_json::json;
    ///
    ///
    /// let mut socket = SocketBuilder::new("http://localhost:4200/")
    ///     .namespace("/admin")
    ///     .expect("illegal namespace")
    ///     .on("error", |err, _| eprintln!("Socket error!: {:#?}", err))
    ///     .connect()
    ///     .expect("connection failed");
    ///
    /// // use the socket
    /// let json_payload = json!({"token": 123});
    ///
    /// let result = socket.emit("foo", json_payload);
    ///
    /// assert!(result.is_ok());
    /// ```
    pub fn connect(self) -> Result<Socket> {
        //TODO: Add check for empty path and set to /socket.io/
        let mut socket = Socket::new(
            self.address,
            self.namespace,
            self.tls_config,
            self.opening_headers,
        )?;
        if let Some(callbacks) = self.on {
            for (event, callback) in callbacks {
                socket.on(event, Box::new(callback)).unwrap();
            }
        }
        socket.connect()?;
        Ok(socket)
    }
}

impl Socket {
    /// Creates a socket with a certain address to connect to as well as a
    /// namespace. If `None` is passed in as namespace, the default namespace
    /// `"/"` is taken.
    pub fn new<T: Into<String>>(
        address: T,
        nsp: Option<String>,
        tls_config: Option<TlsConnector>,
        opening_headers: Option<HeaderMap>,
    ) -> Result<Self> {
        let mut url: Url = Url::parse(&address.into())?;

        if url.path() == "/" {
            url.set_path("/socket.io/");
        }

        let mut engine_socket_builder = EngineIoSocketBuilder::new(url.clone());
        if let Some(tls_config) = tls_config {
            // SAFETY: Checked is_some
            engine_socket_builder = engine_socket_builder.set_tls_config(tls_config);
        }
        if let Some(opening_headers) = opening_headers {
            // SAFETY: Checked is_some
            engine_socket_builder = engine_socket_builder.set_headers(opening_headers);
        }
        Ok(Socket {
            engine_socket: Arc::new(RwLock::new(engine_socket_builder.build_with_fallback()?)),
            host: Arc::new(url),
            connected: Arc::new(AtomicBool::default()),
            on: Arc::new(Vec::new()),
            outstanding_acks: Arc::new(RwLock::new(Vec::new())),
            unfinished_packet: Arc::new(RwLock::new(None)),
            nsp: Arc::new(nsp),
        })
    }

    /// Registers a new callback for a certain event. This returns an
    /// `Error::IllegalActionAfterOpen` error if the callback is registered
    /// after a call to the `connect` method.
    pub fn on<F, T: Into<Event>>(&mut self, event: T, callback: Box<F>) -> Result<()>
    where
        F: FnMut(Payload, Socket) + 'static + Sync + Send,
    {
        Arc::get_mut(&mut self.on)
            .unwrap()
            .push((event.into(), RwLock::new(callback)));
        Ok(())
    }

    /// Connects the client to a server. Afterwards the `emit_*` methods can be
    /// called to interact with the server. Attention: it's not allowed to add a
    /// callback after a call to this method.
    pub fn connect(&mut self) -> Result<()> {
        self.setup_callbacks()?;

        self.engine_socket.write()?.connect()?;

        // TODO: refactor me
        let engine_socket = self.engine_socket.clone();
        thread::spawn(move || {
            // tries to restart a poll cycle whenever a 'normal' error occurs,
            // it just panics on network errors, in case the poll cycle returned
            // `Result::Ok`, the server receives a close frame so it's safe to
            // terminate
            loop {
                match engine_socket.read().unwrap().poll_cycle() {
                    Ok(_) => break,
                    e @ Err(Error::IncompleteHttp(_))
                    | e @ Err(Error::IncompleteResponseFromReqwest(_)) => {
                        panic!("{}", e.unwrap_err())
                    }
                    _ => (),
                }
            }
        });

        // construct the opening packet
        let open_packet = SocketPacket::new(
            SocketPacketId::Connect,
            self.nsp
                .as_ref()
                .as_ref()
                .unwrap_or(&String::from("/"))
                .to_owned(),
            None,
            None,
            None,
            None,
        );

        // store the connected value as true, if the connection process fails
        // later, the value will be updated
        self.connected.store(true, Ordering::Release);
        self.send(&open_packet)
    }

    /// Sends a message to the server using the underlying `engine.io` protocol.
    /// This message takes an event, which could either be one of the common
    /// events like "message" or "error" or a custom event like "foo". But be
    /// careful, the data string needs to be valid JSON. It's recommended to use
    /// a library like `serde_json` to serialize the data properly.
    ///
    /// # Example
    /// ```
    /// use rust_socketio::{SocketBuilder, Payload};
    /// use serde_json::json;
    ///
    /// let mut socket = SocketBuilder::new("http://localhost:4200/")
    ///     .on("test", |payload: Payload, mut socket| {
    ///         println!("Received: {:#?}", payload);
    ///         socket.emit("test", json!({"hello": true})).expect("Server unreachable");
    ///      })
    ///     .connect()
    ///     .expect("connection failed");
    ///
    /// let json_payload = json!({"token": 123});
    ///
    /// let result = socket.emit("foo", json_payload);
    ///
    /// assert!(result.is_ok());
    /// ```
    pub fn emit<T: Into<Event>, U: Into<Payload>>(&self, event: T, data: U) -> Result<()> {
        let default = String::from("/");
        let nsp = self.nsp.as_ref().as_ref().unwrap_or(&default);

        let data = data.into();

        let is_string_packet = matches!(&data, &Payload::String(_));
        let socket_packet = self.build_packet_for_payload(data, event.into(), nsp, None)?;

        if is_string_packet {
            self.send(&socket_packet)
        } else {
            // first send the raw packet announcing the attachment
            self.send(&socket_packet)?;

            // then send the attachment
            // unwrapping here is safe as this is a binary payload
            self.send_binary_attachment(socket_packet.binary_data.unwrap())
        }
    }

    /// Disconnects from the server by sending a socket.io `Disconnect` packet. This results
    /// in the underlying engine.io transport to get closed as well.
    /// # Example
    /// ```rust
    /// use rust_socketio::{SocketBuilder, Payload};
    /// use serde_json::json;
    ///
    /// let mut socket = SocketBuilder::new("http://localhost:4200/")
    ///     .on("test", |payload: Payload, mut socket| {
    ///         println!("Received: {:#?}", payload);
    ///         socket.emit("test", json!({"hello": true})).expect("Server unreachable");
    ///      })
    ///     .connect()
    ///     .expect("connection failed");
    ///
    /// let json_payload = json!({"token": 123});
    ///
    /// socket.emit("foo", json_payload);
    ///
    /// // disconnect from the server
    /// socket.disconnect();
    ///
    /// ```
    pub fn disconnect(&mut self) -> Result<()> {
        if !self.is_engineio_connected()? || !self.connected.load(Ordering::Acquire) {
            return Err(Error::IllegalActionAfterOpen());
        }

        let disconnect_packet = SocketPacket::new(
            SocketPacketId::Disconnect,
            self.nsp
                .as_ref()
                .as_ref()
                .unwrap_or(&String::from("/"))
                .to_owned(),
            None,
            None,
            None,
            None,
        );

        self.send(&disconnect_packet)?;
        self.connected.store(false, Ordering::Release);
        Ok(())
    }

    /// Sends a message to the server but `alloc`s an `ack` to check whether the
    /// server responded in a given timespan. This message takes an event, which
    /// could either be one of the common events like "message" or "error" or a
    /// custom event like "foo", as well as a data parameter. But be careful,
    /// in case you send a [`Payload::String`], the string needs to be valid JSON.
    /// It's even recommended to use a library like serde_json to serialize the data properly.
    /// It also requires a timeout `Duration` in which the client needs to answer.
    /// If the ack is acked in the correct timespan, the specified callback is
    /// called. The callback consumes a [`Payload`] which represents the data send
    /// by the server.
    ///
    /// # Example
    /// ```
    /// use rust_socketio::{SocketBuilder, Payload};
    /// use serde_json::json;
    /// use std::time::Duration;
    /// use std::thread::sleep;
    ///
    /// let mut socket = SocketBuilder::new("http://localhost:4200/")
    ///     .on("foo", |payload: Payload, _| println!("Received: {:#?}", payload))
    ///     .connect()
    ///     .expect("connection failed");
    ///
    ///
    ///
    /// let ack_callback = |message: Payload, _| {
    ///     match message {
    ///         Payload::String(str) => println!("{}", str),
    ///         Payload::Binary(bytes) => println!("Received bytes: {:#?}", bytes),
    ///    }    
    /// };
    ///
    /// let payload = json!({"token": 123});
    /// socket.emit_with_ack("foo", payload, Duration::from_secs(2), ack_callback).unwrap();
    ///
    /// sleep(Duration::from_secs(2));
    /// ```
    pub fn emit_with_ack<F, T: Into<Event>, U: Into<Payload>>(
        &mut self,
        event: T,
        data: U,
        timeout: Duration,
        callback: F,
    ) -> Result<()>
    where
        F: FnMut(Payload, Socket) + 'static + Send + Sync,
    {
        let id = thread_rng().gen_range(0..999);
        let default = String::from("/");
        let nsp = self.nsp.as_ref().as_ref().unwrap_or(&default);
        let socket_packet =
            self.build_packet_for_payload(data.into(), event.into(), nsp, Some(id))?;

        let ack = Ack {
            id,
            time_started: Instant::now(),
            timeout,
            callback: Arc::new(RwLock::new(Box::new(callback))),
        };

        // add the ack to the tuple of outstanding acks
        self.outstanding_acks.write()?.push(ack);

        self.send(&socket_packet)?;
        Ok(())
    }

    /// Sends a `socket.io` packet to the server using the `engine.io` client.
    fn send(&self, packet: &SocketPacket) -> Result<()> {
        if !self.is_engineio_connected()? || !self.connected.load(Ordering::Acquire) {
            return Err(Error::IllegalActionBeforeOpen());
        }

        // the packet, encoded as an engine.io message packet
        let engine_packet = EnginePacket::new(EnginePacketId::Message, Bytes::from(packet));

        self.engine_socket.read()?.emit(engine_packet, false)
    }

    /// Sends a single binary attachment to the server. This method
    /// should only be called if a `BinaryEvent` or `BinaryAck` was
    /// send to the server mentioning this attachment in it's
    /// `attachments` field.
    fn send_binary_attachment(&self, attachment: Bytes) -> Result<()> {
        if !self.is_engineio_connected()? || !self.connected.load(Ordering::Acquire) {
            return Err(Error::IllegalActionBeforeOpen());
        }

        self.engine_socket
            .read()?
            .emit(EnginePacket::new(EnginePacketId::Message, attachment), true)
    }
    /// Returns a packet for a payload, could be used for bot binary and non binary
    /// events and acks.
    #[inline]
    fn build_packet_for_payload<'a>(
        &'a self,
        payload: Payload,
        event: Event,
        nsp: &'a str,
        id: Option<i32>,
    ) -> Result<SocketPacket> {
        match payload {
            Payload::Binary(bin_data) => Ok(SocketPacket::new(
                if id.is_some() {
                    SocketPacketId::BinaryAck
                } else {
                    SocketPacketId::BinaryEvent
                },
                nsp.to_owned(),
                Some(serde_json::Value::String(event.into()).to_string()),
                Some(bin_data),
                id,
                Some(1),
            )),
            Payload::String(str_data) => {
                serde_json::from_str::<serde_json::Value>(&str_data)?;

                let payload = format!("[\"{}\",{}]", String::from(event), str_data);

                Ok(SocketPacket::new(
                    SocketPacketId::Event,
                    nsp.to_owned(),
                    Some(payload),
                    None,
                    id,
                    None,
                ))
            }
        }
    }

    /// Handles the incoming messages and classifies what callbacks to call and how.
    /// This method is later registered as the callback for the `on_data` event of the
    /// engineio client.
    #[inline]
    fn handle_new_message(socket_bytes: Bytes, clone_self: &Socket) {
        let mut is_finalized_packet = false;
        // either this is a complete packet or the rest of a binary packet (as attachments are
        // sent in a separate packet).
        let decoded_packet = if clone_self.unfinished_packet.read().unwrap().is_some() {
            // this must be an attachement, so parse it
            let mut unfinished_packet = clone_self.unfinished_packet.write().unwrap();
            let mut finalized_packet = unfinished_packet.take().unwrap();
            finalized_packet.binary_data = Some(socket_bytes);

            is_finalized_packet = true;
            Ok(finalized_packet)
        } else {
            // this is a normal packet, so decode it
            SocketPacket::try_from(&socket_bytes)
        };

        if let Ok(socket_packet) = decoded_packet {
            let default = String::from("/");
            if socket_packet.nsp != *clone_self.nsp.as_ref().as_ref().unwrap_or(&default) {
                return;
            }

            match socket_packet.packet_type {
                SocketPacketId::Connect => {
                    clone_self.connected.store(true, Ordering::Release);
                }
                SocketPacketId::ConnectError => {
                    clone_self.connected.store(false, Ordering::Release);
                    if let Some(function) = clone_self.get_event_callback(&Event::Error) {
                        spawn_scoped!({
                            let mut lock = function.1.write().unwrap();
                            let socket = clone_self.clone();
                            lock(
                                Payload::String(
                                    String::from("Received an ConnectError frame")
                                        + &socket_packet.data.unwrap_or_else(|| {
                                            String::from("\"No error message provided\"")
                                        }),
                                ),
                                socket,
                            );
                            drop(lock);
                        });
                    }
                }
                SocketPacketId::Disconnect => {
                    clone_self.connected.store(false, Ordering::Release);
                }
                SocketPacketId::Event => {
                    Socket::handle_event(socket_packet, clone_self);
                }
                SocketPacketId::Ack => {
                    Self::handle_ack(socket_packet, clone_self);
                }
                SocketPacketId::BinaryEvent => {
                    // in case of a binary event, check if this is the attachement or not and
                    // then either handle the event or set the open packet
                    if is_finalized_packet {
                        Self::handle_binary_event(socket_packet, clone_self);
                    } else {
                        *clone_self.unfinished_packet.write().unwrap() = Some(socket_packet);
                    }
                }
                SocketPacketId::BinaryAck => {
                    if is_finalized_packet {
                        Self::handle_ack(socket_packet, clone_self);
                    } else {
                        *clone_self.unfinished_packet.write().unwrap() = Some(socket_packet);
                    }
                }
            }
        }
    }

    /// Handles the incoming acks and classifies what callbacks to call and how.
    #[inline]
    fn handle_ack(socket_packet: SocketPacket, clone_self: &Socket) {
        let mut to_be_removed = Vec::new();
        if let Some(id) = socket_packet.id {
            for (index, ack) in clone_self
                .clone()
                .outstanding_acks
                .read()
                .unwrap()
                .iter()
                .enumerate()
            {
                if ack.id == id {
                    to_be_removed.push(index);

                    if ack.time_started.elapsed() < ack.timeout {
                        if let Some(ref payload) = socket_packet.data {
                            spawn_scoped!({
                                let mut function = ack.callback.write().unwrap();
                                let socket = clone_self.clone();
                                function(Payload::String(payload.to_owned()), socket);
                                drop(function);
                            });
                        }
                        if let Some(ref payload) = socket_packet.binary_data {
                            spawn_scoped!({
                                let mut function = ack.callback.write().unwrap();
                                let socket = clone_self.clone();
                                function(Payload::Binary(payload.to_owned()), socket);
                                drop(function);
                            });
                        }
                    }
                }
            }
            for index in to_be_removed {
                clone_self.outstanding_acks.write().unwrap().remove(index);
            }
        }
    }

    /// Sets up the callback routes on the engine.io socket, called before
    /// opening the connection.
    fn setup_callbacks(&mut self) -> Result<()> {
        let clone_self: Socket = self.clone();
        let error_callback = move |msg| {
            if let Some(function) = clone_self.get_event_callback(&Event::Error) {
                let mut lock = function.1.write().unwrap();
                let socket = clone_self.clone();
                lock(Payload::String(msg), socket);
                drop(lock)
            }
        };

        let clone_self = self.clone();
        let open_callback = move |_| {
            if let Some(function) = clone_self.get_event_callback(&Event::Connect) {
                let mut lock = function.1.write().unwrap();
                let socket = clone_self.clone();
                lock(
                    Payload::String(String::from("Connection is opened")),
                    socket,
                );
                drop(lock)
            }
        };

        let clone_self = self.clone();
        let close_callback = move |_| {
            if let Some(function) = clone_self.get_event_callback(&Event::Close) {
                let mut lock = function.1.write().unwrap();
                let socket = clone_self.clone();
                lock(
                    Payload::String(String::from("Connection is closed")),
                    socket,
                );
                drop(lock)
            }
        };

        self.engine_socket.write()?.on_open(open_callback)?;

        self.engine_socket.write()?.on_error(error_callback)?;

        self.engine_socket.write()?.on_close(close_callback)?;

        let clone_self = self.clone();
        self.engine_socket
            .write()?
            .on_data(move |data| Self::handle_new_message(data, &clone_self))
    }

    /// Handles a binary event.
    #[inline]
    fn handle_binary_event(socket_packet: SocketPacket, clone_self: &Socket) {
        let event = if let Some(string_data) = socket_packet.data {
            string_data.replace("\"", "").into()
        } else {
            Event::Message
        };

        if let Some(binary_payload) = socket_packet.binary_data {
            if let Some(function) = clone_self.get_event_callback(&event) {
                spawn_scoped!({
                    let mut lock = function.1.write().unwrap();
                    let socket = clone_self.clone();
                    lock(Payload::Binary(binary_payload), socket);
                    drop(lock);
                });
            }
        }
    }

    /// A method for handling the Event Socket Packets.
    // this could only be called with an event
    fn handle_event(socket_packet: SocketPacket, clone_self: &Socket) {
        // unwrap the potential data
        if let Some(data) = socket_packet.data {
            // the string must be a valid json array with the event at index 0 and the
            // payload at index 1. if no event is specified, the message callback is used
            if let Ok(serde_json::Value::Array(contents)) =
                serde_json::from_str::<serde_json::Value>(&data)
            {
                let event: Event = if contents.len() > 1 {
                    contents
                        .get(0)
                        .map(|value| match value {
                            serde_json::Value::String(ev) => ev,
                            _ => "message",
                        })
                        .unwrap_or("message")
                        .into()
                } else {
                    Event::Message
                };
                // check which callback to use and call it with the data if it's present
                if let Some(function) = clone_self.get_event_callback(&event) {
                    spawn_scoped!({
                        let mut lock = function.1.write().unwrap();
                        let socket = clone_self.clone();
                        // if the data doesn't contain an event type at position `1`, the event must be
                        // of the type `Message`, in that case the data must be on position one and
                        // unwrapping is safe
                        lock(
                            Payload::String(
                                contents
                                    .get(1)
                                    .unwrap_or_else(|| contents.get(0).unwrap())
                                    .to_string(),
                            ),
                            socket,
                        );
                        drop(lock);
                    });
                }
            }
        }
    }

    /// A convenient method for finding a callback for a certain event.
    #[inline]
    fn get_event_callback(&self, event: &Event) -> Option<&(Event, Callback<Payload>)> {
        self.on.iter().find(|item| item.0 == *event)
    }

    fn is_engineio_connected(&self) -> Result<bool> {
        self.engine_socket.read()?.is_connected()
    }
}

impl Debug for Ack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "Ack(id: {:?}), timeout: {:?}, time_started: {:?}, callback: {}",
            self.id, self.timeout, self.time_started, "Fn(String)",
        ))
    }
}

impl Debug for Socket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("Socket(engine_socket: {:?}, host: {:?}, connected: {:?}, on: <defined callbacks>, outstanding_acks: {:?}, nsp: {:?})",
            self.engine_socket,
            self.host,
            self.connected,
            self.outstanding_acks,
            self.nsp,
        ))
    }
}

#[cfg(test)]
mod test {

    use std::thread::sleep;

    use super::*;
    use crate::socketio::payload::Payload;
    use bytes::Bytes;
    use native_tls::TlsConnector;
    use reqwest::header::{ACCEPT_ENCODING, HOST};
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn socket_io_integration() -> Result<()> {
        let url = crate::socketio::test::socket_io_server()?;

        let mut socket = Socket::new(url, None, None, None)?;

        let result = socket.on(
            "test",
            Box::new(|msg, _| match msg {
                Payload::String(str) => println!("Received string: {}", str),
                Payload::Binary(bin) => println!("Received binary data: {:#?}", bin),
            }),
        );
        assert!(result.is_ok());

        let result = socket.connect();
        assert!(result.is_ok());

        let payload = json!({"token": 123});
        let result = socket.emit("test", Payload::String(payload.to_string()));

        assert!(result.is_ok());

        let ack_callback = move |message: Payload, mut socket_: Socket| {
            let result = socket_.emit(
                "test",
                Payload::String(json!({"got ack": true}).to_string()),
            );
            assert!(result.is_ok());

            println!("Yehaa! My ack got acked?");
            if let Payload::String(str) = message {
                println!("Received string Ack");
                println!("Ack data: {}", str);
            }
        };

        let ack = socket.emit_with_ack(
            "test",
            Payload::String(payload.to_string()),
            Duration::from_secs(2),
            ack_callback,
        );
        assert!(ack.is_ok());

        socket.disconnect().unwrap();
        // assert!(socket.disconnect().is_ok());

        sleep(Duration::from_secs(10));

        Ok(())
    }

    #[test]
    fn socket_io_builder_integration() -> Result<()> {
        let url = crate::socketio::test::socket_io_server()?;

        // test socket build logic
        let socket_builder = SocketBuilder::new(url);

        let tls_connector = TlsConnector::builder()
            .use_sni(true)
            .build()
            .expect("Found illegal configuration");

        let socket = socket_builder
            .namespace("/")
            .expect("Error!")
            .tls_config(tls_connector)
            .opening_header(HOST, "localhost".parse().unwrap())
            .opening_header(ACCEPT_ENCODING, "application/json".parse().unwrap())
            .on("test", |str, _| println!("Received: {:#?}", str))
            .on("message", |payload, _| println!("{:#?}", payload))
            .connect();

        assert!(socket.is_ok());

        let mut socket = socket.unwrap();
        assert!(socket.emit("message", json!("Hello World")).is_ok());

        assert!(socket.emit("binary", Bytes::from_static(&[46, 88])).is_ok());

        let ack_cb = |payload, _| {
            println!("Yehaa the ack got acked");
            println!("With data: {:#?}", payload);
        };

        assert!(socket
            .emit_with_ack("binary", json!("pls ack"), Duration::from_secs(1), ack_cb,)
            .is_ok());

        sleep(Duration::from_secs(5));

        Ok(())
    }

    #[test]
    fn it_works() -> Result<()> {
        let url = crate::socketio::test::socket_io_server()?;

        let mut socket = Socket::new(url, None, None, None)?;

        assert!(socket
            .on(
                "test",
                Box::new(|message, _| {
                    if let Payload::String(st) = message {
                        println!("{}", st)
                    }
                })
            )
            .is_ok());

        assert!(socket.on("Error", Box::new(|_, _| {})).is_ok());

        assert!(socket.on("Connect", Box::new(|_, _| {})).is_ok());

        assert!(socket.on("Close", Box::new(|_, _| {})).is_ok());

        socket.connect().unwrap();

        let ack_callback = |message: Payload, _| {
            println!("Yehaa! My ack got acked?");
            if let Payload::String(str) = message {
                println!("Received string ack");
                println!("Ack data: {}", str);
            }
        };

        assert!(socket
            .emit_with_ack(
                "test",
                Payload::String("123".to_owned()),
                Duration::from_secs(10),
                ack_callback
            )
            .is_ok());
        Ok(())
    }

    #[test]
    fn test_error_cases() -> Result<()> {
        let result = Socket::new("http://localhost:123", None, None, None);
        assert!(result.is_err());
        Ok(())
    }
}
