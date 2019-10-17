// Copyright 2019 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

use std::fmt::Formatter;
use std::os::unix::io::{AsRawFd, RawFd};
use std::rc::{Rc};
use std::ops::Deref;
use std::io;

#[derive(Default, Clone, PartialEq)]
pub struct OwnedFD {
    fd: RawFd,
}

impl OwnedFD {
    fn dup(fd: RawFd) -> Result<RawFd, io::Error> {
        unsafe { 
            match libc::dup(fd) {
                fd if fd < 0 => Err(io::Error::last_os_error()),
                _ => Ok(fd)
            }
        }
    }

    pub fn from_unowned(rawfd: RawFd) -> Result<Pollable, io::Error> {
        Ok(Rc::new(OwnedFD { fd: OwnedFD::dup(rawfd)? }))
    }

    pub fn from<T: AsRawFd>(rawfd: &T) -> Result<Pollable, io::Error> {
        Ok(Rc::new(OwnedFD {
            fd: OwnedFD::dup(rawfd.as_raw_fd())?,
        }))
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

pub type EventRegistrationData = (Pollable, EventType);
pub type Pollable = Rc<OwnedFD>;

pub enum PollableOp {
    Register(EventRegistrationData),
    Unregister(EventRegistrationData),
    Update(EventRegistrationData),
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
}

impl std::fmt::Display for OwnedFD {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        write!(f, "{}", self.fd)
    }
}

pub struct PollableOpBuilder {
    fd: Pollable,
    event_mask: EventType,
}

impl PollableOpBuilder {
    pub fn new(fd: Pollable) -> PollableOpBuilder {
        PollableOpBuilder {
            fd: fd,
            event_mask: EventType::NONE,
        }
    }

    pub fn readable<'a>(&'a mut self) -> &'a mut PollableOpBuilder {
        self.event_mask |= EventType::READ;
        self
    }

    pub fn writeable<'a>(&'a mut self) -> &'a mut PollableOpBuilder {
        self.event_mask |= EventType::WRITE;
        self
    }

    pub fn closeable<'a>(&'a mut self) -> &'a mut PollableOpBuilder {
        self.event_mask |= EventType::CLOSE;
        self
    }

    pub fn register(&self) -> PollableOp {
        PollableOp::Register((self.fd.clone(), self.event_mask))
    }

    pub fn unregister(&self) -> PollableOp {
        PollableOp::Unregister((self.fd.clone(), self.event_mask))
    }

    pub fn update(&self) -> PollableOp {
        PollableOp::Update((self.fd.clone(), self.event_mask))
    }
}