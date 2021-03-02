use crate::engineio::packet::{decode_payload, encode_payload, Packet, PacketId};
use crate::error::{Error, Result};
use adler32::adler32;
use reqwest::{blocking::Client, Url};
use serde::{Deserialize, Serialize};
use std::time::SystemTime;
use std::{fmt::Debug, sync::atomic::Ordering};
use std::{
    sync::{atomic::AtomicBool, Arc, Mutex, RwLock},
    time::{Duration, Instant},
};
use websocket::{
    dataframe::DataFrame as WsDataFrame, receiver::Reader, sync::Writer, ws::dataframe::DataFrame,
    OwnedMessage,
};
use websocket::{sync::stream::TcpStream, ClientBuilder};

/// The different types of transport used for transmitting a payload.
#[derive(Clone)]
enum TransportType {
    Polling(Arc<Mutex<Client>>),
    Websocket(Arc<Mutex<Reader<TcpStream>>>, Arc<Mutex<Writer<TcpStream>>>),
}

/// Type of a `Callback` function. (Normal closures can be passed in here).
type Callback<I> = Arc<RwLock<Option<Box<dyn Fn(I) + 'static + Sync + Send>>>>;

/// A client which handles the plain transmission of packets in the `engine.io`
/// protocol. Used by the wrapper `EngineSocket`. This struct also holds the
/// callback functions.
#[derive(Clone)]
pub struct TransportClient {
    transport: Arc<TransportType>,
    pub on_error: Callback<String>,
    pub on_open: Callback<()>,
    pub on_close: Callback<()>,
    pub on_data: Callback<Vec<u8>>,
    pub on_packet: Callback<Packet>,
    connected: Arc<AtomicBool>,
    last_ping: Arc<Mutex<Instant>>,
    last_pong: Arc<Mutex<Instant>>,
    host_address: Arc<Mutex<Option<String>>>,
    connection_data: Arc<RwLock<Option<HandshakeData>>>,
    engine_io_mode: Arc<AtomicBool>,
}

/// Data which gets exchanged in a handshake as defined by the server.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct HandshakeData {
    sid: String,
    upgrades: Vec<String>,
    #[serde(rename = "pingInterval")]
    ping_interval: u64,
    #[serde(rename = "pingTimeout")]
    ping_timeout: u64,
}

impl TransportClient {
    /// Creates an instance of `TransportClient`.
    pub fn new(engine_io_mode: bool) -> Self {
        TransportClient {
            transport: Arc::new(TransportType::Polling(Arc::new(Mutex::new(Client::new())))),
            on_error: Arc::new(RwLock::new(None)),
            on_open: Arc::new(RwLock::new(None)),
            on_close: Arc::new(RwLock::new(None)),
            on_data: Arc::new(RwLock::new(None)),
            on_packet: Arc::new(RwLock::new(None)),
            connected: Arc::new(AtomicBool::default()),
            last_ping: Arc::new(Mutex::new(Instant::now())),
            last_pong: Arc::new(Mutex::new(Instant::now())),
            host_address: Arc::new(Mutex::new(None)),
            connection_data: Arc::new(RwLock::new(None)),
            engine_io_mode: Arc::new(AtomicBool::from(engine_io_mode)),
        }
    }

    /// Registers an `on_open` callback.
    pub fn set_on_open<F>(&mut self, function: F) -> Result<()>
    where
        F: Fn(()) + 'static + Sync + Send,
    {
        let mut on_open = self.on_open.write()?;
        *on_open = Some(Box::new(function));
        drop(on_open);
        Ok(())
    }

    /// Registers an `on_error` callback.
    pub fn set_on_error<F>(&mut self, function: F) -> Result<()>
    where
        F: Fn(String) + 'static + Sync + Send,
    {
        let mut on_error = self.on_error.write()?;
        *on_error = Some(Box::new(function));
        drop(on_error);
        Ok(())
    }

    /// Registers an `on_packet` callback.
    pub fn set_on_packet<F>(&mut self, function: F) -> Result<()>
    where
        F: Fn(Packet) + 'static + Sync + Send,
    {
        let mut on_packet = self.on_packet.write()?;
        *on_packet = Some(Box::new(function));
        drop(on_packet);
        Ok(())
    }

    /// Registers an `on_data` callback.
    pub fn set_on_data<F>(&mut self, function: F) -> Result<()>
    where
        F: Fn(Vec<u8>) + 'static + Sync + Send,
    {
        let mut on_data = self.on_data.write()?;
        *on_data = Some(Box::new(function));
        drop(on_data);
        Ok(())
    }

