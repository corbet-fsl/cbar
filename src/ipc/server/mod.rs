mod bar;
mod ironvar;
mod style;

use std::fs;
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use color_eyre::{Report, Result};
use gtk::Application;
use gtk::prelude::*;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::time::timeout;
use tracing::{debug, error, info, trace, warn};

use super::Ipc;
use crate::channels::{AsyncSenderExt, MpscReceiverExt};
use crate::ipc::{Command, Response};
use crate::{Ironbar, spawn};

const MAX_COMMAND_BYTES: u64 = 64 * 1024;
const COMMAND_READ_TIMEOUT: Duration = Duration::from_secs(2);

impl Ipc {
    /// Starts the IPC server on its socket.
    ///
    /// Once started, the server will begin accepting connections.
    pub fn start(&self, application: &Application, ironbar: Rc<Ironbar>) {
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let (res_tx, mut res_rx) = mpsc::channel(32);

        let path = self.path.clone();

        if path.exists() {
            warn!("Socket already exists. Did Cbar exit abruptly?");
            warn!("Attempting IPC shutdown to allow binding to address");
            if let Err(err) = Self::remove_socket(&path) {
                error!(
                    "Refusing to replace the existing IPC path {}: {err}",
                    path.display()
                );
                return;
            }
        }

        spawn(async move {
            info!("Starting IPC on {}", path.display());

            let (listener, owner_uid) = match Self::bind_listener(&path) {
                Ok(listener) => listener,
                Err(err) => {
                    error!(
                        "{:?}",
                        Report::new(err).wrap_err("Unable to start IPC server")
                    );
                    return;
                }
            };

            loop {
                match listener.accept().await {
                    Ok((stream, _addr)) => {
                        let credentials = match stream.peer_cred() {
                            Ok(credentials) => credentials,
                            Err(err) => {
                                warn!("Unable to authenticate IPC peer: {err}");
                                continue;
                            }
                        };

                        if credentials.uid() != owner_uid {
                            warn!(
                                "Rejected IPC peer with uid {} (expected {owner_uid})",
                                credentials.uid()
                            );
                            continue;
                        }

                        debug!("handling incoming connection");
                        if let Err(err) =
                            Self::handle_connection(stream, &cmd_tx, &mut res_rx).await
                        {
                            error!("{err:?}");
                        }
                        debug!("done");
                    }
                    Err(err) => {
                        error!("{err:?}");
                    }
                }
            }
        });

        cmd_rx.recv_glib(application, move |application, command| {
            let res = Self::handle_command(command, application, &ironbar);
            res_tx.send_spawn(res);
        });
    }

    /// Takes an incoming connections,
    /// reads the command message, and sends the response.
    ///
    /// The connection is closed once the response has been written.
    async fn handle_connection(
        mut stream: UnixStream,
        cmd_tx: &Sender<Command>,
        res_rx: &mut Receiver<Response>,
    ) -> Result<()> {
        trace!("awaiting readable state");
        stream.readable().await?;

        let mut read_buffer = Vec::with_capacity(1024);

        let mut reader = BufReader::new(&mut stream).take(MAX_COMMAND_BYTES + 1);

        trace!("reading bytes");
        let bytes = timeout(
            COMMAND_READ_TIMEOUT,
            reader.read_until(b'\n', &mut read_buffer),
        )
        .await
        .map_err(|_| color_eyre::eyre::eyre!("Timed out reading IPC command"))??;
        debug!("read {} bytes", bytes);

        if bytes == 0 {
            return Err(color_eyre::eyre::eyre!("IPC peer closed without a command"));
        }

        if bytes as u64 > MAX_COMMAND_BYTES || !read_buffer.ends_with(b"\n") {
            return Err(color_eyre::eyre::eyre!(
                "IPC command exceeds the {MAX_COMMAND_BYTES}-byte limit"
            ));
        }

        // FIXME: Error on invalid command
        let command = serde_json::from_slice::<Command>(&read_buffer[..bytes])?;

        debug!("Received command: {command:?}");

        cmd_tx.send_expect(command).await;
        let res = res_rx
            .recv()
            .await
            .unwrap_or(Response::Err { message: None });

        let mut res = serde_json::to_vec(&res)?;
        res.push(b'\n');

        trace!("awaiting writable state");
        stream.writable().await?;

        debug!("writing {} bytes", res.len());
        stream.write_all(&res).await?;

        trace!("bytes written, shutting down stream");
        stream.shutdown().await?;

        Ok(())
    }

