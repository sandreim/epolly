extern crate epoll;
extern crate libc;
#[macro_use]
extern crate bitflags;

use std::net::{TcpListener, TcpStream};

mod event_manager;
mod pollable;

use std::os::unix::io::{AsRawFd, RawFd};
use pollable::*;
use event_manager::*;
use std::ffi::OsString;
use std::env;
use std::io::{Read, Write};

pub struct ChatServer {
    clients: Vec<TcpStream>,
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
        if **source == self.listener.as_raw_fd() {
            match self.listener.accept() {
                Ok((stream, addr)) => {
                    let new_pollable = OwnedFD::from(&stream).unwrap();
                    self.clients.push(stream);

                    println!("New client connected: {:?}", addr);

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

                    let src_addr = self.clients[index].peer_addr().unwrap();
                    self.rx += count as u64;
                    let message = format!(
                        "{} said {}",
                        src_addr,
                        String::from_utf8_lossy(&read_buf.as_slice()[0..count])
                    );

                    for mut client in &self.clients {
                        client.write(message.as_bytes());
                        self.tx += message.len() as u64;
                    }
                }
            }
        }
        None
    }

    fn handle_close(&mut self, source: Pollable) -> Option<Vec<PollableOp>> {
        if let Some(_) = self
            .clients
            .iter()
            .position(|pollable| pollable.as_raw_fd() == source.as_raw_fd())
        {
            return Some(vec![PollableOpBuilder::new(source).unregister()]);
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
//             info.push(PollableOpBuilder::new(fd.clone())
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
    let mut em = EventManager::new().unwrap();
    let mut em2 = EventManager::new().unwrap();

    let server = ChatServer::new(port);
    let wrapped_server = em2.register(server).unwrap();
    let wrapped_em2 = em.register(em2);

    loop {
        let server = wrapped_server.borrow();
        server.stats();
        drop(server);
        em.run_timeout(1000).unwrap();
    }
}
