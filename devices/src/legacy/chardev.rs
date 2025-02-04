// Copyright (c) 2020 Huawei Technologies Co.,Ltd. All rights reserved.
//
// StratoVirt is licensed under Mulan PSL v2.
// You can use this software according to the terms and conditions of the Mulan
// PSL v2.
// You may obtain a copy of Mulan PSL v2 at:
//         http://license.coscl.org.cn/MulanPSL2
// THIS SOFTWARE IS PROVIDED ON AN "AS IS" BASIS, WITHOUT WARRANTIES OF ANY
// KIND, EITHER EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO
// NON-INFRINGEMENT, MERCHANTABILITY OR FIT FOR A PARTICULAR PURPOSE.
// See the Mulan PSL v2 for more details.

use std::fs::{read_link, File, OpenOptions};
use std::io::{Stdin, Stdout};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use libc::{cfmakeraw, tcgetattr, tcsetattr, termios};
use machine_manager::machine::{PathInfo, PTY_PATH};
use machine_manager::{
    config::{ChardevConfig, ChardevType},
    temp_cleaner::TempCleaner,
};
use util::loop_context::{EventNotifier, EventNotifierHelper, NotifierCallback, NotifierOperation};
use util::set_termi_raw_mode;
use util::unix::limit_permission;
use vmm_sys_util::epoll::EventSet;

use super::errors::{Result, ResultExt};

/// Provide the trait that helps handle the input data.
pub trait InputReceiver: Send {
    fn input_handle(&mut self, buffer: &[u8]);

    fn get_remain_space_size(&mut self) -> usize;
}

/// Character device structure.
pub struct Chardev {
    /// Id of chardev.
    pub id: String,
    /// Type of backend device.
    pub backend: ChardevType,
    /// UnixListener for socket-type chardev.
    pub listener: Option<UnixListener>,
    /// Chardev input.
    pub input: Option<Arc<Mutex<dyn CommunicatInInterface>>>,
    /// Chardev output.
    pub output: Option<Arc<Mutex<dyn CommunicatOutInterface>>>,
    /// Fd of socket stream.
    pub stream_fd: Option<i32>,
    /// Handle the input data and trigger interrupt if necessary.
    receive: Option<Arc<dyn Fn(&[u8]) + Send + Sync>>,
    /// Return the remain space size of receiver buffer.
    get_remain_space_size: Option<Arc<dyn Fn() -> usize + Send + Sync>>,
}

impl Chardev {
    pub fn new(chardev_cfg: ChardevConfig) -> Self {
        Chardev {
            id: chardev_cfg.id,
            backend: chardev_cfg.backend,
            listener: None,
            input: None,
            output: None,
            stream_fd: None,
            receive: None,
            get_remain_space_size: None,
        }
    }

    pub fn realize(&mut self) -> Result<()> {
        match &self.backend {
            ChardevType::Stdio => {
                set_termi_raw_mode().chain_err(|| "Failed to set terminal to raw mode")?;
                self.input = Some(Arc::new(Mutex::new(std::io::stdin())));
                self.output = Some(Arc::new(Mutex::new(std::io::stdout())));
            }
            ChardevType::Pty => {
                let (master, path) =
                    set_pty_raw_mode().chain_err(|| "Failed to set pty to raw mode")?;
                info!("Pty path is: {:?}", path);
                let path_info = PathInfo {
                    path: format!("pty:{:?}", &path),
                    label: self.id.clone(),
                };
                PTY_PATH.lock().unwrap().push(path_info);
                // Safe because `master_arc` is the only one owner for the file descriptor.
                let master_arc = unsafe { Arc::new(Mutex::new(File::from_raw_fd(master))) };
                self.input = Some(master_arc.clone());
                self.output = Some(master_arc);
            }
            ChardevType::Socket(path) => {
                let sock = UnixListener::bind(path.clone())
                    .chain_err(|| format!("Failed to bind socket for chardev, path:{}", path))?;
                self.listener = Some(sock);
                // add file to temporary pool, so it could be cleaned when vm exit.
                TempCleaner::add_path(path.clone());
                limit_permission(path).chain_err(|| {
                    format!(
                        "Failed to change file permission for chardev, path:{}",
                        path
                    )
                })?;
            }
            ChardevType::File(path) => {
                let file = Arc::new(Mutex::new(
                    OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .open(path)?,
                ));
                self.output = Some(file);
            }
        };
        Ok(())
    }

