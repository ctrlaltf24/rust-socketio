use crate::error::Result;

pub trait Client {
    fn connect(&mut self) -> Result<()>;
    fn disconnect(&mut self) -> Result<()>;
}
