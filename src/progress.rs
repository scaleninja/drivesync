// SPDX-License-Identifier: MIT
// Copyright (c) 2026 ScaleNinja
// DriveSync (dsync) — https://github.com/scaleninja/drivesync

//! A dependency-free terminal spinner plus console-safe printing for worker threads.
//!
//! One global console lock serialises spinner redraws and worker output, so a completed-file line
//! never lands in the middle of a spinner frame. Without a TTY on stderr the spinner is silent.
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

static CONSOLE: Mutex<()> = Mutex::new(());
/// True while a spinner frame is on screen and must be erased before printing anything else.
static DRAWN: AtomicBool = AtomicBool::new(false);
const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn clear_line() {
    if DRAWN.swap(false, Ordering::SeqCst) {
        eprint!("\r\x1b[2K");
    }
}

/// Print a line to stdout without corrupting the spinner.
pub fn println(line: &str) {
    let _g = CONSOLE.lock().unwrap();
    clear_line();
    println!("{line}");
}

/// Print a line to stderr without corrupting the spinner.
pub fn eprintln(line: &str) {
    let _g = CONSOLE.lock().unwrap();
    clear_line();
    eprintln!("{line}");
}

pub struct Spinner {
    stop: Arc<AtomicBool>,
    msg: Arc<Mutex<String>>,
    handle: Option<JoinHandle<()>>,
}

impl Spinner {
    /// Start spinning with `msg`; does nothing visible when stderr is not a terminal.
    pub fn start(msg: &str) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let msg = Arc::new(Mutex::new(msg.to_string()));
        let handle = std::io::stderr().is_terminal().then(|| {
            let (stop, msg) = (stop.clone(), msg.clone());
            std::thread::spawn(move || {
                let mut i = 0;
                while !stop.load(Ordering::SeqCst) {
                    {
                        let _g = CONSOLE.lock().unwrap();
                        eprint!(
                            "\r\x1b[2K{} {}",
                            FRAMES[i % FRAMES.len()],
                            msg.lock().unwrap()
                        );
                        DRAWN.store(true, Ordering::SeqCst);
                    }
                    i += 1;
                    std::thread::sleep(Duration::from_millis(80));
                }
                let _g = CONSOLE.lock().unwrap();
                clear_line();
            })
        });
        Self { stop, msg, handle }
    }

    pub fn set(&self, msg: String) {
        *self.msg.lock().unwrap() = msg;
    }

    /// Stop and erase the spinner.
    pub fn finish(mut self) {
        self.stop_and_join();
    }

    fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}
