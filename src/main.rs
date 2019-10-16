extern crate epoll;
extern crate libc;
#[macro_use]
extern crate bitflags;
use epoll::Events;
use std::cell::{RefCell, RefMut};
use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::fmt::Formatter;
use std::io::{Read, Write};
use std::mem::{forget, transmute};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::rc::{Rc, Weak};
// use std::borrow::{Borrow};
use libc::c_void;
use std::ops::Deref;

const EVENT_BUFFER_SIZE: usize = 100;

#[derive(Default, Clone, PartialEq)]
pub struct OwnedFD {
    fd: RawFd,
}
type Pollable = Rc<OwnedFD>;

impl OwnedFD {
    pub fn from_unowned(rawfd: RawFd) -> Pollable {
        Rc::new(OwnedFD { fd: rawfd })
    }

    pub fn from<T: IntoRawFd>(rawfd: T) -> Pollable {
        Rc::new(OwnedFD {
            fd: rawfd.into_raw_fd(),
        })
    }
}

impl AsRawFd for OwnedFD {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

impl Drop for OwnedFD {
    fn drop(&mut self) {
        println!("Closing fd {}", self.fd);
        unsafe { libc::close(self.fd) };
    }
}

impl Deref for OwnedFD {
    type Target = i32;
    fn deref(&self) -> &Self::Target {
        &self.fd
    }
}

type Result<T> = std::result::Result<T, Error>;
type EventRegistrationData = (Pollable, EventType);
type SharedEventManager = Rc<RefCell<EventManager>>;

/// EventManager can run PollableOp
pub enum PollableOp {
    Register(EventRegistrationData),
    Unregister(EventRegistrationData),
    Update(EventRegistrationData)
}

pub enum Error {
    EpollCreate,
    Poll,
    AlreadyExists,
    PollableNotFound(Pollable),
}

bitflags! {
    #[derive(Default)]
    pub struct EventType: u32 {
        const NONE = 0b00000000;
        const READ = 0b00000001;
        const WRITE = 0b00000010;
        const CLOSE = 0b00000100;
        const ERROR = 0b00000100;

    }
}

impl EventType {
    pub fn readable(&self) -> bool {
        self.contains(EventType::READ)
    }

    pub fn writeable(&self) -> bool {
        self.contains(EventType::WRITE)
    }

    pub fn closed(&self) -> bool {
        self.contains(EventType::CLOSE)
    }

    pub fn error(&self) -> bool {
        self.contains(EventType::ERROR)
    }
}

impl std::fmt::Display for OwnedFD {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        write!(f, "{}", self.fd)
    }
}

impl std::fmt::Debug for Error {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        use self::Error::*;

        match self {
            EpollCreate => write!(f, "Unable to create epoll fd."),
            Poll => write!(f, "Error during epoll call."),
            AlreadyExists => write!(f, "A handler for the specified pollable already exists"),
            PollableNotFound(pollable) => write!(
                f,
                "A handler for the specified pollable {} was not found",
                *pollable
            ),
        }
    }
}

struct EventHandlerData {
    data: EventRegistrationData,
    handler: Weak<RefCell<dyn EventHandler>>,
}

impl EventHandlerData {
    fn new(
        data: EventRegistrationData,
        handler: Weak<RefCell<dyn EventHandler>>,
    ) -> EventHandlerData {
        EventHandlerData {
            data: data,
            handler: handler,
        }
    }
}

struct RegistrationBuilder {
    fd: Pollable,
    event_mask: EventType,
}

impl RegistrationBuilder {
    fn new(fd: Pollable) -> RegistrationBuilder {
        RegistrationBuilder {
            fd: fd,
            event_mask: EventType::NONE,
        }
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
        (self.fd.clone(), self.event_mask)
    }
}

pub trait EventHandler {
    fn handle_read(&mut self, source: Pollable) -> Option<Vec<PollableOp>> {
        None
    }

    fn handle_write(&mut self, source: Pollable) -> Option<Vec<PollableOp>> {
        None
    }

    fn handle_close(&mut self, source: Pollable) -> Option<Vec<PollableOp>> {
        None
    }

    /// Initial registration of pollable objects.
    fn init(&self) -> Option<Vec<PollableOp>>;
}

struct HandlerMap {
    handlers: HashMap<i32, EventHandlerData>,
}

