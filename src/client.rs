use crate::error::Result;

#[cfg(feature = "client")]
pub trait Client {
    fn connect(&mut self) -> Result<()>;
    fn disconnect(&mut self) -> Result<()>;
}