    pub fn set_input_callback<T: 'static + InputReceiver>(&mut self, dev: &Arc<Mutex<T>>) {
        let cloned_dev = dev.clone();
        self.receive = Some(Arc::new(move |data: &[u8]| {
            cloned_dev.lock().unwrap().input_handle(data)
        }));
        let cloned_dev = dev.clone();
        self.get_remain_space_size = Some(Arc::new(move || {
            cloned_dev.lock().unwrap().get_remain_space_size()
        }));
    }
}

fn set_pty_raw_mode() -> Result<(i32, PathBuf)> {
    let mut master: libc::c_int = 0;
    let master_ptr: *mut libc::c_int = &mut master;
    let mut slave: libc::c_int = 0;
    let slave_ptr: *mut libc::c_int = &mut slave;
    // Safe because this only create a new pseudoterminal and set the master and slave fd.
    let ret = {
        unsafe {
            libc::openpty(
                master_ptr,
                slave_ptr,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        }
    };
    if ret < 0 {
        bail!(
            "Failed to open pty, error is {}",
            std::io::Error::last_os_error()
        )
    }
    let proc_path = PathBuf::from(format!("/proc/self/fd/{}", slave));
    let path = read_link(proc_path).chain_err(|| "Failed to read slave pty link")?;
    // Safe because this only set the `old_termios` struct to zero.
    let mut old_termios: termios = unsafe { std::mem::zeroed() };
    // Safe because this only get the current mode of slave pty and save it.
    let ret = unsafe { tcgetattr(slave, &mut old_termios as *mut _) };
    if ret < 0 {
        bail!(
            "Failed to get mode of pty, error is {}",
            std::io::Error::last_os_error()
        );
    }
    let mut new_termios: termios = old_termios;
    // Safe because this function only change the `new_termios` argument.
    unsafe { cfmakeraw(&mut new_termios as *mut _) };
    // Safe because this function only set the slave pty to raw mode.
    let ret = unsafe { tcsetattr(slave, libc::TCSAFLUSH, &new_termios as *const _) };
    if ret < 0 {
        bail!(
            "Failed to set pty to raw mode, error is {}",
            std::io::Error::last_os_error()
        );
    }
    Ok((master, path))
}

fn get_notifier_handler(
    chardev: Arc<Mutex<Chardev>>,
    backend: ChardevType,
) -> Box<NotifierCallback> {
    match backend {
        ChardevType::Stdio | ChardevType::Pty => Box::new(move |_, _| {
            let locked_chardev = chardev.lock().unwrap();
            let buff_size = locked_chardev.get_remain_space_size.as_ref().unwrap()();
            let mut buffer = vec![0_u8; buff_size];
            if let Some(input) = locked_chardev.input.clone() {
                if let Ok(index) = input.lock().unwrap().chr_read_raw(&mut buffer) {
                    locked_chardev.receive.as_ref().unwrap()(&mut buffer[..index]);
                } else {
                    error!("Failed to read input data");
                }
            } else {
                error!("Failed to get chardev input fd");
            }
            None
        }),
        ChardevType::Socket(_) => Box::new(move |_, _| {
            let mut locked_chardev = chardev.lock().unwrap();
            let (stream, _) = locked_chardev.listener.as_ref().unwrap().accept().unwrap();
            let listener_fd = locked_chardev.listener.as_ref().unwrap().as_raw_fd();
            let stream_fd = stream.as_raw_fd();
            locked_chardev.stream_fd = Some(stream_fd);
            let stream_arc = Arc::new(Mutex::new(stream));
            locked_chardev.input = Some(stream_arc.clone());
            locked_chardev.output = Some(stream_arc);

            let cloned_chardev = chardev.clone();
            let inner_handler = Box::new(move |event, _| {
                if event == EventSet::IN {
                    let locked_chardev = cloned_chardev.lock().unwrap();
                    let buff_size = locked_chardev.get_remain_space_size.as_ref().unwrap()();
                    let mut buffer = vec![0_u8; buff_size];
                    if let Some(input) = locked_chardev.input.clone() {
                        if let Ok(index) = input.lock().unwrap().chr_read_raw(&mut buffer) {
                            locked_chardev.receive.as_ref().unwrap()(&mut buffer[..index]);
                        } else {
                            error!("Failed to read input data");
                        }
                    } else {
                        error!("Failed to get chardev input fd");
                    }
                }
                if event & EventSet::HANG_UP == EventSet::HANG_UP {
                    cloned_chardev.lock().unwrap().input = None;
                    cloned_chardev.lock().unwrap().output = None;
                    cloned_chardev.lock().unwrap().stream_fd = None;
                    Some(vec![EventNotifier::new(
                        NotifierOperation::Delete,
                        stream_fd,
                        Some(listener_fd),
                        EventSet::IN | EventSet::HANG_UP,
                        Vec::new(),
                    )])
                } else {
                    None
                }
            });
            Some(vec![EventNotifier::new(
                NotifierOperation::AddShared,
                stream_fd,
                Some(listener_fd),
                EventSet::IN | EventSet::HANG_UP,
                vec![Arc::new(Mutex::new(inner_handler))],
            )])
        }),
        ChardevType::File(_) => Box::new(move |_, _| None),
    }
}

impl EventNotifierHelper for Chardev {
    fn internal_notifiers(chardev: Arc<Mutex<Self>>) -> Vec<EventNotifier> {
        let mut notifiers = Vec::new();
        let backend = chardev.lock().unwrap().backend.clone();
        let cloned_chardev = chardev.clone();
        match backend {
            ChardevType::Stdio | ChardevType::Pty => {
                if let Some(input) = chardev.lock().unwrap().input.clone() {
                    notifiers.push(EventNotifier::new(
                        NotifierOperation::AddShared,
                        input.lock().unwrap().as_raw_fd(),
                        None,
                        EventSet::IN,
                        vec![Arc::new(Mutex::new(get_notifier_handler(
                            cloned_chardev,
                            backend,
                        )))],
                    ));
                }
            }
            ChardevType::Socket(_) => {
                if let Some(listener) = chardev.lock().unwrap().listener.as_ref() {
                    notifiers.push(EventNotifier::new(
                        NotifierOperation::AddShared,
                        listener.as_raw_fd(),
                        None,
                        EventSet::IN,
                        vec![Arc::new(Mutex::new(get_notifier_handler(
                            cloned_chardev,
                            backend,
                        )))],
                    ));
                }
            }
            ChardevType::File(_) => (),
        }
        notifiers
    }
}

/// Provide backend trait object receiving the input from the guest.
pub trait CommunicatInInterface: std::marker::Send + std::os::unix::io::AsRawFd {
    fn chr_read_raw(&mut self, buf: &mut [u8]) -> Result<usize> {
        use libc::read;
        // Safe because this only read the bytes from terminal within the buffer.
        let ret = unsafe { read(self.as_raw_fd(), buf.as_mut_ptr() as *mut _, buf.len()) };
        if ret < 0 {
            bail!("Failed to read buffer");
        }
        Ok(ret as usize)
    }
}

/// Provide backend trait object processing the output from the guest.
pub trait CommunicatOutInterface: std::io::Write + std::marker::Send {}

impl CommunicatInInterface for UnixStream {}
impl CommunicatInInterface for File {}
impl CommunicatInInterface for Stdin {}

impl CommunicatOutInterface for UnixStream {}
impl CommunicatOutInterface for File {}
impl CommunicatOutInterface for Stdout {}