    /// Registers an `on_close` callback.
    pub fn set_on_close<F>(&mut self, function: F) -> Result<()>
    where
        F: Fn(()) + 'static + Sync + Send,
    {
        let mut on_close = self.on_close.write()?;
        *on_close = Some(Box::new(function));
        drop(on_close);
        Ok(())
    }

    /// Opens the connection to a specified server. Includes an opening `GET`
    /// request to the server, the server passes back the handshake data in the
    /// response. Afterwards a first Pong packet is sent to the server to
    /// trigger the Ping-cycle.
    pub fn open<T: Into<String> + Clone>(&mut self, address: T) -> Result<()> {
        // TODO: Check if Relaxed is appropiate -> change all occurences if not
        if self.connected.load(Ordering::Relaxed) {
            return Ok(());
        }

        // build the query path, random_t is used to prevent browser caching
        let query_path = self.get_query_path()?;

        match &mut self.transport.as_ref() {
            TransportType::Polling(client) => {
                if let Ok(full_address) = Url::parse(&(address.to_owned().into() + &query_path)) {
                    self.host_address = Arc::new(Mutex::new(Some(address.into())));

                    let response = client.lock()?.get(full_address).send()?.text()?;

                    // the response contains the handshake data
                    if let Ok(conn_data) = serde_json::from_str::<HandshakeData>(&response[1..]) {
                        *Arc::get_mut(&mut self.connected).unwrap() = AtomicBool::from(true);

                        // check if we could upgrade to websockets
                        let websocket_upgrade = conn_data
                            .upgrades
                            .iter()
                            .any(|upgrade| upgrade.to_lowercase() == *"websocket");

                        // if we have an upgrade option, send the corresponding request
                        if websocket_upgrade && self.upgrade_connection(&conn_data.sid).is_err() {
                            eprintln!("upgrading to websockets was not succesfull, proceeding via polling");
                        }

                        // set the connection data
                        let mut connection_data = self.connection_data.write()?;
                        *connection_data = Some(conn_data);
                        drop(connection_data);

                        // call the on_open callback
                        let function = self.on_open.read()?;
                        if let Some(function) = function.as_ref() {
                            spawn_scoped!(function(()));
                        }
                        drop(function);

                        // set the last ping to now and set the connected state
                        *self.last_ping.lock()? = Instant::now();

                        // emit a pong packet to keep trigger the ping cycle on the server
                        self.emit(Packet::new(PacketId::Pong, Vec::new()))?;

                        return Ok(());
                    }

                    let error = Error::HandshakeError(response);
                    self.call_error_callback(format!("{}", error))?;
                    return Err(error);
                }

                let error = Error::InvalidUrl(address.into());
                self.call_error_callback(format!("{}", error))?;
                Err(error)
            }
            _ => unreachable!("Can't happen as we initialize with polling and upgrade later"),
        }
    }

    /// This handles the upgrade from polling to websocket transport. Looking at the protocol
    /// specs, this needs the following preconditions:
    /// - the handshake must have already happened
    /// - the handshake `upgrade` field needs to contain the value `websocket`.
    /// If those precondionts are valid, it is save to call the method. The protocol then
    /// requires the following procedure:
    /// - the client starts to connect to the server via websocket, mentioning the related `sid` received
    ///   in the handshake.
    /// - to test out the connection, the client sends a `ping` packet with the payload `probe`.
    /// - if the server receives this correctly, it responses by sending a `pong` packet with the payload `probe`.
    /// - if the client receives that both sides can be sure that the upgrade is save and the client requests
    ///   the final updgrade by sending another `update` packet over `websocket`.
    /// If this procedure is alright, the new transport is established.
    fn upgrade_connection(&mut self, new_sid: &str) -> Result<()> {
        let host_address = self.host_address.lock()?;
        if let Some(address) = &*host_address {
            // unwrapping here is infact save as we only set this url if it gets orderly parsed
            let mut address = Url::parse(&address).unwrap();

            let full_address = address
                .query_pairs_mut()
                .clear()
                .append_pair("EIO", "4")
                .append_pair("transport", "websocket")
                .append_pair("sid", new_sid)
                .finish();

            // connect to the server via websockets
            let client = ClientBuilder::new(full_address.as_ref())?.connect_insecure()?;

            let (mut receiver, mut sender) = client.split()?;

            // send the probe packet
            let probe_packet = Packet::new(PacketId::Ping, b"probe".to_vec());
            sender.send_dataframe(&WsDataFrame::new(
                true,
                websocket::dataframe::Opcode::Text,
                encode_payload(vec![probe_packet]),
            ))?;

            // expect to receive a probe packet
            let message = receiver.recv_message()?;
            if message.take_payload() != b"3probe" {
                eprintln!("Error while handshaking ws");
                return Err(Error::HandshakeError("Error".to_owned()));
            }

            // finally send the upgrade request
            sender.send_message(&OwnedMessage::Text("5".to_owned()))?;

            // upgrade the transport layer
            self.transport = Arc::new(TransportType::Websocket(
                Arc::new(Mutex::new(receiver)),
                Arc::new(Mutex::new(sender)),
            ));

            return Ok(());
        }
        Err(Error::HandshakeError("Error - invalid url".to_owned()))
    }