impl HandlerMap {
    pub fn new() -> HandlerMap {
        HandlerMap {
            handlers: HashMap::new(),
        }
    }

    pub fn insert(&mut self, id: i32, handler_data: EventHandlerData) {
        self.handlers.insert(id, handler_data);
    }

    pub fn remove(&mut self, id: &i32) -> Option<EventHandlerData> {
        self.handlers.remove(id)
    }

    // Returns a copy insrtead of ref 
    pub fn get(&mut self, id: &i32) -> Option<EventHandlerData> {
        match self.handlers.get(id) {
            Some(handler) => Some(EventHandlerData::new(
                (handler.data.0.clone(), handler.data.1),
                handler.handler.clone(),
            )),
            None => None,
        }
    }
}

impl Deref for HandlerMap {
    type Target = HashMap<i32, EventHandlerData>;
    fn deref(&self) -> &Self::Target {
       &self.handlers
    }
}

pub struct EventManager {
    fd: Pollable,
    handlers: HandlerMap,
    events: Vec<epoll::Event>,
}

impl EventManager {
    pub fn new() -> Result<SharedEventManager> {
        let epoll_fd = OwnedFD::from_unowned(epoll::create(true).map_err(|_| Error::EpollCreate)?);
        Ok(Rc::new(RefCell::new(EventManager {
            fd: epoll_fd,
            handlers: HandlerMap::new(),
            events: vec![epoll::Event::new(epoll::Events::empty(), 0); EVENT_BUFFER_SIZE],
        })))
    }

    fn event_type_to_epoll_mask(event_types: EventType) -> epoll::Events {
        let mut epoll_event_mask = epoll::Events::empty();

        if event_types.readable() {
            epoll_event_mask |= epoll::Events::EPOLLIN;
        }

        if event_types.writeable() {
            epoll_event_mask |= epoll::Events::EPOLLOUT;
        }

        if event_types.closed() {
            epoll_event_mask |= epoll::Events::EPOLLRDHUP;
        }

        epoll_event_mask
    }

    fn run_pollable_ops(&mut self, wrapped_handler: Weak<RefCell<dyn EventHandler>>, pollable_ops: Vec<PollableOp>) -> Result<()> {
        for op in pollable_ops {
            match op {
                PollableOp::Register(data) => {
                    println!("Handling a Register op");
                    let (pollable, event_type) = data;

                    if self.handlers.get(&pollable.clone()).is_some() {
                        println!("Pollable {} already registered", pollable.clone());
                        return Err(Error::AlreadyExists);
                    };

                    epoll::ctl(
                        self.fd.as_raw_fd(),
                        epoll::ControlOptions::EPOLL_CTL_ADD,
                        pollable.as_raw_fd(),
                        epoll::Event::new(
                            EventManager::event_type_to_epoll_mask(event_type),
                            **pollable as u64,
                        ),
                    )
                    .map_err(|_| Error::Poll)?;

                    let event_handler_data = EventHandlerData::new(
                        (pollable.clone(), event_type),
                        wrapped_handler.clone(),
                    );

                    self.handlers.insert(**pollable.clone(), event_handler_data);
                },
                PollableOp::Unregister(data) => {
                    println!("Handling a Unregister op");
                    self.unregister(data.0)?;
                }
                _ => ()
            }
        }
        Ok(())
    }

    pub fn register<T: EventHandler + 'static>(&mut self, handler: T) -> Result<Rc<RefCell<T>>> {        
        let pollable_ops = handler.init();
        let wrapped_type = Rc::new(RefCell::new(handler));
        let wrapped_handler: Rc<RefCell<dyn EventHandler>> = wrapped_type.clone();
        
        if let Some(ops) = pollable_ops {
            self.run_pollable_ops(Rc::downgrade(&wrapped_handler), ops);
        }

