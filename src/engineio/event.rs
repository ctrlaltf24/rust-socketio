#[derive(Eq, Hash, PartialEq, Clone)]
pub enum Event {
    Close,
    Error,
    Packet,
    Data,
    Open,
}