    /// Takes an input command, runs it and returns with the appropriate response.
    ///
    /// This runs on the main thread, allowing commands to interact with GTK.
    fn handle_command(
        command: Command,
        application: &Application,
        ironbar: &Rc<Ironbar>,
    ) -> Response {
        match command {
            Command::Ping => Response::Ok,
            Command::Inspect => {
                gtk::Window::set_interactive_debugging(true);
                Response::Ok
            }
            Command::Reload => {
                info!("Closing existing bars");
                ironbar.bars.borrow_mut().clear();

                let windows = application.windows();
                for window in windows {
                    #[cfg(feature = "matrix_launcher")]
                    if ironbar
                        .matrix_launcher()
                        .is_some_and(|launcher| launcher.owns_window(&window))
                    {
                        continue;
                    }
                    window.close();
                }

                ironbar.reload_config();

                match crate::load_output_bars(ironbar, application) {
                    Ok(()) => {}
                    Err(err) => error!("{err:?}"),
                }
                Response::Ok
            }
            Command::Var(cmd) => ironvar::handle_command(cmd),
            Command::Bar(cmd) => bar::handle_command(&cmd, ironbar),
            Command::Style(cmd) => style::handle_command(cmd, ironbar),
            #[cfg(feature = "matrix_launcher")]
            Command::Launcher(cmd) => {
                use crate::ipc::LauncherCommand;

                let Some(launcher) = ironbar.matrix_launcher() else {
                    return Response::error("launcher is not initialized");
                };
                let result = match cmd {
                    LauncherCommand::Show => launcher.show().map(|()| Response::Ok),
                    LauncherCommand::Hide => {
                        launcher.hide();
                        Ok(Response::Ok)
                    }
                    LauncherCommand::Toggle => launcher.toggle().map(|()| Response::Ok),
                    LauncherCommand::Refresh => launcher.refresh().map(|()| Response::Ok),
                    LauncherCommand::Status => Ok(Response::OkValue {
                        value: launcher.status(),
                    }),
                };
                result.unwrap_or_else(|error| Response::error(&error))
            }
        }
    }

    /// Shuts down the IPC server,
    /// removing the socket file in the process.
    ///
    /// Note this is static as the `Ipc` struct is not `Send`.
    pub fn shutdown<P: AsRef<Path>>(path: P) {
        if let Err(err) = Self::remove_socket(path.as_ref())
            && err.kind() != io::ErrorKind::NotFound
        {
            warn!("Unable to remove IPC socket: {err}");
        }
    }

    fn bind_listener(path: &Path) -> io::Result<(UnixListener, u32)> {
        let listener = UnixListener::bind(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        let owner_uid = fs::symlink_metadata(path)?.uid();

        Ok((listener, owner_uid))
    }

    fn remove_socket(path: &Path) -> io::Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_socket() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "IPC path is not a Unix socket",
            ));
        }

        fs::remove_file(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn test_path(name: &str) -> std::path::PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        std::env::temp_dir().join(format!(
            "cbar-ipc-{name}-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[tokio::test]
    async fn listener_is_private_and_peer_uid_matches_owner() {
        let path = test_path("private");
        let (listener, owner_uid) = Ipc::bind_listener(&path).expect("listener to bind");

        let mode = fs::symlink_metadata(&path)
            .expect("socket metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);

        let client = UnixStream::connect(&path).await.expect("client to connect");
        let (server, _) = listener.accept().await.expect("server to accept");
        assert_eq!(
            server.peer_cred().expect("peer credentials").uid(),
            owner_uid
        );

        drop(client);
        drop(server);
        drop(listener);
        Ipc::remove_socket(&path).expect("socket cleanup");
    }

    #[test]
    fn cleanup_refuses_non_socket_paths() {
        let path = test_path("regular-file");
        fs::write(&path, b"not a socket").expect("test file to be written");

        let err = Ipc::remove_socket(&path).expect_err("regular file must be retained");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(path.exists());

        fs::remove_file(path).expect("test file cleanup");
    }
}