        Ok(wrapped_type)
    }

    pub fn update(&mut self, pollable: Pollable, event_types: EventType) -> Result<()> {
        if let Some(event_handler_data) = self.handlers.get(&**pollable) {
            epoll::ctl(
                self.fd.as_raw_fd(),
                epoll::ControlOptions::EPOLL_CTL_MOD,
                pollable.as_raw_fd(),
                epoll::Event::new(
                    EventManager::event_type_to_epoll_mask(event_types),
                    **pollable as u64,
                ),
            )
            .map_err(|_| Error::Poll)?;
        } else {
            println!("Pollable ID {} not found", pollable);
            return Err(Error::PollableNotFound(pollable));
        }

        Ok(())
    }

    pub fn unregister(&mut self, pollable: Pollable) -> Result<()> {
        match self.handlers.remove(&pollable) {
            Some(event_handler_data) => {
                epoll::ctl(
                    self.fd.as_raw_fd(),
                    epoll::ControlOptions::EPOLL_CTL_DEL,
                    pollable.as_raw_fd(),
                    epoll::Event::new(epoll::Events::empty(), 0),
                )
                .map_err(|_| Error::Poll);
            }
            None => {
                println!("Pollable id {} not found", pollable);
                return Err(Error::PollableNotFound(pollable));
            }
        }
        Ok(())
    }

    fn dispatch_event(
        &mut self,
        source: Pollable,
        evset: epoll::Events,
        mut handler: RefMut<'_, dyn EventHandler>,
        wrapped_handler: Weak<RefCell<dyn EventHandler>>
    ) {
        if evset.contains(epoll::Events::EPOLLIN) {
            if let Some(ops) = handler.handle_read(source.clone()) {
                self.run_pollable_ops(wrapped_handler.clone(), ops);
            }
        }

        if evset.contains(epoll::Events::EPOLLOUT) {
            if let Some(ops) = handler.handle_write(source.clone()) {
                self.run_pollable_ops(wrapped_handler.clone(), ops);
            }
        }

        if evset.contains(epoll::Events::EPOLLRDHUP) {
            if let Some(ops) = handler.handle_close(source.clone()) {
                self.run_pollable_ops(wrapped_handler.clone(), ops);
            }
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

            let mut delete_handler = None;
            if let Some(event_handler_data) = self.handlers.get(&(event_data as i32)) {
                if let Some(handler) = event_handler_data.handler.upgrade() {
                    match handler.try_borrow_mut() {
                        Ok(handler_ref) => {
                            self.dispatch_event(
                                event_handler_data.data.0.clone(),
                                evset,
                                handler_ref,
                                event_handler_data.handler,
                            );
                        }
                        Err(e) => {
                            println!("Failed to borrow mutable handler: {}", e);
                            delete_handler = Some(event_handler_data.data.0.clone());
                        }
                    }
                } else {
                    println!("Cannot upgrade weak handler for event id {}.", event_data);
                }
            }

            if let Some(handler) = delete_handler {
                self.unregister(handler);
            }
        }
    }

    pub fn run(&mut self) -> Result<usize> {
        let event_count =
            epoll::wait(self.fd.as_raw_fd(), -1, &mut self.events[..]).map_err(|_| Error::Poll)?;
        self.process_events(event_count);

        Ok(event_count)
    }

    pub fn run_timed(&mut self, milliseconds: i32) -> Result<usize> {
        let event_count = epoll::wait(self.fd.as_raw_fd(), milliseconds, &mut self.events[..])
            .map_err(|_| Error::Poll)?;
        self.process_events(event_count);

        Ok(event_count)
    }
}

pub struct ChatServer {
    clients: Vec<TcpStream>,
    listener: TcpListener,
}

impl ChatServer {
    fn new(port: u16) -> ChatServer {
        let tcp_server = TcpListener::bind(format!("127.0.0.1:{}", port)).unwrap();
        tcp_server.set_nonblocking(true);

        ChatServer {
            clients: Vec::new(),
            listener: tcp_server,
        }
    }
}

impl EventHandler for ChatServer {
    fn init(&self) -> Option<Vec<PollableOp>> {
        let mut info = Vec::new();

        // info = self
        //     .clients
        //     .iter()
        //     .map(|client| {
        //         RegistrationBuilder::new(OwnedFD::from_unowned(client.as_raw_fd()))
        //             .readable()
        //             .closeable()
        //             .finish()
        //     })
        //     .collect();

        info.push(
            PollableOp::Register(RegistrationBuilder::new(OwnedFD::from_unowned(self.listener.as_raw_fd()))
                .readable()
                .finish(),
        ));
        
        Some(info)
    }

