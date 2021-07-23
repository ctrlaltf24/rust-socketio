use crate::engineio::event::Event as EngineEvent;

/// An `Event` in `socket.io` could either (`Message`, `Error`) or custom.
#[derive(Debug, PartialEq, PartialOrd, Eq, Hash)]
pub enum Event {
    Message,
    Error,
    Custom(String),
    Connect,
    Close,
}

impl From<String> for Event {
    fn from(string: String) -> Self {
        match &string.to_lowercase()[..] {
            "message" => Event::Message,
            "error" => Event::Error,
            "open" => Event::Connect,
            "close" => Event::Close,
            custom => Event::Custom(custom.to_owned()),
        }
    }
}

impl From<&str> for Event {
    fn from(string: &str) -> Self {
        Event::from(String::from(string))
    }
}

impl From<Event> for String {
    fn from(event: Event) -> Self {
        match event {
            Event::Message => Self::from("message"),
            Event::Connect => Self::from("open"),
            Event::Close => Self::from("close"),
            Event::Error => Self::from("error"),
            Event::Custom(string) => string,
        }
    }
}
