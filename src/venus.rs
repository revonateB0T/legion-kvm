//! Venus VM sessions, shared filesystems and guest graphics verification.
use crate::{
    archive, cpu, doctor, memory, probe,
    process::{CoreDumpLimit, OwnedChild},
    qemu_policy,
    qemu_policy::qemu_path,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    fs::{self, File},
    os::unix::fs::{DirBuilderExt, FileTypeExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Default)]
struct Config {
    qemu: PathBuf,
    kernel: PathBuf,
    init: PathBuf,
    modules: Vec<PathBuf>,
    userspace: PathBuf,
    virtiofsd: PathBuf,
    output: PathBuf,
    render_node: Option<PathBuf>,
    workload: Option<PathBuf>,
    workload_config: Option<PathBuf>,
    presentation: Option<PathBuf>,
    presentation_config: Option<PathBuf>,
    window: bool,
    x11_window: bool,
    auto_window: bool,
    desktop_renderer: bool,
    shares: Option<PathBuf>,
    memory_mib: Option<u32>,
    interactive: bool,
    application: bool,
    desktop_audio: bool,
    qmp: Option<PathBuf>,
    qemu_data: Option<PathBuf>,
}

fn parse(args: &[OsString]) -> Result<Config> {
    let mut config = Config::default();
    let (pairs, remainder) = args.as_chunks::<2>();
    for pair in pairs {
        let path = PathBuf::from(&pair[1]);
        match pair[0].to_str() {
            Some("--gpu-backend") => {
                ensure!(pair[1] == "venus", "only the Venus backend is supported")
            }
            Some("--qemu") => config.qemu = path,
            Some("--qmp") => config.qmp = Some(path),
            Some("--qemu-data") => config.qemu_data = Some(path),
            Some("--kernel") => config.kernel = path,
            Some("--init") => config.init = path,
            Some("--module") => config.modules.push(path),
            Some("--userspace") => config.userspace = path,
            Some("--virtiofsd") => config.virtiofsd = path,
            Some("--output") => config.output = path,
            Some("--render-node") => config.render_node = Some(path),
            Some("--workload") => config.workload = Some(path),
            Some("--workload-config") => config.workload_config = Some(path),
            Some("--presentation") => config.presentation = Some(path),
            Some("--presentation-config") => config.presentation_config = Some(path),
            Some("--shares") => config.shares = Some(path),
            Some("--memory-mib") => {
                let mib: u32 = pair[1].to_str().context("invalid memory size")?.parse()?;
                ensure!(
                    (1024..=65536).contains(&mib),
                    "memory must be 1024..65536 MiB"
                );
                config.memory_mib = Some(mib);
            }
            Some("--audio") => match pair[1].to_str() {
                Some("desktop") => config.desktop_audio = true,
                Some("off") => config.desktop_audio = false,
                _ => bail!("--audio must be desktop or off"),
            },
            Some("--window") => match pair[1].to_str() {
                Some("true") => config.window = true,
                Some("false") => config.window = false,
                _ => bail!("--window must be true or false"),
            },
            Some("--interactive" | "--application") => {
                let value = match pair[1].to_str() {
                    Some("true") => true,
                    Some("false") => false,
                    _ => bail!("session switch must be true or false"),
                };
                if pair[0] == "--interactive" {
                    config.interactive = value;
                } else {
                    config.application = value;
                }
            }
            Some("--window-system") => match pair[1].to_str() {
                Some("auto") => config.auto_window = true,
                Some("wayland") => config.x11_window = false,
                Some("x11") => config.x11_window = true,
                _ => bail!("--window-system must be auto, wayland or x11"),
            },
            Some("--window-renderer") => match pair[1].to_str() {
                Some("desktop") => config.desktop_renderer = true,
                Some("render-node") => config.desktop_renderer = false,
                _ => bail!("--window-renderer must be desktop or render-node"),
            },
            _ => bail!("unknown Venus session option: {:?}", pair[0]),
        }
    }
    ensure!(
        remainder.is_empty(),
        "Venus session option requires a value"
    );
    for (name, path) in [
        ("qemu", &config.qemu),
        ("kernel", &config.kernel),
        ("init", &config.init),
        ("userspace", &config.userspace),
        ("virtiofsd", &config.virtiofsd),
        ("output", &config.output),
    ] {
        ensure!(!path.as_os_str().is_empty(), "--{name} is required");
    }
    ensure!(
        !config.modules.is_empty() && config.modules.len() <= 16,
        "supply 1..16 matching guest modules in dependency order"
    );
    ensure!(
        config.workload.is_some() == config.workload_config.is_some(),
        "--workload and --workload-config must be supplied together"
    );
    ensure!(
        config.presentation_config.is_none()
            || config.presentation.is_some()
            || config.workload.is_some(),
        "--presentation-config requires --presentation or --workload"
    );
    Ok(config)
}