    /// Sends a packet to the server
    pub fn emit(&self, packet: Packet) -> Result<()> {
        if !self.connected.load(Ordering::Relaxed) {
            let error = Error::ActionBeforeOpen;
            self.call_error_callback(format!("{}", error))?;
            return Err(error);
        }
        // build the target address
        let query_path = self.get_query_path()?;

        let host = self.host_address.lock()?;
        let address = Url::parse(&(host.as_ref().unwrap().to_owned() + &query_path)[..]).unwrap();
        drop(host);

        // send a post request with the encoded payload as body
        let data = encode_payload(vec![packet]);

        match &self.transport.as_ref() {
            TransportType::Polling(client) => {
                let client = client.lock()?;
                let status = client.post(address).body(data).send()?.status().as_u16();
                drop(client);

                if status != 200 {
                    let error = Error::HttpError(status);
                    self.call_error_callback(format!("{}", error))?;
                    return Err(error);
                }

                Ok(())
            }
            TransportType::Websocket(_, writer) => {
                let mut writer = writer.lock()?;

                let dataframe = WsDataFrame::new(true, websocket::dataframe::Opcode::Text, data);
                writer.send_dataframe(&dataframe).unwrap();

                Ok(())
            }
        }
    }

    /// Performs the server long polling procedure as long as the client is
    /// connected. This should run separately at all time to ensure proper
    /// response handling from the server.
    pub fn poll_cycle(&mut self) -> Result<()> {
        if !self.connected.load(Ordering::Relaxed) {
            let error = Error::ActionBeforeOpen;
            self.call_error_callback(format!("{}", error))?;
            return Err(error);
        }
        let client = Client::new();

        // as we don't have a mut self, the last_ping needs to be safed for later
        let mut last_ping = *self.last_ping.lock()?;
        // the time after we assume the server to be timed out
        let server_timeout = Duration::from_millis(
            Arc::as_ref(&self.connection_data)
                .read()?
                .as_ref()
                .unwrap()
                .ping_timeout
                + Arc::as_ref(&self.connection_data)
                    .read()?
                    .as_ref()
                    .unwrap()
                    .ping_interval,
        );

        while self.connected.load(Ordering::Relaxed) {
            let data = match &self.transport.as_ref() {
                // we wont't use the shared client as this blocks the ressource
                // in the long polling requests
                TransportType::Polling(_) => {
                    let query_path = self.get_query_path()?;

                    let host = self.host_address.lock()?;
                    let address =
                        Url::parse(&(host.as_ref().unwrap().to_owned() + &query_path)[..]).unwrap();
                    drop(host);

                    // TODO: check if to_vec is inefficient here
                    client.get(address).send()?.bytes()?.to_vec()
                }
                TransportType::Websocket(receiver, _) => {
                    let mut receiver = receiver.lock()?;

                    println!("About to receive");
                    dbg!(receiver.recv_message().unwrap().take_payload())
                }
            };

            if data.is_empty() {
                return Ok(());
            }

            let packets = decode_payload(data)?;

            for packet in packets {
                {
                    let on_packet = self.on_packet.read()?;
                    // call the packet callback
                    if let Some(function) = on_packet.as_ref() {
                        spawn_scoped!(function(packet.clone()));
                    }
                }
                // check for the appropiate action or callback
                match packet.packet_id {
                    PacketId::Message => {
                        let on_data = self.on_data.read()?;
                        if let Some(function) = on_data.as_ref() {
                            spawn_scoped!(function(packet.data));
                        }
                        drop(on_data);
                    }

                    PacketId::Close => {
                        let on_close = self.on_close.read()?;
                        if let Some(function) = on_close.as_ref() {
                            spawn_scoped!(function(()));
                        }
                        drop(on_close);
                        // set current state to not connected and stop polling
                        self.connected
                            .compare_and_swap(true, false, Ordering::Acquire);
                    }
                    PacketId::Open => {
                        unreachable!("Won't happen as we open the connection beforehand");
                    }
                    PacketId::Upgrade => {
                        todo!("Upgrade the connection, but only if possible");
                    }
                    PacketId::Ping => {
                        last_ping = Instant::now();
                        self.emit(Packet::new(PacketId::Pong, Vec::new()))?;
                    }
                    PacketId::Pong => {
                        // this will never happen as the pong packet is
                        // only sent by the client
                        unreachable!();
                    }
                    PacketId::Noop => (),
                }
            }

            if server_timeout < last_ping.elapsed() {
                // the server is unreachable
                // set current state to not connected and stop polling
                self.connected
                    .compare_and_swap(true, false, Ordering::Acquire);
            }
        }
        Ok(())
    }

