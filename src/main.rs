extern crate epoll;
extern crate libc;
#[macro_use]
extern crate bitflags;
use epoll::Events;
use std::cell::{RefMut, RefCell};
use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::fmt::Formatter;
use std::net::UdpSocket;
use std::os::unix::io::{AsRawFd, RawFd};
use std::rc::{Rc, Weak};

const EVENT_BUFFER_SIZE: usize = 100;

#[derive(Default, Clone, Copy, PartialEq)]
pub struct EventID(u64);

pub enum Error {
    EpollCreate,
    Poll,
    AlreadyExists,
    EventNotFound(EventID),
}

bitflags! {
    #[derive(Default)]
    pub struct EventType: u32 {
        const NONE = 0b00000000;
        const READ = 0b00000001;
        const WRITE = 0b00000010;
        const CLOSE = 0b00000100;
    }
}

impl std::fmt::Debug for Error {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        use self::Error::*;

        match self {
            EpollCreate => write!(f, "Unable to create epoll fd."),
            Poll => write!(f, "Error during epoll call."),
            AlreadyExists => write!(f, "A handler for the specified event id already exists"),
            EventNotFound(id) => write!(f, "A handler for the specified event id {} was not found", id.0),
        }
    }
}

type Result<T> = std::result::Result<T, Error>;
type EventRegistrationData = (RawFd, EventID, EventType);

struct EventHandlerData {
    data: EventRegistrationData,
    handler: Weak<RefCell<dyn EventHandler>>,
}

impl EventHandlerData {
    fn new(data: EventRegistrationData, handler: Weak<RefCell<dyn EventHandler>>) -> EventHandlerData {
        EventHandlerData {
            data: data,
            handler: handler,
        }
    }
}

struct RegistrationBuilder {
    fd: RawFd,
    id: EventID,
    event_mask: EventType,
}

impl RegistrationBuilder {
    fn new(fd: RawFd) -> RegistrationBuilder {
        RegistrationBuilder {
            fd: fd,
            id: EventID(0),
            event_mask: EventType::NONE,
        }
    }

    pub fn with_id<'a>(&'a mut self, id: EventID) -> &'a mut RegistrationBuilder {
        self.id = id;
        self
    }

    pub fn readable<'a>(&'a mut self) -> &'a mut RegistrationBuilder {
        self.event_mask |= EventType::READ;
        self
    }

    pub fn writeable<'a>(&'a mut self) -> &'a mut RegistrationBuilder {
        self.event_mask |= EventType::WRITE;
        self
    }

    pub fn closeable<'a>(&'a mut self) -> &'a mut RegistrationBuilder {
        self.event_mask |= EventType::CLOSE;
        self
    }

    pub fn finish(&self) -> EventRegistrationData {
        (self.fd.clone(), EventID(self.id.0), self.event_mask)
    }
}

pub trait EventHandler {
    fn handle_read(&mut self, _info: EventID) {}
    fn handle_write(&mut self, _info: EventID) {}
    fn handle_close(&mut self, _info: EventID) {}

    /// Support registration of multiple fds. For each FD there is an associated ID in the
    /// EventID structure.
    fn registration_info(&self) -> Vec<EventRegistrationData>;
}

pub struct EventManager {
    fd: RawFd,
    handlers: HashMap<u64, EventHandlerData>,
    events: Vec<epoll::Event>,
    auto_id_index: EventID,
}

const EVENTMANAGER_AUTO_ID_BASE: u64 = 1_337_000_000_000;

impl EventManager {
    pub fn new() -> Result<Self> {
        let epoll_raw_fd = epoll::create(true).map_err(|_| Error::EpollCreate)?;
        Ok(EventManager {
            fd: epoll_raw_fd,
            handlers: HashMap::new(),
            events: vec![epoll::Event::new(epoll::Events::empty(), 0); EVENT_BUFFER_SIZE],
            auto_id_index: EventID(EVENTMANAGER_AUTO_ID_BASE),
        })
    }