fn read_marker(console: &str, marker: &str) -> Result<Option<Value>> {
    console
        .split_inclusive('\n')
        .filter(|line| line.ends_with('\n'))
        .find_map(|line| line.strip_prefix(marker))
        .map(serde_json::from_str)
        .transpose()
        .map_err(Into::into)
}

fn share(
    program: &Path,
    sockets: &Path,
    output: &Path,
    tag: &str,
    directory: &Path,
    readonly: bool,
    command: &mut Command,
) -> Result<OwnedChild> {
    let child = start_share(program, sockets, output, tag, directory, readonly)?;
    let socket = sockets.join(format!("{tag}.sock"));
    command.args([
        "-chardev",
        &format!("socket,id=fs-{tag},path={}", qemu_path(&socket)?),
        "-device",
        &format!("vhost-user-fs-pci,chardev=fs-{tag},tag={tag}"),
    ]);
    Ok(child)
}

fn start_share(
    program: &Path,
    sockets: &Path,
    output: &Path,
    tag: &str,
    directory: &Path,
    readonly: bool,
) -> Result<OwnedChild> {
    let socket = sockets.join(format!("{tag}.sock"));
    let log = File::create(output.join(format!("virtiofs-{tag}.log")))?;
    let mut service = Command::new(program);
    service
        .arg("--shared-dir")
        .arg(directory)
        .arg("--socket-path")
        .arg(&socket)
        .args(["--sandbox", "namespace"])
        .env_remove("LD_PRELOAD")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    if readonly {
        service.arg("--readonly");
    }
    let mut child = OwnedChild(service.spawn().context("start Venus virtiofsd")?);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket
        .metadata()
        .is_ok_and(|meta| meta.file_type().is_socket())
    {
        ensure!(
            child.0.try_wait()?.is_none(),
            "virtiofsd exited; see virtiofs-{tag}.log"
        );
        ensure!(Instant::now() < deadline, "virtiofsd socket timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(child)
}

/// A failed QEMU may already have connected to every filesystem service.
fn restart_shares(
    program: &Path,
    sockets: &Path,
    output: &Path,
    exports: &[(&str, &Path, bool)],
    services: &mut Vec<OwnedChild>,
) -> Result<()> {
    services.clear();
    for &(tag, directory, readonly) in exports {
        let log = output.join(format!("virtiofs-{tag}.log"));
        fs::copy(&log, output.join(format!("virtiofs-{tag}-wayland.log")))?;
        let socket = sockets.join(format!("{tag}.sock"));
        if socket.exists() {
            fs::remove_file(socket)?;
        }
        services.push(start_share(
            program, sockets, output, tag, directory, readonly,
        )?);
    }
    Ok(())
}

pub(crate) fn run(args: &[OsString]) -> Result<()> {
    let config = parse(args)?;
    let requested = config
        .render_node
        .as_ref()
        .map(fs::canonicalize)
        .transpose()?;
    let devices = doctor::render_devices()?;
    let node = crate::graphics::select(&devices, requested.as_deref())?.node;
    let topology = cpu::host_topology()?;
    let capabilities = legion_kvm_capabilities::inspect(Path::new("/dev/kvm"))?;
    ensure!(
        capabilities
            .cpu_probe_issues(topology.qemu_slots()?)
            .is_empty(),
        "KVM CPU prerequisites are missing"
    );
    let userspace = config.userspace.canonicalize()?;
    let shares = config
        .shares
        .as_ref()
        .map(|path| -> Result<_> {
            ensure!(
                fs::metadata(path)?.len() <= legion_shared_directories::MAX_MANIFEST_BYTES,
                "directory-share configuration exceeds 64 KiB"
            );
            let sources: legion_shared_directories::Sources =
                serde_json::from_slice(&fs::read(path)?)?;
            sources.resolve()
        })
        .transpose()?;
    ensure!(
        userspace.join("bin/vulkaninfo").is_file()
            && userspace
                .join("share/vulkan/icd.d/virtio_icd.json")
                .is_file(),
        "userspace must contain vulkaninfo and the selected hardware Vulkan ICD"
    );
    let kernel = config.kernel.canonicalize()?;
    let init = fs::read(&config.init)?;
    probe::validate_static_elf(&init)?;
    ensure!(init.len() <= 32 * 1024 * 1024, "guest init is too large");
    let modules = config
        .modules
        .iter()
        .map(|path| {
            ensure!(
                fs::metadata(path)?.len() <= 32 * 1024 * 1024,
                "guest module is too large"
            );
            fs::read(path).map_err(Into::into)
        })
        .collect::<Result<Vec<_>>>()?;
    let workload = config
        .workload
        .as_ref()
        .map(|path| {
            ensure!(
                fs::metadata(path)?.len() <= 32 * 1024 * 1024,
                "workload exceeds 32 MiB"
            );
            fs::read(path).map_err(Into::into)
        })
        .transpose()?;
    let workload_config = config
        .workload_config
        .as_ref()
        .map(|path| -> Result<_> {
            ensure!(
                fs::metadata(path)?.len() <= 64 * 1024,
                "workload configuration exceeds 64 KiB"
            );
            let bytes = fs::read(path)?;
            let settings: legion_workload_probe::Config = serde_json::from_slice(&bytes)?;
            settings.validate()?;
            Ok((bytes, settings.timeout_seconds()))
        })
        .transpose()?;
    let timeout_seconds = workload_config
        .as_ref()
        .map_or(60, |(_, timeout)| 60 + timeout);
    let presentation = config
        .presentation
        .as_ref()
        .map(|path| -> Result<_> {
            ensure!(
                fs::metadata(path)?.len() <= 32 * 1024 * 1024,
                "presentation program exceeds 32 MiB"
            );
            Ok(fs::read(path)?)
        })
        .transpose()?;
    fs::DirBuilder::new().mode(0o700).create(&config.output)?;
    let output = config.output.canonicalize()?;
    let mut progress = crate::startup::Progress::new(&output, config.interactive);
    let frames = output.join("frames");
    fs::create_dir(&frames)?;
    if let Some(shares) = &shares {
        fs::write(
            frames.join(legion_shared_directories::MANIFEST),
            serde_json::to_vec_pretty(&shares.manifest())?,
        )?;
    }
    let mut dimensions = (1920_u32, 1080_u32);
    let mut fullscreen = false;
    if let Some(path) = &config.presentation_config {
        ensure!(
            fs::metadata(path)?.len() <= 64 * 1024,
            "presentation configuration exceeds 64 KiB"
        );
        let bytes = fs::read(path)?;
        let settings: Value = serde_json::from_slice(&bytes)?;
        ensure!(
            settings["schema"] == 1,
            "unknown presentation configuration schema"
        );
        if let (Some(w), Some(h)) = (settings["width"].as_u64(), settings["height"].as_u64()) {
            ensure!(
                (64..=3840).contains(&w) && (64..=2160).contains(&h),
                "invalid display dimensions"
            );
            dimensions = (u32::try_from(w)?, u32::try_from(h)?);
        }
        fullscreen = settings["fullscreen"] == true;
        fs::write(frames.join("kms-presentation.json"), bytes)?;
    }
    let initrd = output.join("initramfs.cpio");
    archive::write_with_workload(
        File::create(&initrd)?,
        &init,
        &modules,
        None,
        presentation.as_deref(),
        workload.as_deref(),
        workload_config.as_ref().map(|(bytes, _)| bytes.as_slice()),
    )?;
    let _core_limit = CoreDumpLimit::disabled()?;
    let sockets = tempfile::Builder::new().prefix("legion-drm-").tempdir()?;
    let memory_mib = config.memory_mib.unwrap_or(1024);
    let ram = memory::Ram::new(memory::Mode::Aligned, u64::from(memory_mib))?;
    let model = cpu::boot_cpu()?;
    let mut command = qemu_policy::command(&config.qemu, qemu_policy::HugePages::Off)?;
    let (vendor_id, device_id) = crate::graphics::pci_id(&node)?;
    command.args(["-machine","q35,accel=kvm,smm=off,memory-backend=ram","-cpu",&model,"-m",&memory_mib.to_string()])
        .args(topology.qemu_args(&model)?)
        .args(["-object",&ram.argument,"-nodefaults","-no-reboot","-monitor","none","-serial","stdio","-nic","none"])
        .args(["-device","virtio-keyboard-pci","-device","virtio-mouse-pci"])
        .args(["-display",&if config.window { if fullscreen { "sdl,gl=es,full-screen=on".to_owned() } else { "sdl,gl=es".to_owned() } } else { format!("egl-headless,rendernode={}",qemu_path(&node)?) },
            "-device",&format!("virtio-gpu-gl-pci,hostmem=1G,blob=on,drm_native_context=off,venus=on,xres={},yres={}",dimensions.0,dimensions.1)])
        .arg("-kernel").arg(kernel).arg("-initrd").arg(initrd)
        .args(["-append", &format!("console=ttyS0 rdinit=/init quiet panic=-1 legion.drm_probe=1 legion.vulkan=1 legion.gpu=venus legion.gpu_vendor={vendor_id:04x} legion.gpu_device={device_id:04x}")])
        .env_remove("LD_PRELOAD").env_remove("GBM_BACKEND").env_remove("__EGL_VENDOR_LIBRARY_FILENAMES")
        .stdin(Stdio::null()).stdout(File::create(output.join("console.log"))?)
        .stderr(File::create(output.join("qemu.log"))?);
    crate::audio::configure(&mut command, config.desktop_audio);
    if let Some(data) = &config.qemu_data {
        ensure!(data.is_dir(), "QEMU data directory is missing");
        command.arg("-L").arg(data);
    }
    // Select Vulkan independently of the GPU driving the host desktop.
    let pci = fs::canonicalize(
        Path::new("/sys/class/drm")
            .join(node.file_name().context("render node name missing")?)
            .join("device"),
    )?;
    let pci = pci
        .file_name()
        .and_then(|v| v.to_str())
        .context("GPU PCI address missing")?;
    command.env(
        "LEGION_VENUS_DRI_PRIME",
        format!("pci-{}!", pci.replace([':', '.'], "_")),
    );
    command.args([
        "-name",
        &format!("legion-venus-probe-{}", std::process::id()),
    ]);
    if let Some(socket) = &config.qmp {
        ensure!(!socket.exists(), "QMP socket already exists");
        command.args([
            "-qmp",
            &format!("unix:{},server=on,wait=off", qemu_path(socket)?),
        ]);
    }
    if config.window && !config.desktop_renderer {
        let pci = fs::canonicalize(
            Path::new("/sys/class/drm")
                .join(node.file_name().context("render node name missing")?)
                .join("device"),
        )?;
        let pci = pci
            .file_name()
            .and_then(|name| name.to_str())
            .context("GPU PCI address missing")?;
        command
            .env("DRI_PRIME", format!("pci-{}", pci.replace([':', '.'], "_")))
            .env(
                "__EGL_VENDOR_LIBRARY_FILENAMES",
                "/usr/share/glvnd/egl_vendor.d/50_mesa.json",
            )
            .env(
                "SDL_VIDEODRIVER",
                if config.x11_window { "x11" } else { "wayland" },
            );
        if config.x11_window {
            command.env("__GLX_VENDOR_LIBRARY_NAME", "mesa");
        }
    }
    if config.window && config.desktop_renderer {
        command
            .env("LEGION_SCANOUT_EXTERNAL", "1")
            .env_remove("DRI_PRIME")
            .env_remove("__GLX_VENDOR_LIBRARY_NAME")
            .env(
                "SDL_VIDEODRIVER",
                if config.x11_window { "x11" } else { "wayland" },
            );
    }
    progress.set(crate::startup::Stage::Graphics);
    let mut services = vec![
        share(
            &config.virtiofsd,
            sockets.path(),
            &output,
            "usr",
            &userspace,
            true,
            &mut command,
        )?,
        share(
            &config.virtiofsd,
            sockets.path(),
            &output,
            "frames",
            &frames,
            false,
            &mut command,
        )?,
    ];
    if let Some(shares) = &shares {
        for source in &shares.directories {
            services.push(share(
                &config.virtiofsd,
                sockets.path(),
                &output,
                &source.tag,
                &source.directory,
                source.readonly,
                &mut command,
            )?);
        }
    }
    fs::write(
        output.join("command.json"),
        serde_json::to_vec_pretty(&json!({
            "program":command.get_program(),"arguments":command.get_args().collect::<Vec<_>>(),"render_node":node
        }))?,
    )?;
    progress.set(crate::startup::Stage::Booting);
    let mut child = OwnedChild(command.spawn().context("start graphics probe QEMU")?);
    let session_deadline = if config.interactive {
        crate::session::Deadline::InteractiveStartup(crate::session::startup_timeout()?)
    } else {
        crate::session::Deadline::Bounded(Duration::from_secs(timeout_seconds))
    };
    let started = Instant::now();
    let mut retried_window = false;
    let host_vulkan_environment: serde_json::Map<String, Value> = [
        "VK_DRIVER_FILES",
        "VK_ICD_FILENAMES",
        "VK_ADD_DRIVER_FILES",
        "VK_LOADER_DRIVERS_SELECT",
        "VK_LOADER_DRIVERS_DISABLE",
        "VK_LOADER_LAYERS_ENABLE",
        "VK_LOADER_LAYERS_DISABLE",
        "NODEVICE_SELECT",
    ]
    .into_iter()
    .filter_map(|name| {
        std::env::var_os(name).map(|value| (name.to_owned(), json!(value.to_string_lossy())))
    })
    .collect();
    let mut report = json!({"schema":1,"passed":false,"render_node":node,
        "host_vulkan_environment":host_vulkan_environment,
        "expected_gpu":{"vendor_id":vendor_id,"device_id":device_id},
        "hardware_rendering_verified":false,"presentation_included":presentation.is_some() || config.presentation_config.is_some(),
        "desktop_capture_verified":false,"guest_memory":ram.info,
        "desktop_window_requested":config.window,
        "scope":"venus_vulkan"});
    let result: Result<()> = (|| {
        loop {
            progress.poll();
            let path = output.join("console.log");
            ensure!(
                fs::metadata(&path)?.len() <= 1024 * 1024,
                "guest console exceeds probe bound"
            );
            let text = fs::read_to_string(&path)?;
            if let Some(driver) = read_marker(&text, "LEGION_DRM_PROBE ")? {
                progress.set(crate::startup::Stage::GuestReady);
                report["driver"] = driver;
                ensure!(
                    report["driver"]["passed"] == true,
                    "guest virtio-GPU driver probe failed"
                );
                let nodes = report["driver"]["render_nodes"]
                    .as_array()
                    .context("missing guest render nodes")?;
                ensure!(
                    nodes
                        .iter()
                        .any(|node| node["resource_blob"] == 1 && node["context_init"] == 1),
                    "guest lacks virtio-GPU blob/context capabilities"
                );
            }
            if let Some(vulkan) = read_marker(&text, "LEGION_VULKAN_PROBE ")? {
                report["vulkan"] = vulkan;
                let cpu = probe::read_report(&path)?.context("guest CPU report missing")?;
                cpu.verify(&topology)?;
                report["cpu"] = serde_json::to_value(cpu)?;
                ensure!(
                    report["driver"]["passed"] == true,
                    "guest driver proof is missing"
                );
                ensure!(
                    report["vulkan"]["passed"] == true
                        && report["vulkan"]["enumeration_verified"] == true,
                    "guest hardware Vulkan check failed: {}; inspect report.json and frames/vulkaninfo.stderr",
                    report["vulkan"]["error"]
                        .as_str()
                        .unwrap_or("enumeration did not pass")
                );
                if workload.is_some() && !config.application {
                    ensure!(
                        report["vulkan"]["workload_gpu_verified"] == true,
                        "Venus workload did not verify its rendered pixels"
                    );
                    report["hardware_rendering_verified"] = true.into();
                }
                report["passed"] = true.into();
                return Ok(());
            }
            if let Some(status) = child.0.try_wait()? {
                let error = fs::read_to_string(output.join("qemu.log"))?;
                if !retried_window
                    && retry_window(
                        &config,
                        std::env::var_os("DISPLAY").is_some(),
                        &error,
                        text.is_empty(),
                    )
                {
                    retried_window = true;
                    eprintln!(
                        "legion: Wayland window initialization failed; retrying with XWayland"
                    );
                    fs::copy(output.join("qemu.log"), output.join("qemu-wayland.log"))?;
                    let mut exports = vec![
                        ("usr", userspace.as_path(), true),
                        ("frames", frames.as_path(), false),
                    ];
                    if let Some(shares) = &shares {
                        exports.extend(
                            shares
                                .directories
                                .iter()
                                .map(|s| (s.tag.as_str(), s.directory.as_path(), s.readonly)),
                        );
                    }
                    restart_shares(
                        &config.virtiofsd,
                        sockets.path(),
                        &output,
                        &exports,
                        &mut services,
                    )?;
                    command
                        .env("SDL_VIDEODRIVER", "x11")
                        .stdout(File::create(output.join("console.log"))?)
                        .stderr(File::create(output.join("qemu.log"))?);
                    child = OwnedChild(
                        command
                            .spawn()
                            .context("retry Legion window using XWayland")?,
                    );
                    report["window_fallback"] = "x11".into();
                    continue;
                }
                ensure!(
                    config.interactive && status.success(),
                    "QEMU exited ({status}): {}; inspect {}",
                    error.trim(),
                    output.join("qemu.log").display()
                );
                report["closed_by_user"] = true.into();
                report["passed"] = true.into();
                return Ok(());
            }
            ensure!(
                !session_deadline.expired(started.elapsed(), progress.rendering_started()),
                "{}",
                crate::session::timeout_detail(&frames)
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    })();
    child.stop()?;
    for service in &mut services {
        service.stop_after_grace(Duration::from_millis(100))?;
    }
    drop(services);
    drop(ram);
    if let Err(error) = &result {
        report["error"] = format!("{error:#}").into();
        if let Ok(bytes) = fs::read(frames.join("vulkan-enumeration.json"))
            && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
        {
            report["vulkan_enumeration"] = value;
        }
    }
    progress.set(if result.is_ok() {
        crate::startup::Stage::Complete
    } else {
        crate::startup::Stage::Failed
    });
    fs::write(
        output.join("report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    result
}

/// Retry only a window initialization failure, before the guest has run.
fn retry_window(config: &Config, x11_available: bool, stderr: &str, console_empty: bool) -> bool {
    config.window
        && config.auto_window
        && !config.x11_window
        && x11_available
        && console_empty
        && stderr.contains("SDL window creation failed")
        && (stderr.contains("EGL") || stderr.contains("Wayland") || stderr.contains("wayland"))
}

#[cfg(test)]
#[path = "../tests/unit/venus.rs"]
mod tests;
