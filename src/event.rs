use bytes::Bytes;
use crate::error::Result;

pub trait EventEmitter<RemoteEvent, LocalEvent, Callback> {
    fn emit<T: Into<Bytes>>(&self, event: RemoteEvent, bytes: T, callback: Option<Callback>) -> Result<()>;

    fn on(&mut self, event: LocalEvent, callback: Callback) -> Result<()>;

    fn off(&mut self, event: LocalEvent, callback: Option<Callback>) -> Result<()>;
}
