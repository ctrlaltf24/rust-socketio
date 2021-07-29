use super::event::Event;
#[cfg(feature = "client")]
use super::transports::{
    websocket::WebsocketTransport, websocket_secure::WebsocketSecureTransport,
};
#[cfg(feature = "client")]
use crate::client::Client;
use crate::engineio::packet::Payload;
use crate::engineio::packet::{HandshakePacket, Packet, PacketId};
use crate::engineio::transport::Transport;
use crate::engineio::transports::polling::PollingTransport;
use crate::error::{Error, Result};
pub use crate::event::EventEmitter;
use ::websocket::header::Headers;
use bytes::Bytes;
use native_tls::TlsConnector;
use reqwest::header::HeaderMap;
use std::collections::HashMap;
use std::convert::TryInto;
use std::sync::atomic::Ordering;
#[cfg(feature = "client")]
use std::time::Duration;
use std::{
    fmt::Debug,
    sync::{atomic::AtomicBool, Arc, Mutex, RwLock},
    time::Instant,
};
use url::Url;

/// Type of a `Callback` function. (Normal closures can be passed in here).
type Callback = Box<dyn Fn(Bytes) + 'static + Sync + Send>;

/// An `engine.io` socket which manages a connection with the server and allows
/// it to register common callbacks.
#[derive(Clone)]
pub struct EngineIoSocket {
    pub(super) transport: Arc<RwLock<Box<dyn Transport + Sync + Send>>>,
    pub connected: Arc<AtomicBool>,
    last_ping: Arc<Mutex<Instant>>,
    last_pong: Arc<Mutex<Instant>>,
    base_url: Arc<RwLock<Url>>,
    connection_data: Arc<RwLock<Option<HandshakePacket>>>,
    callbacks: Arc<RwLock<HashMap<Event, Vec<Callback>>>>,
    opening_headers: Arc<RwLock<Option<HeaderMap>>>,
    tls_config: Arc<RwLock<Option<TlsConnector>>>,
}

impl EngineIoSocket {
    /// Creates an instance of `EngineIoSocket`.
    pub fn new(
        base_url: Url,
        root_path: Option<String>,
        tls_config: Option<TlsConnector>,
        opening_headers: Option<HeaderMap>,
    ) -> Self {
        let mut url = base_url.clone();
        url.path_segments_mut()
            .unwrap()
            .push(&root_path.unwrap_or_else(|| "/engine.io".to_owned()));
        if url.query_pairs().any(|(k, _)| k == "EIO") {
            url.query_pairs_mut().append_pair("EIO", "4");
        }
        EngineIoSocket {
            transport: Arc::new(RwLock::new(Box::new(PollingTransport::new(
                url.clone(),
                tls_config.clone(),
                opening_headers.clone(),
            )))),
            connected: Arc::new(AtomicBool::default()),
            last_ping: Arc::new(Mutex::new(Instant::now())),
            last_pong: Arc::new(Mutex::new(Instant::now())),
            connection_data: Arc::new(RwLock::new(None)),
            callbacks: Arc::new(RwLock::new(HashMap::new())),
            opening_headers: Arc::new(RwLock::new(opening_headers)),
            tls_config: Arc::new(RwLock::new(tls_config)),
            base_url: Arc::new(RwLock::new(url)),
        }
    }

    /// Sends a packet to the server. This optionally handles sending of a
    /// socketio binary attachment via the boolean attribute `is_binary_att`.
    pub fn emit(&self, packet: Packet, is_binary_att: bool) -> Result<()> {
        if !self.connected.load(Ordering::Acquire) {
            let error = Error::IllegalActionAfterOpen();
            self.callback(Event::Error, format!("{}", error))?;
            return Err(error);
        }

        // send a post request with the encoded payload as body
        // if this is a binary attachment, then send the raw bytes
        let data = if is_binary_att {
            packet.data
        } else {
            Payload::new(vec![packet]).try_into()?
        };

        if let Err(error) = self.transport.read()?.emit(data, is_binary_att) {
            self.callback(Event::Error, error.to_string())?;
            return Err(error);
        }

        Ok(())
    }

