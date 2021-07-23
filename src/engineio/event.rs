#[derive(Eq, Hash, PartialEq)]
pub enum Event {
    Close,
    Error,
    Packet,
    Data,
    Open,
}
