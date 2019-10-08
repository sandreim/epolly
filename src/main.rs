extern crate epoll;
extern crate libc;
use epoll::Events;
use libc::c_int;
use std::collections::HashMap;
use std::env;
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::rc::{Weak, Rc};
use std::ffi::OsString;
use std::fmt::{Display, Formatter};
use std::net::UdpSocket;


pub enum Error {
    EpollCreate,
    Poll,
}

impl std::fmt::Debug for Error {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        use self::Error::*;

        match self {
            EpollCreate => write!(f, "Unable to create epoll fd."),
            Poll => write!(f, "Error during epoll call."),
        }
    }
}
const EVENT_BUFFER_SIZE: usize = 100;

type Result<T> = std::result::Result<T, Error>;

pub trait EventHandler {
    fn handle_read(&mut self) {}

    fn handle_write(&mut self) {}

    fn handle_close(&mut self) {}
    fn fd(&self) -> RawFd;
    fn id(&self) -> u64;
}

impl AsRawFd for EventHandler {
    fn as_raw_fd(&self) -> RawFd {
        self.fd()
    }
}

pub struct EventManager {
    fd: RawFd,
    handlers: HashMap<u64, Weak<dyn EventHandler>>,
    events: Vec<epoll::Event>,
}

impl EventManager{
    pub fn new() -> Result<Self> {
        let epoll_raw_fd = epoll::create(true).map_err(|_| Error::EpollCreate)?;
        Ok(EventManager {
            fd: epoll_raw_fd,
            handlers: HashMap::new(),
            events: vec![epoll::Event::new(epoll::Events::empty(), 0); EVENT_BUFFER_SIZE],
        })
    }

    pub fn register(&mut self, weak_handler: &Weak<EventHandler>) -> Result<usize> {
        match weak_handler.upgrade() {
            Some(handler) => {
                epoll::ctl(
                    self.fd,
                    epoll::ControlOptions::EPOLL_CTL_ADD,
                    handler.fd(),
                    epoll::Event::new(epoll::Events::EPOLLIN, handler.id()),
                )
                .map_err(|_| Error::Poll)?;

                self.handlers.insert(handler.id(), *weak_handler);
            },
            None => println!("Unable to upgrade handler weak ref")
        }
       
        Ok(0)
    }

    fn process_events(&mut self, event_count: usize) {
        for idx in 0..event_count {
            let event = self.events[idx];
            let evset = match Events::from_bits(event.events) {
                Some(evset) => evset,
                None => {
                    println!("epoll: ignoring unknown event set: 0x{:x}", event.events);
                    continue;
                }
            };

            // Fetch the handler by epoll userdata <-> handler.id()
            // Will panic if we received an event for an unknown handler.
            match self.handlers.get_mut(&event.data).unwrap().upgrade() {
                Some(handler) => {
                    if evset.contains(epoll::Events::EPOLLIN) {
                        handler.handle_read();
                    }

                    if evset.contains(epoll::Events::EPOLLOUT) {
                        handler.handle_write();
                    }

                    if evset.contains(epoll::Events::EPOLLHUP) {
                        handler.handle_close();
                    }
                },
                None => println!("Unable to upgrade handler weak ref")
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

struct UdpEchoServer {
    socket: UdpSocket,
    read_buf: Vec<u8>,
}

impl UdpEchoServer {
    fn new(port: u16) -> std::io::Result<UdpEchoServer> {
        Ok(UdpEchoServer {
            socket: UdpSocket::bind(format!("127.0.0.1:{}", port))?,
            read_buf: vec![0; 128]
        })
    }

    pub fn yahoo(&self) {
        println!("Yahoo!");
    }
}

impl EventHandler for UdpEchoServer {
    fn handle_read(&mut self) {
        let (count, from) = self.socket.recv_from(self.read_buf.as_mut()).unwrap();
        println!("Received {} from {}", count, from);
        self.socket.send_to(&self.read_buf[..count], from);
    }

    fn handle_close(&mut self) {}
    fn fd(&self) -> RawFd {
        self.socket.as_raw_fd()
    }
    fn id(&self) -> u64 {
        1337
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
    let server = Rc::new(UdpEchoServer::new(port).unwrap());
    let mut em = EventManager::new().unwrap();
    em.register(server);

    loop {
        em.run_timed(100);
       // server.yahoo();
    }
}