    fn handle_read(&mut self, source: Pollable) -> Option<Vec<PollableOp>>  {
        if **source == self.listener.as_raw_fd() {
            match self.listener.accept() {
                Ok((stream, addr)) => {
                    let new_pollable = OwnedFD::from_unowned(stream.as_raw_fd());
                    self.clients.push(stream);
                    println!("New client connected: {:?}", addr);

                    return Some(vec![PollableOp::Register(RegistrationBuilder::new(new_pollable)
                            .readable()
                            .closeable()
                            .finish(),
                    )]);
                }
                Err(e) => {
                    println!("couldn't get client: {:?}", e);
                }
            }
        } else {
            if let Some(index) = self
                .clients
                .iter()
                .position(|pollable| pollable.as_raw_fd() == source.as_raw_fd())
            {
                let mut read_buf: Vec<u8> = vec![0; 128];
                self.clients[index].read(read_buf.as_mut());

                let src_addr = self.clients[index].peer_addr().unwrap();
                let message = format!("{} said {}", src_addr, String::from_utf8_lossy(read_buf.as_slice()));
                
                for mut client in &self.clients {
                    client.write(message.as_bytes());
                }

                //self.clients[index].write(&read_buf);
            }
        }
        None
    }

    fn handle_close(&mut self, source: Pollable) -> Option<Vec<PollableOp>> {
        if let Some(index) = self
                .clients
                .iter()
                .position(|pollable| pollable.as_raw_fd() == source.as_raw_fd())
        {
            return Some(vec![PollableOp::Unregister(RegistrationBuilder::new(source).finish())]);
        }   
        None 
    }
}

// struct UdpEchoServer {
//     sockets: Vec<UdpSocket>,
//     fds: Vec<Pollable>,
//     read_buf: Vec<u8>,
//     total_rx: usize,
//     total_tx: usize,
// }

// impl UdpEchoServer {
//     fn new(port: u16) -> std::io::Result<UdpEchoServer> {
//         let mut sockets = Vec::new();
//         let mut fds = Vec::new();

//         let mut socket = UdpSocket::bind(format!("127.0.0.1:{}", port))?;
//         fds.push(OwnedFD::from_unowned(socket.as_raw_fd()));
//         sockets.push(socket);
//         socket = UdpSocket::bind(format!("127.0.0.1:{}", port + 1))?;
//         fds.push(OwnedFD::from_unowned(socket.as_raw_fd()));
//         sockets.push(socket);

//         Ok(UdpEchoServer {
//             sockets: sockets,
//             fds: fds,
//             read_buf: vec![0; 128],
//             total_rx: 0,
//             total_tx: 0,
//         })
//     }

//     pub fn stats(&self) {
//         println!("Rx bytes: {}, Tx bytes: {}", self.total_rx, self.total_tx);
//     }
// }

// impl EventHandler for UdpEchoServer {
//     fn handle_read(&mut self, source: Pollable) {
//         let mut socket_iter = self.fds.iter();

//         match socket_iter.position(|pollable| pollable.as_raw_fd() == source.as_raw_fd()) {
//             Some(index) => {
//                 match self.sockets[index].recv_from(self.read_buf.as_mut()) {
//                     Ok((count, from)) => {
//                         self.total_rx += count;
//                         println!("event_info: {}, - received {} from {}", *source, count, from);
//                         match self.sockets[index].send_to(&self.read_buf[..count], from) {
//                                 Ok(tx) => self.total_tx += tx,
//                                 Err(e) => println!("Error while doing TX: {}", e)
//                         }
//                     },
//                     Err(e) => println!("Error while doing RX: {}", e)
//                 }
//             },
//             None => println!("Unable to find pollable")
//         }
//     }
//     fn handle_write(&mut self, _source: Pollable) {
//         println!("Writeable")
//     }

//     fn registration_info(&self) -> Vec<EventRegistrationData> {
//         let mut info = Vec::new();
//         for (i, fd) in self.fds.iter().enumerate() {
//             info.push(RegistrationBuilder::new(fd.clone())
//             .readable()
//             .finish())
//         }
//         info
//     }
// }

fn main() {
    let port = env::args_os()
        .nth(1)
        .unwrap_or(OsString::from("1984"))
        .into_string()
        .unwrap()
        .parse::<u16>()
        .unwrap();

    println!("Running ePolly with TCP server on port {}...", port);
    let mut wrapped_em = EventManager::new().unwrap();
    let server = ChatServer::new(port);
    let wrapped_server = wrapped_em.borrow_mut().register(server).unwrap();

    loop {
        let server = wrapped_server.borrow();
        //server.stats();
        drop(server);
        wrapped_em.borrow_mut().run_timed(1000).unwrap();
    }
}