    fn event_type_to_epoll_mask(event_types: EventType) -> epoll::Events {
        let mut epoll_event_mask = epoll::Events::empty();

        if event_types.contains(EventType::READ) {
            epoll_event_mask |= epoll::Events::EPOLLIN;
        }

        if event_types.contains(EventType::WRITE) {
            epoll_event_mask |= epoll::Events::EPOLLOUT;
        }

        if event_types.contains(EventType::CLOSE) {
            epoll_event_mask |= epoll::Events::EPOLLHUP;
        }

        epoll_event_mask
    }

    pub fn register<T: EventHandler + 'static>(&mut self, handler: T) -> Result<Rc<RefCell<T>>> {
        let registration_info = handler.registration_info();
        let wrapped_type = Rc::new(RefCell::new(handler));
        let wrapped_handler: Rc<RefCell<dyn EventHandler>> = wrapped_type.clone();

        for info in registration_info {
            let (fd, mut event_id, event_type) = info;

            if event_id == EventID(0) {
                event_id = self.auto_id_index;
                self.auto_id_index.0 += 1;
            }

            if self.handlers.get(&event_id.0).is_some() {
                println!("Event ID {} already registered", event_id.0);
                return Err(Error::AlreadyExists);
            };

            epoll::ctl(
                self.fd,
                epoll::ControlOptions::EPOLL_CTL_ADD,
                fd,
                epoll::Event::new(EventManager::event_type_to_epoll_mask(event_type), event_id.0),
            )
            .map_err(|_| Error::Poll)?;

            let event_handler_data = EventHandlerData::new((fd, event_id, event_type), Rc::downgrade(&wrapped_handler));

            self.handlers
                .insert(event_id.0, event_handler_data);
        }

        Ok(wrapped_type)
    }

    pub fn update(&mut self, id: EventID, event_types: EventType) -> Result<()> {
        if let Some(event_handler_data) = self.handlers.get(&id.0) {
            epoll::ctl(
                self.fd,
                epoll::ControlOptions::EPOLL_CTL_MOD,
                event_handler_data.data.0,
                epoll::Event::new(EventManager::event_type_to_epoll_mask(event_types), id.0),
            )
            .map_err(|_| Error::Poll)?;
        } else {
            println!("Event ID {} not found", id.0);
            return Err(Error::EventNotFound(id));
        }

        Ok(())
    }

    pub fn unregister(&mut self, id: EventID) -> Result <()> {
        match self.handlers.remove(&id.0) {
            Some(event_handler_data) => {
                epoll::ctl(
                    self.fd,
                    epoll::ControlOptions::EPOLL_CTL_DEL,
                    event_handler_data.data.0,
                    epoll::Event::new(epoll::Events::empty(), 0),
                )
                .map_err(|_| Error::Poll);
            },
            None => {
                println!("Event id {} not found", id.0);
                return Err(Error::EventNotFound(id));
            }
        }
        Ok(())
    }

    fn dispatch_event(&self, event_id: EventID, evset: epoll::Events, mut handler: RefMut<'_, dyn EventHandler>) {
        if evset.contains(epoll::Events::EPOLLIN) {
            handler.handle_read(event_id);
        }

        if evset.contains(epoll::Events::EPOLLOUT) {
            handler.handle_write(event_id);
        }

        if evset.contains(epoll::Events::EPOLLHUP) {
            handler.handle_close(event_id);
        }
    }

    fn process_events(&mut self, event_count: usize) {
        for idx in 0..event_count {
            let event = self.events[idx];
            let event_mask = event.events;
            let event_data = event.data;
            let evset = match Events::from_bits(event_mask) {
                Some(evset) => evset,
                None => {
                    println!("epoll: ignoring unknown event set: 0x{:x}", event_mask);
                    continue;
                }
            };

            let mut delete_handler = false;
            if let Some(event_handler_data) = self.handlers.get(&event_data) {
                if let Some(handler) = event_handler_data.handler.upgrade() {
                    match handler.try_borrow_mut() {
                        Ok(handler_ref) => self.dispatch_event(EventID(event_data), evset, handler_ref),
                        Err(e) => {
                            println!("Failed to borrow mutable handler: {}", e);
                            delete_handler = true;
                        }
                    }
                } else {
                    println!("Cannot upgrade weak handler for event id {}.", event_data);
                }
            }

            if delete_handler { self.unregister(EventID(event_data)); }
            
        }
    }

    pub fn run(&mut self) -> Result<usize> {
        let event_count =
            epoll::wait(self.fd, -1, &mut self.events[..]).map_err(|_| Error::Poll)?;
        self.process_events(event_count);

        Ok(event_count)
    }

    pub fn run_timed(&mut self, milliseconds: i32) -> Result<usize> {
        let event_count =
            epoll::wait(self.fd, milliseconds, &mut self.events[..]).map_err(|_| Error::Poll)?;
        self.process_events(event_count);

        Ok(event_count)
    }
}

