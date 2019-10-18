// Copyright 2019 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

use epoll::Events;
use std::cell::{RefCell, RefMut};
use std::collections::HashMap;
use std::fmt::Formatter;
use std::os::unix::io::{AsRawFd};
use std::rc::{Rc, Weak};
use std::ops::Deref;
use pollable::{ Pollable, PollableOp, OwnedFD, EventRegistrationData, EventType, PollableOpBuilder };
use std::io;

const EVENT_BUFFER_SIZE: usize = 100;
const DEFAULT_EPOLL_TIMEOUT: i32 = 250;

type Result<T> = std::result::Result<T, Error>;

pub enum Error {
    EpollCreate,
    Poll(io::Error),
    AlreadyExists,
    PollableNotFound(Pollable),
}

impl std::fmt::Debug for Error {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        use self::Error::*;

        match self {
            EpollCreate => write!(f, "Unable to create epoll fd."),
            Poll(err) => write!(f, "Error during epoll call: {}",err),
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

pub trait EventHandler {
    fn handle_read(&mut self, _source: Pollable) -> Option<Vec<PollableOp>> {
        None
    }

    fn handle_write(&mut self, _source: Pollable) -> Option<Vec<PollableOp>> {
        None
    }

    fn handle_close(&mut self, _source: Pollable) -> Option<Vec<PollableOp>> {
        None
    }

    fn handle_error(&mut self, _source: Pollable) -> Option<Vec<PollableOp>> {
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

    // Returns a copy instead of ref
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
    pub fn new() -> Result<EventManager> {
        let epoll_fd = OwnedFD::from_unowned(epoll::create(true).map_err(|_| Error::EpollCreate)?).map_err(|_| Error::EpollCreate).unwrap();
        Ok(EventManager {
            fd: epoll_fd,
            handlers: HandlerMap::new(),
            events: vec![epoll::Event::new(epoll::Events::empty(), 0); EVENT_BUFFER_SIZE],
        })
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

    fn register_handler(&mut self, event_data: EventRegistrationData, wrapped_handler: Weak<RefCell<dyn EventHandler>>) -> Result<()> {
        let (pollable, event_type) = event_data;

        if let Some(_) = self.handlers.get(&pollable.clone()) {
            println!("Pollable {} already registered", pollable.clone());
            return Err(Error::AlreadyExists);
        };

        epoll::ctl(
            self.fd.as_raw_fd(),
            epoll::ControlOptions::EPOLL_CTL_ADD,
            // Use the duplicate fd for event polling.
            pollable.dup_fd(),
            epoll::Event::new(
                EventManager::event_type_to_epoll_mask(event_type),
                // Use the original fd for event source identification.
                **pollable as u64,
            ),
        )
        .map_err(|_| Error::Poll(io::Error::last_os_error()))?;

        let event_handler_data = EventHandlerData::new(
            (pollable.clone(), event_type),
            wrapped_handler.clone(),
        );

        self.handlers.insert(**pollable.clone(), event_handler_data);
        Ok(())
    }

    // Update an event handler subscription.
    pub fn update(
        &mut self,
        wrapped_handler: Weak<RefCell<dyn EventHandler>>,
        pollable_ops: Vec<PollableOp>,
    ) -> Result<()> {
        self.run_pollable_ops(wrapped_handler, pollable_ops)
    }

    fn run_pollable_ops(
        &mut self,
        wrapped_handler: Weak<RefCell<dyn EventHandler>>,
        pollable_ops: Vec<PollableOp>,
    ) -> Result<()> {
        for op in pollable_ops {
            match op {
                PollableOp::Register(data) => self.register_handler(data, wrapped_handler.clone())?,
                PollableOp::Unregister(data) => self.unregister(data.0)?,
                PollableOp::Update(data) => self.update_event(data)?,
            }
        }
        Ok(())
    }

    // Register a new event handler.
    pub fn register<T: EventHandler + 'static>(&mut self, handler: T) -> Result<Rc<RefCell<T>>> {
        let pollable_ops = handler.init();
        let wrapped_type = Rc::new(RefCell::new(handler));
        let wrapped_handler: Rc<RefCell<dyn EventHandler>> = wrapped_type.clone();

        if let Some(ops) = pollable_ops {
            self.run_pollable_ops(Rc::downgrade(&wrapped_handler), ops)?;
        }

        Ok(wrapped_type)
    }

    fn update_event(&mut self, event: EventRegistrationData) -> Result<()> {
        if let Some(_) = self.handlers.get(&**event.0) {
            epoll::ctl(
                self.fd.as_raw_fd(),
                epoll::ControlOptions::EPOLL_CTL_MOD,
                event.0.dup_fd(),
                epoll::Event::new(
                    EventManager::event_type_to_epoll_mask(event.1),
                    **event.0 as u64,
                ),
            )
            .map_err(|_| Error::Poll(io::Error::last_os_error()))?;
        } else {
            println!("Pollable ID {} not found", event.0);
            return Err(Error::PollableNotFound(event.0));
        }

        Ok(())
    }

    fn unregister(&mut self, pollable: Pollable) -> Result<()> {
        match self.handlers.remove(&pollable) {
            Some(_) => {
                epoll::ctl(
                    self.fd.as_raw_fd(),
                    epoll::ControlOptions::EPOLL_CTL_DEL,
                    pollable.dup_fd(),
                    epoll::Event::new(epoll::Events::empty(), 0),
                )
                .map_err(|_| Error::Poll(io::Error::last_os_error()))?;
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
        wrapped_handler: Weak<RefCell<dyn EventHandler>>,
    ) -> Result<()> {
        let mut all_ops = Vec::new();
        
        // If an error occurs on a fd then only dispatch the error callback,
        // ignoring other flags.
        if evset.contains(epoll::Events::EPOLLERR) {
            if let Some(mut ops) = handler.handle_error(source.clone()) {
                all_ops.append(&mut ops);
            }
        } else {
            if evset.contains(epoll::Events::EPOLLIN) {
                if let Some(mut ops) = handler.handle_read(source.clone()) {
                    all_ops.append(&mut ops);
                }
            }
            if evset.contains(epoll::Events::EPOLLOUT) {
                if let Some(mut ops) = handler.handle_write(source.clone()) {
                    all_ops.append(&mut ops);
                }
            }
            if evset.contains(epoll::Events::EPOLLRDHUP) {
                if let Some(mut ops) = handler.handle_close(source.clone()) {
                    all_ops.append(&mut ops);
                }
            }
        }

        self.run_pollable_ops(wrapped_handler.clone(), all_ops)?;
        Ok(())
    }

    fn process_events(&mut self, event_count: usize) -> Result<()> {
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
                            )?;
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
                self.unregister(handler)?;
            }
        }
        Ok(())
    }

    // Wait for events, then dispatch to registered event handlers.
    pub fn run(&mut self) -> Result<usize> {
        let event_count =
            epoll::wait(self.fd.as_raw_fd(), -1, &mut self.events[..]).map_err(|_| Error::Poll(io::Error::last_os_error()))?;
        self.process_events(event_count)?;

        Ok(event_count)
    }

    // Wait for events or a timeout, then dispatch to registered event handlers. 
    pub fn run_timeout(&mut self, milliseconds: i32) -> Result<usize> {
        let event_count = epoll::wait(self.fd.as_raw_fd(), milliseconds, &mut self.events[..])
            .map_err(|_| Error::Poll(io::Error::last_os_error()))?;
        self.process_events(event_count)?;

        Ok(event_count)
    }
}

// Cascaded epoll support.
impl EventHandler for EventManager {
    fn handle_read(&mut self, _source: Pollable) -> Option<Vec<PollableOp>> {
        match self.run_timeout(DEFAULT_EPOLL_TIMEOUT) {
            Ok(_) => None,
            Err(_) => None,
        }
    }

    fn init(&self) -> Option<Vec<PollableOp>> {
       Some(vec![PollableOpBuilder::new(OwnedFD::from(&*self.fd).unwrap())
            .readable()
            .register()])
    }
}