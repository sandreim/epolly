extern crate epoll;
extern crate libc;
#[macro_use]
extern crate bitflags;

use std::net::{TcpListener, TcpStream, SocketAddr};

mod event_manager;
mod pollable;

use std::os::unix::io::{AsRawFd, RawFd};
use pollable::*;
use event_manager::*;
use std::ffi::OsString;
use std::env;
use std::io::{Read, Write};
use std::ops::{ Deref, DerefMut };
use std::fmt;

pub struct ChatClient {
    stream: TcpStream,
    addr: SocketAddr,
}

impl ChatClient {
    pub fn new(stream: TcpStream, addr: SocketAddr) -> ChatClient{
        ChatClient {
            stream: stream,
            addr: addr,
        }
    }

    pub fn greet(&mut self) -> std::io::Result<usize> {
        let message = format!("Welcome {} to the example Polly chatserver\r\n", &self);
        self.stream.write(message.as_bytes())
    }
}

impl fmt::Display for ChatClient {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self.addr)
    }
}

impl Deref for ChatClient {
    type Target = TcpStream;
    fn deref(&self) -> &Self::Target {
        &self.stream
    }
}

impl DerefMut for ChatClient {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.stream
    }
}

pub struct ChatServer {
    clients: Vec<ChatClient>,
    listener: TcpListener,
    rx: u64,
    tx: u64,
}

impl ChatServer {
    fn new(port: u16) -> ChatServer {
        let tcp_server = TcpListener::bind(format!("127.0.0.1:{}", port)).unwrap();
        tcp_server.set_nonblocking(true);

        ChatServer {
            clients: Vec::new(),
            listener: tcp_server,
            rx: 0,
            tx: 0,
        }
    }

    pub fn stats(&self) {
        println!("Total Rx: {}, total Tx: {}", self.rx, self.tx);
    }
}

impl EventHandler for ChatServer {
    fn init(&self) -> Option<Vec<PollableOp>> {
        let mut info = Vec::new();

        info.push(
            PollableOpBuilder::new(OwnedFD::from(&self.listener).unwrap())
                .readable()
                .register(),
        );

        Some(info)
    }

    fn handle_read(&mut self, source: Pollable) -> Option<Vec<PollableOp>> {
        if source.as_raw_fd() == self.listener.as_raw_fd() {
            match self.listener.accept() {
                Ok((stream, addr)) => {
                    let new_pollable = OwnedFD::from(&stream).unwrap();
                    let mut client = ChatClient::new(stream, addr);
                    println!("Client connected {}", &client);
                    match client.greet() {
                        Ok(count) => self.tx += count as u64,
                        Err(err) => println!("Error writing to client socket {}", err)
                    }
                    self.clients.push(client);

                    return Some(vec![PollableOpBuilder::new(new_pollable)
                        .readable()
                        .closeable()
                        .register()]);
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

                if let Ok(count) = self.clients[index].read(read_buf.as_mut()) {
                    if count <= 0 {
                        return None;
                    }

                    self.rx += count as u64;
                    let message = format!(
                        "{} said {}",
                        self.clients[index],
                        String::from_utf8_lossy(&read_buf.as_slice()[0..count])
                    );

                    for client in self.clients.iter_mut() {
                        client.write(message.as_bytes());
                        self.tx += message.len() as u64;
                    }
                }
            }
        }
        None
    }

    fn handle_close(&mut self, source: Pollable) -> Option<Vec<PollableOp>> {
        if let Some(index) = self
            .clients
            .iter()
            .position(|client| client.as_raw_fd() == source.as_raw_fd())
        {
            println!("Client {} closed connection.", self.clients.remove(index));
            return Some(vec![PollableOpBuilder::new(source).unregister()]);
        }
        None
    }

    fn handle_error(&mut self, source: Pollable) -> Option<Vec<PollableOp>>  {
        if let Some(index) = self
            .clients
            .iter()
            .position(|client| client.as_raw_fd() == source.as_raw_fd())
        {
            let client = self.clients.remove(index);
            println!("Closing connection for client {} : {}", source, client.take_error().unwrap().unwrap());
            return Some(vec![PollableOpBuilder::new(source).unregister()]);
        }
        None
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

    println!("Running ePolly with TCP server on port {}...", port);
    let mut em = EventManager::new().unwrap();
    let mut em2 = EventManager::new().unwrap();

    let server = ChatServer::new(port);
    let wrapped_server = em2.register(server).unwrap();
    let wrapped_em2 = em.register(em2);

    loop {
        let server = wrapped_server.borrow();
        server.stats();
        drop(server);
        match em.run_timeout(1000) {
            Ok(_count) => {},
            Err(err) => println!("Error: {:?}", err)
        }
    }
}