    // Check if the underlying transport client is connected.
    pub(crate) fn is_connected(&self) -> Result<bool> {
        Ok(self.connected.load(Ordering::Acquire))
    }

    /// Calls the error callback with a given message.
    #[inline]
    fn callback<T: Into<Bytes>>(&self, event: Event, payload: T) -> Result<()> {
        let callbacks = self.callbacks.read()?;
        let functions = callbacks.get(&event);
        if let Some(functions) = functions {
            let bytes = payload.into();
            for function in functions {
                spawn_scoped!(function(bytes.clone()));
            }
        }
        Ok(())
    }

    /// Polls for next payload
    pub(crate) fn poll(&self) -> Result<Option<Payload>> {
        if self.connected.load(Ordering::Acquire) {
            let transport = self.transport.read()?;
            let data = transport.poll()?;
            drop(transport);

            if data.is_empty() {
                return Ok(None);
            }

            Ok(Some(data.try_into()?))
        } else {
            Err(Error::IllegalActionAfterClose())
        }
    }

    fn get_ws_headers(&self) -> Result<Headers> {
        let mut headers = Headers::new();
        // SAFETY: unwrapping is safe as we only hand out `Weak` copies after the connection procedure
        if self.opening_headers.read()?.is_some() {
            let opening_headers = self.opening_headers.read()?;
            for (key, val) in opening_headers.clone().unwrap() {
                headers.append_raw(key.unwrap().to_string(), val.as_bytes().to_owned());
            }
        }
        Ok(headers)
    }

    /// Performs the server long polling procedure as long as the client is
    /// connected. This should run separately at all time to ensure proper
    /// response handling from the server.
    #[cfg(feature = "client")]
    pub(crate) fn poll_cycle(&self) -> Result<()> {
        if !self.connected.load(Ordering::Acquire) {
            let error = Error::IllegalActionBeforeOpen();
            self.callback(Event::Error, format!("{}", error))?;
            return Err(error);
        }

        // as we don't have a mut self, the last_ping needs to be safed for later
        let mut last_ping = *self.last_ping.lock()?;
        // the time after we assume the server to be timed out
        let server_timeout = Duration::from_millis(
            (*self.connection_data)
                .read()?
                .as_ref()
                .map_or_else(|| 0, |data| data.ping_timeout)
                + (*self.connection_data)
                    .read()?
                    .as_ref()
                    .map_or_else(|| 0, |data| data.ping_interval),
        );

        while self.connected.load(Ordering::Acquire) {
            let packets = self.poll()?;

            if packets.is_none() {
                break;
            }

            for packet in packets.unwrap().as_vec() {
                self.callback(Event::Packet, packet.clone())?;

                // check for the appropriate action or callback
                match packet.packet_id {
                    PacketId::Message => {
                        self.callback(Event::Data, packet.clone())?;
                    }

                    PacketId::Close => {
                        self.callback(Event::Close, packet.clone())?;
                        // set current state to not connected and stop polling
                        self.connected.store(false, Ordering::Release);
                    }
                    PacketId::Open => {
                        unreachable!("Won't happen as we open the connection beforehand");
                    }
                    PacketId::Upgrade => {
                        // this is already checked during the handshake, so just do nothing here
                    }
                    PacketId::Ping => {
                        last_ping = Instant::now();
                        self.emit(Packet::new(PacketId::Pong, Bytes::new()), false)?;
                    }
                    PacketId::Pong => {
                        // this will never happen as the pong packet is
                        // only sent by the client
                        unreachable!();
                    }
                    PacketId::Noop => (),
                    PacketId::Base64 => {
                        // Has no id, not possible to decode.
                        unreachable!();
                    }
                }
            }

            if server_timeout < last_ping.elapsed() {
                // the server is unreachable
                // set current state to not connected and stop polling
                self.connected.store(false, Ordering::Release);
            }
        }
        Ok(())
    }

