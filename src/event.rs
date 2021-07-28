use crate::error::Result;
use bytes::Bytes;

pub trait EventEmitter<RemoteEvent, LocalEvent, Callback> {
    fn emit<T: Into<Bytes>>(&self, event: RemoteEvent, bytes: T) -> Result<()>;

    fn on(&mut self, event: LocalEvent, callback: Callback) -> Result<()>;

    fn off(&mut self, event: LocalEvent) -> Result<()>;
}
