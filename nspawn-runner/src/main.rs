use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use clap::Parser;
use log::{debug, info, warn};
use serde::Deserialize;
use simplelog::{ColorChoice, Config as SimpleLogConfig, LevelFilter, TermLogger, TerminalMode};
use tokio::process::Command;
use tokio::sync::Mutex;
use uuid::Uuid;

use treadmill_rs::api::coord_runner::rest as rest_api;
use treadmill_rs::api::coord_runner::sse as sse_api;
use treadmill_rs::connector;
use treadmill_rs::control_socket;
use treadmill_rs::dummy_connector::DummyRunnerConnector;
use treadmill_sse_connector::SSERunnerConnector;
use treadmill_unix_seqpacket_control_socket::UnixSeqpacketControlSocket;

#[derive(Parser, Debug, Clone)]
struct NspawnRunnerArgs {
    /// Path to the TOML configuration file
    #[arg(short, long)]
    config_file: PathBuf,

    /// Whether to test-start a given environment
    #[arg(long)]
    test_env: Option<Uuid>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct NspawnRunnerEnvironmentZfsRootConfig {
    // Should contain full filesystem snapshot spec
    clone_from: Option<String>,
    parent: String,
    mount_base: String,
    quota: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct NspawnRunnerEnvironmentMountConfig {
    src: PathBuf,
    dst: PathBuf,
    readonly: bool,
}

fn default_device_config_resolve_symlink() -> bool {
    true
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeviceConfigAddMount {
    No,
    ReadWrite,
    ReadOnly,
}

impl Default for DeviceConfigAddMount {
    fn default() -> Self {
        DeviceConfigAddMount::No
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct NspawnRunnerEnvironmentDeviceConfig {
    device_node: PathBuf,
    #[serde(default = "default_device_config_resolve_symlink")]
    resolve_symlink: bool,
    #[serde(default)]
    add_mount: DeviceConfigAddMount,
    read: bool,
    write: bool,
    create: bool,
}

#[derive(Deserialize, Debug, Clone)]
pub struct NspawnRunnerEnvironmentVethConfig {
    ifname_host: String,
    ifname_container: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct NspawnRunnerEnvironmentIpv4NetworkConfig {
    address: std::net::Ipv4Addr,
    prefix_length: u8,
    #[serde(default)]
    gateway: Option<std::net::Ipv4Addr>,
    #[serde(default)]
    nameservers: Vec<std::net::Ipv4Addr>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct NspawnRunnerEnvironmentIpv6NetworkConfig {
    address: std::net::Ipv6Addr,
    prefix_length: u8,
    #[serde(default)]
    gateway: Option<std::net::Ipv6Addr>,
    #[serde(default)]
    nameservers: Vec<std::net::Ipv6Addr>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub enum SSHPreferredIPVersion {
    Unspecified,
    V4,
    V6,
}

impl Default for SSHPreferredIPVersion {
    fn default() -> Self {
        SSHPreferredIPVersion::Unspecified
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct NspawnRunnerEnvironmentConfig {
    #[serde(default)]
    init: Option<String>,
    shutdown_timeout: u64,
    #[serde(default)]
    mount: Vec<NspawnRunnerEnvironmentMountConfig>,
    #[serde(default)]
    device: Vec<NspawnRunnerEnvironmentDeviceConfig>,
    #[serde(default)]
    zfsroot: Option<NspawnRunnerEnvironmentZfsRootConfig>,
    control_socket_path: PathBuf,
    #[serde(default)]
    veth: Vec<NspawnRunnerEnvironmentVethConfig>,
    #[serde(default)]
    ssh_port: Option<u16>,
    #[serde(default)]
    ssh_preferred_ip_version: SSHPreferredIPVersion,
    #[serde(default)]
    ipv4_network: Option<NspawnRunnerEnvironmentIpv4NetworkConfig>,
    #[serde(default)]
    ipv6_network: Option<NspawnRunnerEnvironmentIpv6NetworkConfig>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct NspawnRunnerConfig {
    coordinator_base_url: String,
    board_id: Uuid,
    keepalive_timeout: u64,
    reconnect_wait: u64,
    environments: HashMap<Uuid, NspawnRunnerEnvironmentConfig>,
}

pub struct NspawnRunnerJob {
    job_id: Uuid,
    _environment_id: Uuid,
    environment_config: NspawnRunnerEnvironmentConfig,
    ssh_keys: Vec<String>,
    nspawn_proc: Arc<Mutex<tokio::process::Child>>,
    console_streamer_handle: tokio::task::JoinHandle<()>,
    console_streamer_cmd_chan: tokio::sync::mpsc::Sender<ConsoleStreamerCommand>,
    control_socket: UnixSeqpacketControlSocket<NspawnRunner>,
    ssh_rendezvous_proxies: Vec<rendezvous_proxy::RendezvousProxy>,

    // Pointers to created resources, to delete when shutting down (if not
    // indicated otherwise):
    root_fs_mountpoint: Option<PathBuf>,
    zfs_root_fs: Option<String>,
}

pub struct NspawnRunner {
    connector: Arc<dyn connector::RunnerConnector>,
    current_job: Mutex<Option<NspawnRunnerJob>>,
    config: NspawnRunnerConfig,
}

impl NspawnRunner {
    pub fn new(connector: Arc<dyn connector::RunnerConnector>, config: NspawnRunnerConfig) -> Self {
        NspawnRunner {
            connector,
            config,
            current_job: Mutex::new(None),
        }
    }

    async fn allocate_zfs_root(
        &self,
        job_id: Uuid,
        _environment_id: Uuid,
        zfs_root_cfg: &NspawnRunnerEnvironmentZfsRootConfig,
    ) -> Result<(PathBuf, String), String> {
        let mut zfs_create_cmd = vec![
            (if zfs_root_cfg.clone_from.is_some() {
                "clone"
            } else {
                "create"
            })
            .to_string(),
            "-o".to_string(),
            "mountpoint=legacy".to_string(),
        ];

        if let Some(quota) = &zfs_root_cfg.quota {
            zfs_create_cmd.push("-o".to_string());
            zfs_create_cmd.push(format!("quota={}", quota));
        }

        if let Some(ref source_fs) = zfs_root_cfg.clone_from {
            zfs_create_cmd.push(source_fs.clone());
        }

        let zfs_fs = format!("{}/{}", zfs_root_cfg.parent, job_id.to_string());
        zfs_create_cmd.push(zfs_fs.clone());

        // Create the file system:
        match Command::new("zfs").args(&zfs_create_cmd).output().await {
            Ok(std::process::Output {
                status,
                stdout,
                stderr,
            }) => {
                if !status.success() {
                    return Err(format!(
                        "Creating root filesystem through \"zfs {:?}\" failed \
                         with exit-status {:?}. Stdout: {}, Stderr: {}",
                        &zfs_create_cmd,
                        status.code(),
                        String::from_utf8_lossy(&stdout),
                        String::from_utf8_lossy(&stderr)
                    ));
                }
            }
            Err(e) => {
                return Err(format!(
                    "Creating root filesystem through \"zfs {:?}\" failed: {:?}",
                    &zfs_create_cmd, e
                ));
            }
        }

        // Mount the file system at the indicated path. First, create this path:
        let mountpoint = format!("{}/{}", zfs_root_cfg.mount_base, job_id);
        if let Err(e) = tokio::fs::create_dir_all(&mountpoint).await {
            return Err(format!(
                "Failed to create root mountpoint {:?}: {:?}",
                &mountpoint, e,
            ));
        }

        // Now, attempt to mount it:
        match Command::new("mount")
            .args(&["-t", "zfs", &zfs_fs, &mountpoint])
            .output()
            .await
        {
            Ok(std::process::Output {
                status,
                stdout,
                stderr,
            }) => {
                if !status.success() {
                    return Err(format!(
                        "Mounting ZFS root filesystem failed with exit-status \
                         {:?}. Stdout: {}, Stderr: {}",
                        status.code(),
                        String::from_utf8_lossy(&stdout),
                        String::from_utf8_lossy(&stderr)
                    ));
                }
            }
            Err(e) => {
                return Err(format!("Mounting ZFS root filesystem failed: {:?}", e,));
            }
        }

        Ok((PathBuf::from(mountpoint), zfs_fs))
    }

    async fn destroy_zfs_root(&self, zfs_root_fs: &str) -> Result<(), String> {
        match Command::new("zfs")
            .args(&["destroy", "-v", zfs_root_fs])
            .output()
            .await
        {
            Ok(std::process::Output {
                status,
                stdout,
                stderr,
            }) => {
                if !status.success() {
                    Err(format!(
                        "Destroying ZFS root filesystem failed with exit-status \
                                     {:?}. Stdout: {}, Stderr: {}",
                        status.code(),
                        String::from_utf8_lossy(&stdout),
                        String::from_utf8_lossy(&stderr)
                    ))
                } else {
                    Ok(())
                }
            }
            Err(e) => Err(format!(
                "Destroying ZFS filesystem failed with error: {:?}",
                e
            )),
        }
    }
}

#[derive(Clone, Debug)]
enum ConsoleStreamerCommand {
    Shutdown,
}

#[async_trait]
impl connector::Runner for NspawnRunner {
    async fn start_job(this: &Arc<Self>, msg: sse_api::StartJobMessage) {
        // This method must not block for long periods of time. We're provided
        // an &Arc<Self> to be able to launch async tasks, while returning
        // immediately. For now, we assume that all actions performed here are
        // reasonably fast, and we thus only return once the container is
        // started.

        // First, grab the `current_job` mutex. If there is already another job
        // running, we abort:
        let mut current_job_lg = this.current_job.lock().await;
        if let Some(NspawnRunnerJob {
            job_id: ref running_job_id,
            ..
        }) = *current_job_lg
        {
            this.connector
                .post_job_state(
                    msg.job_id,
                    rest_api::JobState::Failed {
                        status_message: Some(format!(
                            "Cannot start job {:?} on board {:?}, still executing job {:?}",
                            msg.job_id, this.config.board_id, running_job_id
                        )),
                    },
                )
                .await;
            return;
        }

        // Try to get a hold of the requested job environment. Error out if the
        // environment can't be found:
        let environment_cfg =
            if let Some(env_cfg) = this.config.environments.get(&msg.environment_id) {
                env_cfg
            } else {
                this.connector
                    .post_job_state(
                        msg.job_id,
                        rest_api::JobState::Failed {
                            status_message: Some(format!(
                                "Cannot start job {:?} on board {:?}, unknown environment {:?}",
                                msg.job_id, this.config.board_id, msg.environment_id
                            )),
                        },
                    )
                    .await;
                return;
            };

        // We're not executing any job and acquired the lock, begin allocating
        // the root file system (volume):
        this.connector
            .post_job_state(
                msg.job_id,
                rest_api::JobState::Starting {
                    stage: rest_api::JobStartingStage::Allocating,
                    status_message: None,
                },
            )
            .await;

        // Dispatch to the methods for allocating the root file system:
        let root_fs_res: Result<(PathBuf, Option<String>), String> =
            if let Some(zfs_root_cfg) = &environment_cfg.zfsroot {
                this.allocate_zfs_root(msg.job_id, msg.environment_id, zfs_root_cfg)
                    .await
                    .map(|(mountpoint, zfs_root_fs)| (mountpoint, Some(zfs_root_fs)))
            } else {
                Err(format!(
                    "Cannot start job {:?} on board {:?}, no root filesystem provider found.",
                    msg.job_id, this.config.board_id,
                ))
            };

        let (root_fs_mountpoint, zfs_root_fs) = match root_fs_res {
            Ok(t) => t,
            Err(emsg) => {
                this.connector
                    .post_job_state(
                        msg.job_id,
                        rest_api::JobState::Failed {
                            status_message: Some(emsg),
                        },
                    )
                    .await;
                return;
            }
        };

        // Create the control socket in the container's root path.

        // Get the absolute path to the socket (convert `control_socket_path`
        // into relative, then join with the `root_fs_mountpoint`):
        let control_socket_path_rel = environment_cfg
            .control_socket_path
            .strip_prefix("/")
            .unwrap_or(&environment_cfg.control_socket_path);
        let control_socket_path_abs = root_fs_mountpoint.join(control_socket_path_rel);
        // Make sure that the final path is within the container:
        assert!(control_socket_path_abs.starts_with(&root_fs_mountpoint));

        // Start the control socket handler and create a new UNIX SeqPacket socket:
        let control_socket = UnixSeqpacketControlSocket::new_unix_seqpacket(
            msg.job_id,
            &control_socket_path_abs,
            this.clone(),
        )
        .await
        .with_context(|| {
            format!(
                "Creating control socket under \"{:?}\"",
                control_socket_path_abs
            )
        })
        .unwrap();

        // Spawn rendezvous proxy clients for SSH connections to the
        // container IP, if one is configured that we can reach.
        use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6};

        let ssh_socket_addr = match (
            &environment_cfg.ssh_port,
            &environment_cfg.ssh_preferred_ip_version,
            &environment_cfg.ipv4_network,
            &environment_cfg.ipv6_network,
        ) {
            (None, _, _, _) => None,
            (Some(port), SSHPreferredIPVersion::V4, Some(ipv4_network), _) => Some(SocketAddr::V4(
                SocketAddrV4::new(ipv4_network.address, *port),
            )),
            (Some(port), SSHPreferredIPVersion::V6, _, Some(ipv6_network)) => Some(SocketAddr::V6(
                SocketAddrV6::new(ipv6_network.address, *port, 0, 0),
            )),
            (Some(port), _, Some(ipv4_network), _) => Some(SocketAddr::V4(SocketAddrV4::new(
                ipv4_network.address,
                *port,
            ))),
            (Some(port), _, _, Some(ipv6_network)) => Some(SocketAddr::V6(SocketAddrV6::new(
                ipv6_network.address,
                *port,
                0,
                0,
            ))),
            _ => None,
        };

        let mut ssh_rendezvous_proxies = Vec::with_capacity(msg.ssh_rendezvous_servers.len());
        if let Some(sa) = ssh_socket_addr {
            for server_spec in &msg.ssh_rendezvous_servers {
                ssh_rendezvous_proxies.push(
                    rendezvous_proxy::RendezvousProxy::start(
                        server_spec.client_id.clone(),
                        server_spec.server_base_url.clone(),
                        sa,
                        server_spec.auth_token.clone(),
                        Duration::from_secs(60),
                        Duration::from_secs(10),
                    )
                    .await,
                );
            }
        }

        // All resources have been allocated, mark the container as booting:
        this.connector
            .post_job_state(
                msg.job_id,
                rest_api::JobState::Starting {
                    stage: rest_api::JobStartingStage::Booting,
                    status_message: None,
                },
            )
            .await;

        let mut run_args = vec![
            "--scope".to_string(),
            "--property=DevicePolicy=closed".to_string(),
        ];

        let mut device_mounts = vec![];
        for device_cfg in environment_cfg.device.iter() {
            if !device_cfg.read && !device_cfg.write && !device_cfg.create {
                // Don't add devices with no permissions:
                continue;
            }

            let mut device_node = device_cfg.device_node.clone();

            if device_cfg.resolve_symlink {
                match tokio::fs::canonicalize(&device_cfg.device_node).await {
                    Ok(canon) => {
                        device_node = canon;
                    }
                    Err(e) => {
                        warn!(
                            "Failed to get canonical path to device node {:?}: {:?}",
                            device_cfg.device_node, e
                        );
                    }
                }
            }

            match &device_cfg.add_mount {
                DeviceConfigAddMount::No => (),
                DeviceConfigAddMount::ReadOnly => {
                    device_mounts.push(NspawnRunnerEnvironmentMountConfig {
                        src: device_node.clone(),
                        dst: device_node.clone(),
                        readonly: true,
                    })
                }
                DeviceConfigAddMount::ReadWrite => {
                    device_mounts.push(NspawnRunnerEnvironmentMountConfig {
                        src: device_node.clone(),
                        dst: device_node.clone(),
                        readonly: false,
                    })
                }
            }

            run_args.push(format!(
                "--property=DeviceAllow={} {}{}{}",
                // TODO: this should retain non-UTF8 characters
                device_node.display(),
                if device_cfg.read { "r" } else { "" },
                if device_cfg.write { "w" } else { "" },
                if device_cfg.create { "m" } else { "" },
            ));
        }

        run_args.extend([
            "--".to_string(),
            "systemd-nspawn".to_string(),
            "-D".to_string(),
            // TODO: what to do about non-Unicode paths?
            format!("{}", root_fs_mountpoint.display()),
            "--keep-unit".to_string(),
            "--private-users=pick".to_string(),
            "--private-network".to_string(),
        ]);

        // Add veth network interfaces:
        for veth_cfg in environment_cfg.veth.iter() {
            run_args.push(format!(
                "--network-veth-extra={}:{}",
                veth_cfg.ifname_host, veth_cfg.ifname_container
            ));
        }

        // Add all additional mountpoints:
        for mount_cfg in environment_cfg.mount.iter().chain(device_mounts.iter()) {
            if mount_cfg.readonly {
                // TODO: this should be able to contain non-UTF8 characters:
                run_args.push(format!(
                    "--bind-ro={}:{}",
                    mount_cfg.src.display(),
                    mount_cfg.dst.display()
                ));
            } else {
                // TODO: this should be able to contain non-UTF8 characters:
                run_args.push(format!(
                    "--bind={}:{}",
                    mount_cfg.src.display(),
                    mount_cfg.dst.display()
                ));
            }
        }

        // Add the init command, or the `--boot` flag to automatically search
        // for a suitable PID1 init:
        if let Some(ref init_cmd) = environment_cfg.init {
            // In this case, we also set the `--kill-signal` to SIGRTMIN+3, as
            // would be set if `--boot` were used. This allows us to gracefully
            // shut down the container with a SIGTERM. This may be a systemd
            // convention and could be made configurable.
            run_args.push("--kill-signal=SIGRTMIN+3".to_string());
            run_args.push(init_cmd.to_string());
        } else {
            run_args.push("--boot".to_string());
        }

        info!("Executing \"systemd-run\" with arguments {:?}", run_args);

        let mut child = match tokio::process::Command::new("systemd-run")
            .args(run_args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                // TODO: cleanup root file system!
                this.connector
                    .post_job_state(
                        msg.job_id,
                        rest_api::JobState::Failed {
                            status_message: Some(format!("Failed to spawn container: {:?}", e,)),
                        },
                    )
                    .await;
                return;
            }
        };

        // Acquire the stdout and stderr pipes, and spawn a new log-streamer
        // task that collects all log output and streams it to the coordinator:
        let stdout = child
            .stdout
            .take()
            .expect("Failed to acquire stdout from child process");
        let stderr = child
            .stderr
            .take()
            .expect("Failed to acquire stderr from child process");

        let child = Arc::new(Mutex::new(child));

        let this_streamer = this.clone();
        let msg_streamer = msg.clone();
        let (streamer_chan_tx, mut streamer_chan_rx) = tokio::sync::mpsc::channel(1);
        let streamer_child = child.clone();
        let console_streamer = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;

            let this = this_streamer;
            let msg = msg_streamer;

            // Create BufReaders from the file descriptors for streaming:
            let mut stdout_reader = tokio::io::BufReader::with_capacity(64 * 1024, stdout);
            let mut stderr_reader = tokio::io::BufReader::with_capacity(64 * 1024, stderr);

            // We also allocate buffers (VecDeques) which are used to buffer
            // output it is acknowledged by the coordinator:
            let mut console_queue = std::collections::VecDeque::new();
            let mut console_queue_offset = 0;
            let _console_queue_sent = 0;

            let mut stdout_buf = [0; 64 * 1024];
            let mut stdout_closed = false;
            let mut stderr_buf = [0; 64 * 1024];
            let mut stderr_closed = false;

            enum ReadConsoleRes {
                ZeroBytes,
                Data,
                Shutdown,
                Error(std::io::Error),
            }

            loop {
                // TODO: force buf flush on timeout?
                #[rustfmt::skip]
                let res = tokio::select! {
                    streamer_cmd_opt = streamer_chan_rx.recv() => {
                        match streamer_cmd_opt {
                            Some(ConsoleStreamerCommand::Shutdown) => ReadConsoleRes::Shutdown,
                            None => {
                                panic!("Streamer command channel TX dropped!");
                            }
                        }
                    }

                    read_res = stdout_reader.read(&mut stdout_buf), if !stdout_closed => {
                        match read_res {
                            Ok(0) => {
                                // Mark as closed, so we don't loop reading zero bytes:
                                stdout_closed = true;
                                ReadConsoleRes::ZeroBytes
                            },
                            Ok(read_len) => {
                                console_queue.push_back((
                                    rest_api::StdioFd::Stdout,
                                    stdout_buf[..read_len].to_vec()
                                ));
                                ReadConsoleRes::Data
                            }
                            Err(e) => ReadConsoleRes::Error(e),
                        }
                    }

                    read_res = stderr_reader.read(&mut stderr_buf), if !stderr_closed => {
                        match read_res {
                            Ok(0) => {
                                // Mark as closed, so we don't loop reading zero bytes:
                                stderr_closed = true;
                                ReadConsoleRes::ZeroBytes
                            },
                            Ok(read_len) => {
                                console_queue.push_back((
                                    rest_api::StdioFd::Stderr,
                                    stderr_buf[..read_len].to_vec()
                                ));
                                ReadConsoleRes::Data
                            },
                            Err(e) => ReadConsoleRes::Error(e),
                        }
                    }
                };

                match res {
                    ReadConsoleRes::Data => {
                        // TODO: this simply assumes that a single buffer
                        // element has been appended to the VecDeque:
                        let (stdio_fd, buf) = console_queue.back().unwrap();

                        this.connector
                            .send_job_console_log(
                                msg.job_id,
                                console_queue_offset,
                                console_queue_offset + 1,
                                &[(*stdio_fd, buf.len())],
                                buf.clone(),
                            )
                            .await;
                        console_queue_offset += 1;
                    }

                    ReadConsoleRes::Shutdown => {
                        // Asked to shut down. Once we implement chunking, do
                        // one last flush to the coordinator.
                        debug!("Shutting down console log streamer.");
                        break;
                    }

                    ReadConsoleRes::ZeroBytes => {
                        info!("Handling ZeroBytes case, acquiring lock...");
                        // Reading zero bytes is an indication that the process
                        // might've exited. Check whether this is the case. If
                        // the process has died, perform one last read and
                        // invoke the shutdown code for this job. Prior to that
                        // we must release the child lock! This code path may
                        // also be invoked while the process is already shutting
                        // down (e.g. because of an invocation of
                        // `stop_job`). In this case, simply attempt another
                        // loop iteration and don't attempt to stop the job
                        // again (as this would result in a deadlock).
                        let mut child = streamer_child.try_lock();

                        let exited = match child.as_mut().map(|c| c.try_wait()) {
                            // We have the lock, and the child has exited:
                            Ok(Ok(Some(_))) => true,
                            // We have the lock, but the child has not exited:
                            Ok(Ok(None)) => false,
                            // We have the lock, but couldn't determine the exit
                            // status:
                            Ok(Err(e)) => {
                                panic!("Error while determining whether child exited: {:?}", e);
                            }
                            // We couldn't acquire the lock (presumably because
                            // a stop was already requested, and thus the
                            // process has shut down):
                            Err(_) => false,
                        };

                        // Unlock the mutex:
                        std::mem::drop(child);

                        if exited {
                            // TODO: Once we implement chunking, do one last
                            // read on both FDs and flush to the coordinator.

                            // This must be executed in an asynchronous task,
                            // inpendent of this current one, or otherwise we'd
                            // deadlock. This is because stop_job will await
                            // this task's join.
                            let stop_this = this.clone();
                            tokio::spawn(async move {
                                NspawnRunner::stop_job(
                                    &stop_this,
                                    sse_api::StopJobMessage { job_id: msg.job_id },
                                )
                                .await;
                            });

                            // Don't break out of the loop -- we still expect
                            // the official Shutdown signal, as usual.
                        }
                    }

                    ReadConsoleRes::Error(e) => {
                        panic!("Error reading process output: {:?}", e);
                    }
                }
            }
        });

        // TODO: it'd be nice if this didn't have to be
        // sequential. But using tokio's JoinSet we get lifetime
        // issues here, as .spawn() requires a 'static borrow of the
        // rendezvous proxies.
        let mut rendezvous_proxy_addrs = vec![];
        for proxy in ssh_rendezvous_proxies.iter() {
            match proxy.public_addr(Duration::from_secs(5)).await {
                Some((hostname, port)) => {
                    rendezvous_proxy_addrs.push(
                        rest_api::JobSessionConnectionInfo::RendezvousSSH {
                            hostname,
                            port,
                            host_key_fingerprints: vec![],
                        },
                    );
                }
                None => {
                    warn!("Rendezvous proxy did not provide public address before timeout.");
                }
            }
        }

        this.connector
            .post_job_state(
                msg.job_id,
                rest_api::JobState::Ready {
                    connection_info: rendezvous_proxy_addrs,
                    status_message: None,
                },
            )
            .await;

        *current_job_lg = Some(NspawnRunnerJob {
            job_id: msg.job_id,
            _environment_id: msg.environment_id,
            environment_config: environment_cfg.clone(),
            ssh_keys: msg.ssh_keys,
            nspawn_proc: child,
            console_streamer_handle: console_streamer,
            console_streamer_cmd_chan: streamer_chan_tx,
            root_fs_mountpoint: Some(root_fs_mountpoint),
            ssh_rendezvous_proxies,
            control_socket,
            zfs_root_fs,
        });
    }

    async fn stop_job(this: &Arc<Self>, msg: sse_api::StopJobMessage) {
        // Temporary workaround to avoid deleting the container root file
        // system. In the future, this should be an option passed from by the
        // coordinator.
        const MSG_DELETE_DATA: bool = false;

        // This method must not block for long periods of time. We're provided
        // an &Arc<Self> to be able to launch async tasks, while returning
        // immediately. For now, we assume that all actions performed here are
        // reasonably fast, and we thus only return once the container is
        // started.

        // First, grab the `current_job` mutex. If there is already another job
        // running, we abort. We remove take the job object from the option, but
        // to prevent another task to race with this method, hold the lock guard
        // til the very end:
        let mut current_job_lg = this.current_job.lock().await;
        match *current_job_lg {
            Some(ref job) => {
                if job.job_id != msg.job_id {
                    this.connector
                        .post_job_state(
                            msg.job_id,
                            rest_api::JobState::Failed {
                                status_message: Some(format!(
                                    "Cannot stop job {:?} on board {:?}, not running!",
                                    msg.job_id, this.config.board_id,
                                )),
                            },
                        )
                        .await;
                    return;
                }
            }

            None => {
                this.connector
                    .post_job_state(
                        msg.job_id,
                        rest_api::JobState::Failed {
                            status_message: Some(format!(
                                "Cannot stop job {:?} on board {:?}, not running!",
                                msg.job_id, this.config.board_id,
                            )),
                        },
                    )
                    .await;
                return;
            }
        };

        // Take the job object, such that we own it:
        let job = current_job_lg.take().unwrap();

        // The requested job is currently running, procede to stop it.
        // Transition into the shutdown state:
        this.connector
            .post_job_state(
                msg.job_id,
                rest_api::JobState::Stopping {
                    status_message: None,
                },
            )
            .await;

        // First, instruct the container to shut down. We attempt a graceful
        // shutdown by sending a SIGTERM to the systemd-nspawn process, which
        // should send a SIGRTMIN+3 to the container's PID1, which will initiate
        // an orderly shutdown:
        let mut child = job.nspawn_proc.lock().await;
        if let Some(pid) = child.id() {
            debug!("Sending SIGTERM to nspawn process...");
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid.try_into().unwrap()),
                nix::sys::signal::Signal::SIGTERM,
            );
        }

        // Now, wait for the container to shut down, or until the shutdown
        // timeout expires:
        debug!(
            "Waiting on process exit or shutdown timeout ({} secs)",
            job.environment_config.shutdown_timeout
        );
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(job.environment_config.shutdown_timeout)) => {},
            _ = child.wait() => {}
        };

        // Attempt to get the process' exit status or, if that doesn't succeed,
        // kill it, in a loop:
        debug!("Process exited OR timeout fired. Check exit code or kill child in a loop.");
        let mut exit_status = None;
        while exit_status.is_none() {
            match child.try_wait() {
                Ok(Some(es)) => {
                    debug!("Child exited.");
                    exit_status = Some(es);
                }
                Ok(None) => child.kill().await.unwrap(),
                Err(e) => {
                    panic!("Error while killing nspawn process: {:?}", e);
                }
            }
        }
        debug!(
            "Child is dead and exited with status: {:?}, code: {:?}",
            exit_status,
            exit_status.map(|es| es.code())
        );

        // The process is dead. Destroy the control socket:
        job.control_socket.shutdown().await.unwrap();

        // Instruct the log streamer to shutdown and wait for the last console
        // logs to be posted to the coordinator.
        debug!("Requesting console streamer to shut down.");
        job.console_streamer_cmd_chan
            .send(ConsoleStreamerCommand::Shutdown)
            .await
            .expect("Console streamer task has quit before receiving shutdown signal!");
        job.console_streamer_handle.await.unwrap();
        debug!("Console streamer has shut down.");

        // Shut down all rendezvous proxy clients:
        for proxy in job.ssh_rendezvous_proxies {
            if let Err(e) = proxy.shutdown().await {
                warn!("Error while shutting down rendezvous proxy client: {:?}", e);
            }
        }

        // Unmount the container's root file system:
        if let Some(mountpoint) = job.root_fs_mountpoint {
            match Command::new("umount").args(&[mountpoint]).output().await {
                Ok(std::process::Output {
                    status,
                    stdout,
                    stderr,
                }) => {
                    if !status.success() {
                        this.connector
                            .post_job_state(
                                msg.job_id,
                                rest_api::JobState::Failed {
                                    status_message: Some(format!(
                                        "Unmounting root filesystem failed with exit-status \
                                         {:?}. Stdout: {}, Stderr: {}",
                                        status.code(),
                                        String::from_utf8_lossy(&stdout),
                                        String::from_utf8_lossy(&stderr)
                                    )),
                                },
                            )
                            .await;
                        return;
                    }
                }
                Err(e) => {
                    this.connector
                        .post_job_state(
                            msg.job_id,
                            rest_api::JobState::Failed {
                                status_message: Some(format!(
                                    "Unmounting root filesystem failed with error: {:?}",
                                    e
                                )),
                            },
                        )
                        .await;
                    return;
                }
            }
        }

        if MSG_DELETE_DATA {
            // If we've created a ZFS file system for this container, destroy it:
            if let Some(zfs_fs) = job.zfs_root_fs {
                if let Err(emsg) = this.destroy_zfs_root(&zfs_fs).await {
                    this.connector
                        .post_job_state(
                            msg.job_id,
                            rest_api::JobState::Failed {
                                status_message: Some(emsg),
                            },
                        )
                        .await;
                    return;
                }
            }
        }

        // Manually drop the lock guard here, to ensure that it stays in scope
        // til the end of this function:
        core::mem::drop(current_job_lg);

        // Mark job as finished:
        this.connector
            .post_job_state(
                msg.job_id,
                rest_api::JobState::Finished {
                    status_message: None,
                },
            )
            .await;
    }
}

#[async_trait]
impl control_socket::Runner for NspawnRunner {
    async fn ssh_keys(&self, tgt_job_id: Uuid) -> Option<Vec<String>> {
        match *self.current_job.lock().await {
            None => None,
            Some(NspawnRunnerJob {
                ref job_id,
                ref ssh_keys,
                ..
            }) if *job_id == tgt_job_id => Some(ssh_keys.clone()),
            Some(_) => None,
        }
    }

    async fn network_config(
        &self,
        tgt_job_id: Uuid,
    ) -> Option<treadmill_rs::api::runner_puppet::NetworkConfig> {
        match *self.current_job.lock().await {
            None => None,
            Some(NspawnRunnerJob {
                ref job_id,
                ref environment_config,
                ..
            }) if *job_id == tgt_job_id => {
                let hostname = format!("job-{}", format!("{}", job_id).split_at(10).0);
                Some(treadmill_rs::api::runner_puppet::NetworkConfig {
                    hostname,
                    interface: Some("host0".to_string()),
                    ipv4: environment_config.ipv4_network.as_ref().map(|ip4| {
                        treadmill_rs::api::runner_puppet::Ipv4NetworkConfig {
                            address: ip4.address,
                            prefix_length: ip4.prefix_length,
                            gateway: ip4.gateway,
                            nameservers: ip4.nameservers.clone(),
                        }
                    }),
                    ipv6: environment_config.ipv6_network.as_ref().map(|ip6| {
                        treadmill_rs::api::runner_puppet::Ipv6NetworkConfig {
                            address: ip6.address,
                            prefix_length: ip6.prefix_length,
                            gateway: ip6.gateway,
                            nameservers: ip6.nameservers.clone(),
                        }
                    }),
                })
            }
            Some(_) => None,
        }
    }
}

#[tokio::main]
async fn main() {
    use treadmill_rs::connector::RunnerConnector;

    TermLogger::init(
        LevelFilter::Debug,
        SimpleLogConfig::default(),
        TerminalMode::Mixed,
        ColorChoice::Auto,
    )
    .unwrap();
    info!("Starting treadmill nspawn runner.");

    let args = NspawnRunnerArgs::parse();

    let config_str = std::fs::read_to_string(args.config_file).unwrap();
    let config: NspawnRunnerConfig = toml::from_str(&config_str).unwrap();

    if let Some(test_env_uuid) = args.test_env {
        let mut connector_opt = None;
        let _nspawn_runner = Arc::new_cyclic(|weak_runner| {
            let connector = Arc::new(DummyRunnerConnector::new(
                config.board_id,
                test_env_uuid,
                weak_runner.clone(),
            ));
            connector_opt = Some(connector.clone());
            NspawnRunner::new(connector, config)
        });
        let connector = connector_opt.take().unwrap();
        connector.run().await;
    } else {
        let mut connector_opt = None;
        let _nspawn_runner = Arc::new_cyclic(|weak_runner| {
            let connector = Arc::new(SSERunnerConnector::new(
                config.coordinator_base_url.clone(),
                config.board_id,
                weak_runner.clone(),
                Duration::from_secs(config.keepalive_timeout),
                Duration::from_secs(config.reconnect_wait),
            ));
            connector_opt = Some(connector.clone());
            NspawnRunner::new(connector, config)
        });
        let connector = connector_opt.take().unwrap();
        connector.run().await;
    }
}