    /// This handles the upgrade from polling to websocket transport. Looking at the protocol
    /// specs, this needs the following preconditions:
    /// - the handshake must have already happened
    /// - the handshake `upgrade` field needs to contain the value `websocket`.
    /// If those preconditions are valid, it is save to call the method. The protocol then
    /// requires the following procedure:
    /// - the client starts to connect to the server via websocket, mentioning the related `sid` received
    ///   in the handshake.
    /// - to test out the connection, the client sends a `ping` packet with the payload `probe`.
    /// - if the server receives this correctly, it responses by sending a `pong` packet with the payload `probe`.
    /// - if the client receives that both sides can be sure that the upgrade is save and the client requests
    ///   the final upgrade by sending another `update` packet over `websocket`.
    /// If this procedure is alright, the new transport is established.
    #[cfg(feature = "client")]
    fn upgrade_connection(&mut self) -> Result<()> {
        let tls_config = self.tls_config.read()?.clone();

        let full_address = self.base_url.read()?.clone();
        let base_url = Url::parse(&full_address.to_string()[..])?;
        drop(full_address);

        match base_url.scheme() {
            "https" => {
                *self.transport.write()? = Box::new(WebsocketSecureTransport::new(
                    base_url,
                    tls_config,
                    self.get_ws_headers()?,
                ));
            }
            "http" => {
                *self.transport.write()? =
                    Box::new(WebsocketTransport::new(base_url, self.get_ws_headers()?));
            }
            _ => return Err(Error::InvalidUrl(base_url.to_string())),
        }

        Ok(())
    }

    pub(crate) fn on<T>(&mut self, event: Event, callback: T) -> Result<()>
    where
        T: Fn(Bytes) + 'static + Sync + Send,
    {
        // For some reason it doesn't resolve types correctly in a generic trait.
        let mut hash = Arc::get_mut(&mut self.callbacks).unwrap().write()?;
        if !hash.contains_key(&event) {
            hash.insert(event.clone(), vec![]);
        }
        let vec = hash.get_mut(&event);
        vec.unwrap().push(Box::new(callback));
        Ok(())
    }
}

impl EventEmitter<PacketId, Event, Callback> for EngineIoSocket {
    fn emit<T: Into<Bytes>>(&self, event: PacketId, bytes: T) -> Result<()> {
        self.emit(Packet::new(event, bytes.into()), false)
    }

    fn on(&mut self, event: Event, callback: Callback) -> Result<()> {
        self.on(event, callback)
    }

    fn off(&mut self, event: Event) -> Result<()> {
        let mut map = Arc::get_mut(&mut self.callbacks).unwrap().write()?;
        map.insert(event, Vec::new()).unwrap();
        Ok(())
    }
}

#[cfg(feature = "client")]
impl Client for EngineIoSocket {
    /// Opens the connection to a specified server. Includes an opening `GET`
    /// request to the server, the server passes back the handshake data in the
    /// response. If the handshake data mentions a websocket upgrade possibility,
    /// we try to upgrade the connection. Afterwards a first Pong packet is sent
    /// to the server to trigger the Ping-cycle.
    #[cfg(feature = "client")]
    fn connect(&mut self) -> Result<()> {
        let packets = self.poll()?;

        if packets.is_some() {
            let conn_data: HandshakePacket = packets
                .unwrap()
                .as_vec()
                .get(0)
                .unwrap()
                .clone()
                .try_into()?;
            self.connected.store(true, Ordering::Release);

            // check if we could upgrade to websockets
            let websocket_upgrade = conn_data
                .upgrades
                .iter()
                .any(|upgrade| upgrade.to_lowercase() == *"websocket");

            // update the base_url with the new sid
            let mut base_url = self.base_url.write()?;
            let new_base_url = base_url
                .query_pairs_mut()
                .append_pair("sid", &conn_data.sid[..])
                .finish()
                .clone();
            *base_url = new_base_url;
            drop(base_url);

            // if we have an upgrade option, send the corresponding request, if this doesn't work
            // for some reason, proceed via polling
            if websocket_upgrade {
                let _ = self.upgrade_connection();
            }

            // set the connection data
            let mut connection_data = self.connection_data.write()?;
            *connection_data = Some(conn_data);
            drop(connection_data);

            self.callback(Event::Open, "")?;

            // set the last ping to now and set the connected state
            *self.last_ping.lock()? = Instant::now();

            // emit a pong packet to keep trigger the ping cycle on the server
            self.emit(Packet::new(PacketId::Pong, Bytes::new()), false)?;

            Ok(())
        } else {
            let error = Error::InvalidHandshake("Empty response".to_owned());
            self.callback(Event::Error, format!("{}", error))?;
            Err(error)
        }
    }

