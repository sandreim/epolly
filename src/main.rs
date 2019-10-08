extern crate epoll;
extern crate libc;
#[macro_use]
extern crate bitflags;
use epoll::Events;
use std::cell::RefCell;
use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::fmt::Formatter;
use std::net::UdpSocket;
use std::os::unix::io::{AsRawFd, RawFd};
use std::rc::{Rc, Weak};

const EVENT_BUFFER_SIZE: usize = 100;

pub struct EventID(u64);

pub enum Error {
    EpollCreate,
    Poll,
    AlreadyExists,
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
        }
    }
}

type Result<T> = std::result::Result<T, Error>;

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

    pub fn finish(&self) -> (RawFd, EventID, EventType) {
        (self.fd.clone(), EventID(self.id.0), self.event_mask)
    }
}

pub trait EventHandler {
    /// called with EventInfo
    fn handle_read(&mut self, _info: EventID) {}
    fn handle_write(&mut self, _info: EventID) {}
    fn handle_close(&mut self, _info: EventID) {}

    /// Supports registering multiple fds. Each one is identified by EventInfo id.
    fn registration_info(&self) -> Vec<(RawFd, EventID, EventType)>;
}

pub struct EventManager {
    fd: RawFd,
    handlers: HashMap<u64, Weak<RefCell<dyn EventHandler>>>,
    events: Vec<epoll::Event>,
}

impl EventManager {
    pub fn new() -> Result<Self> {
        let epoll_raw_fd = epoll::create(true).map_err(|_| Error::EpollCreate)?;
        Ok(EventManager {
            fd: epoll_raw_fd,
            handlers: HashMap::new(),
            events: vec![epoll::Event::new(epoll::Events::empty(), 0); EVENT_BUFFER_SIZE],
        })
    }

    pub fn register<T: EventHandler + 'static>(&mut self, handler: T) -> Result<Rc<RefCell<T>>> {
        let registration_info = handler.registration_info();
        let wrapped_type = Rc::new(RefCell::new(handler));
        let wrapped_handler: Rc<RefCell<dyn EventHandler>> = wrapped_type.clone();

        for info in registration_info {
            let (fd, event_id, event_type) = info;
            if self.handlers.get(&event_id.0).is_some() {
                println!("Event ID {} already registered", event_id.0);
                return Err(Error::AlreadyExists);
            };

            let mut epoll_event_mask = epoll::Events::empty();

            if event_type.contains(EventType::READ) {
                epoll_event_mask |= epoll::Events::EPOLLIN;
            }

            if event_type.contains(EventType::WRITE) {
                epoll_event_mask |= epoll::Events::EPOLLOUT;
            }

            if event_type.contains(EventType::CLOSE) {
                epoll_event_mask |= epoll::Events::EPOLLHUP;
            }

            epoll::ctl(
                self.fd,
                epoll::ControlOptions::EPOLL_CTL_ADD,
                fd,
                epoll::Event::new(epoll_event_mask, event_id.0),
            )
            .map_err(|_| Error::Poll)?;
            self.handlers
                .insert(event_id.0, Rc::downgrade(&wrapped_handler));
        }

        Ok(wrapped_type)
    }

    pub fn unregister(&mut self, id: u64) {
        self.handlers.remove(&id);
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

            // Fetch the handler by epoll userdata <-> handler.id()
            // Will panic if we received an event for an unknown handler.
            match self.handlers.get_mut(&event_data).unwrap().upgrade() {
                Some(handler) => {
                    if evset.contains(epoll::Events::EPOLLIN) {
                        handler.borrow_mut().handle_read(EventID(event_data));
                    }

                    if evset.contains(epoll::Events::EPOLLOUT) {
                        handler.borrow_mut().handle_write(EventID(event_data));
                    }

                    if evset.contains(epoll::Events::EPOLLHUP) {
                        handler.borrow_mut().handle_close(EventID(event_data));
                    }
                }
                None => println!("Unable to upgrade handler weak ref"),
            }
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
}

impl UdpEchoServer {
    fn new(port: u16) -> std::io::Result<UdpEchoServer> {
        let mut sockets = Vec::new();
        sockets.push(UdpSocket::bind(format!("127.0.0.1:{}", port))?);
        sockets.push(UdpSocket::bind(format!("127.0.0.1:{}", port + 1))?);

        Ok(UdpEchoServer {
            sockets: sockets,
            read_buf: vec![0; 128],
        })
    }

    pub fn yahoo(&self) {
        println!("Yahoo!");
    }
}

impl EventHandler for UdpEchoServer {
    fn handle_read(&mut self, info: EventID) {
        let (count, from) = self.sockets[(info.0 - ECHO_SERVER_ID_BASE) as usize]
            .recv_from(self.read_buf.as_mut())
            .unwrap();
        println!("event_info: {}, - received {} from {}", info.0, count, from);
        self.sockets[(info.0 - ECHO_SERVER_ID_BASE) as usize]
            .send_to(&self.read_buf[..count], from);
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
        em.run_timed(1000);
        {
            // temporarly borrow the server
            let server = wrapped_server.borrow();
            server.yahoo();

            // server borrow is dropped so the event manager can borrow it
        }
    }
}