    /// Produces a random String that is used to prevent browser caching.
    #[inline]
    fn get_random_t() -> String {
        let reader = format!("{:#?}", SystemTime::now());
        let hash = adler32(reader.as_bytes()).unwrap();
        hash.to_string()
    }

    /// Calls the error callback with a given message.
    #[inline]
    fn call_error_callback(&self, text: String) -> Result<()> {
        let function = self.on_error.read()?;
        if let Some(function) = function.as_ref() {
            spawn_scoped!(function(text));
        }
        drop(function);

        Ok(())
    }

    // Constructs the path for a request, depending on the different situations.
    #[inline]
    fn get_query_path(&self) -> Result<String> {
        // build the base path
        let mut path = format!(
            "/{}/?EIO=4&transport={}&t={}",
            if self.engine_io_mode.load(Ordering::Relaxed) {
                "engine.io"
            } else {
                "socket.io"
            },
            match self.transport.as_ref() {
                TransportType::Polling(_) => "polling",
                TransportType::Websocket(_, _) => "websocket",
            },
            TransportClient::get_random_t(),
        );

        // append a session id if the socket is connected
        if self.connected.load(Ordering::Relaxed) {
            path.push_str(&format!(
                "&sid={}",
                Arc::as_ref(&self.connection_data)
                    .read()?
                    .as_ref()
                    .unwrap()
                    .sid
            ));
        }

        Ok(path)
    }
}

impl Debug for TransportType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "TransportType({})",
            match self {
                Self::Polling(_) => "Polling",
                Self::Websocket(_, _) => "Websocket",
            }
        ))
    }
}

impl Debug for TransportClient {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_fmt(format_args!(
            "TransportClient(transport: {:?}, on_error: {:?}, on_open: {:?}, on_close: {:?}, on_packet: {:?}, on_data: {:?}, connected: {:?}, last_ping: {:?}, last_pong: {:?}, host_address: {:?}, connection_data: {:?}, engine_io_mode: {:?})",
            self.transport,
            if self.on_error.read().unwrap().is_some() {
                "Fn(String)"
            } else {
                "None"
            },
            if self.on_open.read().unwrap().is_some() {
                "Fn(())"
            } else {
                "None"
            },
            if self.on_close.read().unwrap().is_some() {
                "Fn(())"
            } else {
                "None"
            },
            if self.on_packet.read().unwrap().is_some() {
                "Fn(Packet)"
            } else {
                "None"
            },
            if self.on_data.read().unwrap().is_some() {
                "Fn(Vec<u8>)"
            } else {
                "None"
            },
            self.connected,
            self.last_ping,
            self.last_pong,
            self.host_address,
            self.connection_data,
            self.engine_io_mode,
        ))
    }
}

#[cfg(test)]
mod test {
    use crate::engineio::packet::{Packet, PacketId};

    use super::*;
    /// The `engine.io` server for testing runs on port 4201
    const SERVER_URL: &str = "http://localhost:4201";

    #[test]
    fn test_connection() {
        let mut socket = TransportClient::new(true);
        assert!(socket.open(SERVER_URL).is_ok());

        assert!(socket
            .emit(Packet::new(
                PacketId::Message,
                "HelloWorld".to_owned().into_bytes(),
            ))
            .is_ok());

        socket.on_data = Arc::new(RwLock::new(Some(Box::new(|data| {
            println!(
                "Received: {:?}",
                std::str::from_utf8(&data).expect("Error while decoding utf-8")
            );
        }))));

        // closes the connection
        assert!(socket
            .emit(Packet::new(
                PacketId::Message,
                "CLOSE".to_owned().into_bytes(),
            ))
            .is_ok());

        // assert!(socket.poll_cycle().is_ok());
        socket.poll_cycle().unwrap();
    }
}