    /// Disconnects this client from the server by sending a `engine.io` closing
    /// packet.
    fn disconnect(&mut self) -> Result<()> {
        let packet = Packet::new(PacketId::Close, Bytes::from_static(&[]));
        self.connected.store(false, Ordering::Release);
        self.emit(packet, false)
    }
}

impl Debug for EngineIoSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("EngineIoSocket(connected: {:?}, connection_data: {:?}, base_url: {:?}, last_ping: {:?}, last_pong: {:?}, opening_headers: {:?}, tls_config: {:?}, transport: ?)",
            self.connected,
            self.connection_data,
            self.base_url,
            self.last_ping,
            self.last_pong,
            self.opening_headers,
            self.tls_config,
        ))
    }
}

#[cfg(test)]
mod test {

    #[cfg(feature = "client")]
    use crate::engineio::packet::{Packet, PacketId};

    #[cfg(feature = "client")]
    use super::*;

    #[cfg(feature = "client")]
    use reqwest::header::HOST;
    #[cfg(feature = "client")]
    use std::thread::sleep;

    #[test]
    #[cfg(feature = "client")]
    fn test_connection_polling_packets() -> Result<()> {
        let mut socket =
            EngineIoSocket::new(crate::engineio::test::engine_io_server()?, None, None, None);

        socket.connect()?;

        // Our testing server is set up to send hello client on startup
        {
            let expected = Packet::new(PacketId::Message, Bytes::from_static(b"hello client"));
            let got = socket.poll()?.unwrap().as_vec().get(0).unwrap().clone();
            assert_eq!(expected, got);
        }

        socket.emit(
            Packet::new(PacketId::Message, Bytes::from_static(b"respond")),
            false,
        )?;
        // Our testing server is set up to respond to messages "respond" with "Roger Roger"
        {
            let expected = Packet::new(PacketId::Message, Bytes::from_static(b"Roger Roger"));
            let got = socket.poll()?.unwrap().as_vec().get(0).unwrap().clone();
            assert_eq!(expected, got);
        }

        // Wait for server to ping us
        {
            let expected = Packet::new(PacketId::Ping, Bytes::from_static(b""));
            let got = socket.poll()?.unwrap().as_vec().get(0).unwrap().clone();
            assert_eq!(expected, got);
        }
        // Respond with pong (normally done in poll_cycle)
        socket.emit(Packet::new(PacketId::Pong, Bytes::from_static(b"")), false)?;

        socket.emit(Packet::new(PacketId::Close, Bytes::from_static(&[])), false)?;

        Ok(())
    }

    #[test]
    #[cfg(feature = "client")]
    fn test_connection_secure_ws_http() -> Result<()> {
        let host =
            std::env::var("ENGINE_IO_SECURE_HOST").unwrap_or_else(|_| "localhost".to_owned());

        let mut headers = HeaderMap::new();
        headers.insert(HOST, host.parse().unwrap());

        let url = crate::engineio::test::engine_io_server_secure()?;

        let mut socket = EngineIoSocket::new(
            url,
            None,
            Some(crate::test::tls_connector().unwrap()),
            Some(headers),
        );

        socket.connect().unwrap();

        assert!(socket
            .emit(
                Packet::new(PacketId::Message, Bytes::from_static(b"HelloWorld")),
                false,
            )
            .is_ok());

        socket
            .on(
                Event::Data,
                Box::new(|data: Bytes| {
                    println!(
                        "Received: {:?}",
                        std::str::from_utf8(&data).expect("Error while decoding utf-8")
                    );
                }),
            )
            .unwrap();

        assert!(socket
            .emit(
                Packet::new(PacketId::Message, Bytes::from_static(b"Hi")),
                true
            )
            .is_ok());

        assert!(socket.poll_cycle().is_ok());

        Ok(())
    }