const ECHO_SERVER_ID_BASE: u64 = 1000;

struct UdpEchoServer {
    sockets: Vec<UdpSocket>,
    read_buf: Vec<u8>,
    total_rx: usize,
    total_tx: usize,
}

impl UdpEchoServer {
    fn new(port: u16) -> std::io::Result<UdpEchoServer> {
        let mut sockets = Vec::new();
        sockets.push(UdpSocket::bind(format!("127.0.0.1:{}", port))?);
        sockets.push(UdpSocket::bind(format!("127.0.0.1:{}", port + 1))?);

        Ok(UdpEchoServer {
            sockets: sockets,
            read_buf: vec![0; 128],
            total_rx: 0,
            total_tx: 0,
        })
    }

    pub fn stats(&self) {
        println!("Rx bytes: {}, Tx bytes: {}", self.total_rx, self.total_tx);
    }
}

impl EventHandler for UdpEchoServer {
    fn handle_read(&mut self, info: EventID) {
        match self.sockets[(info.0 - ECHO_SERVER_ID_BASE) as usize]
            .recv_from(self.read_buf.as_mut()) {
            Ok((count, from)) => {
                self.total_rx += count;
                println!("event_info: {}, - received {} from {}", info.0, count, from);
                match self.sockets[(info.0 - ECHO_SERVER_ID_BASE) as usize]
                    .send_to(&self.read_buf[..count], from) {
                        Ok(tx) => self.total_tx += tx,
                        Err(e) => println!("Error while doing TX: {}", e)
                }
            },
            Err(e) => println!("Error while doing RX: {}", e)
        }
    }
    fn handle_write(&mut self, info: EventID) {
        println!("Writeable")
    }

    fn registration_info(&self) -> Vec<(RawFd, EventID, EventType)> {
        let mut info = Vec::new();
        for (i, socket) in self.sockets.iter().enumerate() {
            info.push(RegistrationBuilder::new(socket.as_raw_fd())
            .with_id(EventID(ECHO_SERVER_ID_BASE + i as u64))
            .readable()
            .finish())
        }
        info
    }
}

fn main() {
    let port = env::args_os()
        .nth(1)
        .unwrap_or(OsString::from("1984"))
        .into_string()
        .unwrap()
        .parse::<u16>()
        .unwrap();

    println!("Running ePolly with UDP server on port {}...", port);
    let mut em = EventManager::new().unwrap();

    let wrapped_server = em.register(UdpEchoServer::new(port).unwrap()).unwrap();
    loop {
        let server = wrapped_server.borrow();
        server.stats(); 
        match server.total_rx {
            1...9 => em.update(EventID(ECHO_SERVER_ID_BASE), EventType::WRITE | EventType::READ).unwrap(),
            _ => em.update(EventID(ECHO_SERVER_ID_BASE), EventType::READ).unwrap()
        }

        drop(server);        
        em.run_timed(1000);
    }
}
