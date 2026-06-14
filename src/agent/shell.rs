use anyhow::Context;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver};
use std::thread;

pub struct Shell {
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    output_rx: Receiver<Vec<u8>>,
    _child: Box<dyn portable_pty::Child + Send + Sync>,
    _reader_thread: thread::JoinHandle<()>,
    pub cols: u16,
    pub rows: u16,
}

impl Shell {
    pub fn spawn(cols: u16, rows: u16) -> anyhow::Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("Failed to open pty")?;

        let shell_path =
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());

        let mut cmd = CommandBuilder::new(&shell_path);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("LANG", "en_US.UTF-8");

        if let Ok(home) = std::env::var("HOME") {
            cmd.cwd(&home);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .context("Failed to spawn shell process")?;

        drop(pair.slave);

        let writer = pair
            .master
            .take_writer()
            .context("Failed to take pty master writer")?;

        let mut master_reader = pair
            .master
            .try_clone_reader()
            .context("Failed to clone pty master reader")?;

        let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>();

        let reader_thread = thread::spawn(move || {
            let mut buf = vec![0u8; 4096];
            loop {
                match master_reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if output_tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            master: pair.master,
            writer,
            output_rx,
            _child: child,
            _reader_thread: reader_thread,
            cols,
            rows,
        })
    }

    pub fn read_output(&self) -> Option<Vec<u8>> {
        match self.output_rx.try_recv() {
            Ok(data) => Some(data),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => None,
        }
    }

    pub fn write_input(&mut self, data: &[u8]) -> anyhow::Result<()> {
        self.writer
            .write_all(data)
            .context("Failed to write to pty master")?;
        self.writer
            .flush()
            .context("Failed to flush pty master")?;
        Ok(())
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> anyhow::Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("Failed to resize pty")?;
        self.cols = cols;
        self.rows = rows;
        Ok(())
    }
}