    #[test]
    #[cfg(feature = "client")]
    fn test_open_invariants() -> Result<()> {
        let url = crate::engineio::test::engine_io_server()?;

        let illegal_url = Url::parse("this is illegal")?;
        let mut sut = EngineIoSocket::new(illegal_url, None, None, None);

        let _error = sut.connect().expect_err("Error");
        assert!(matches!(
            Error::InvalidUrl(String::from("this is illegal")),
            _error
        ));

        let invalid_protocol = Url::parse("file:///tmp/foo")?;
        let mut sut = EngineIoSocket::new(invalid_protocol, None, None, None);

        let _error = sut.connect().expect_err("Error");
        assert!(matches!(
            Error::InvalidUrl(String::from("file://localhost:4200")),
            _error
        ));

        let sut = EngineIoSocket::new(url.clone(), None, None, None);
        let _error = sut
            .emit(Packet::new(PacketId::Close, Bytes::from_static(b"")), false)
            .expect_err("error");
        assert!(matches!(Error::IllegalActionBeforeOpen(), _error));

        // test missing match arm in socket constructor
        let mut headers = HeaderMap::new();
        let host =
            std::env::var("ENGINE_IO_SECURE_HOST").unwrap_or_else(|_| "localhost".to_owned());
        headers.insert(HOST, host.parse().unwrap());

        let _ = EngineIoSocket::new(
            url.clone(),
            None,
            Some(
                TlsConnector::builder()
                    .danger_accept_invalid_certs(true)
                    .build()
                    .unwrap(),
            ),
            None,
        );

        let _ = EngineIoSocket::new(url, None, None, Some(headers));

        Ok(())
    }

    #[test]
    #[cfg(feature = "client")]
    fn test_illegal_actions() -> Result<()> {
        let url = crate::engineio::test::engine_io_server()?;
        let sut = EngineIoSocket::new(url, None, None, None);
        assert!(sut.poll_cycle().is_err());

        assert!(sut
            .emit(Packet::new(PacketId::Close, Bytes::from_static(b"")), false)
            .is_err());
        assert!(sut
            .emit(
                Packet::new(PacketId::Message, Bytes::from_static(b"")),
                true
            )
            .is_err());
        Ok(())
    }

    #[test]
    #[cfg(feature = "client")]
    fn test_basic_connection() -> Result<()> {
        let url = crate::engineio::test::engine_io_server()?;

        let mut socket = EngineIoSocket::new(url, None, None, None);

        assert!(socket
            .on(Event::Open, |_| {
                println!("Open event!");
            })
            .is_ok());

        assert!(socket
            .on(Event::Packet, |packet| {
                println!("Received packet: {:?}", packet);
            })
            .is_ok());

        assert!(socket
            .on(Event::Data, |data| {
                println!("Received packet: {:?}", std::str::from_utf8(&data));
            })
            .is_ok());

        assert!(socket.connect().is_ok());

        assert!(socket
            .emit(
                Packet::new(PacketId::Message, Bytes::from_static(b"Hello World"),),
                false
            )
            .is_ok());

        assert!(socket
            .emit(
                Packet::new(PacketId::Message, Bytes::from_static(b"Hello World2"),),
                false
            )
            .is_ok());

        assert!(socket
            .emit(Packet::new(PacketId::Pong, Bytes::new()), false)
            .is_ok());

        sleep(Duration::from_secs(2));

        assert!(socket
            .emit(
                Packet::new(PacketId::Message, Bytes::from_static(b"Hello World3"),),
                false
            )
            .is_ok());

        Ok(())
    }
}
