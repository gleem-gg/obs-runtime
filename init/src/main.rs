//! PID 1 for the Gleem OBS runtime container.
//!
//! Starts the desktop in the order it has to come up in, reaps zombies, and
//! shuts everything down when told. Exists so the image needs neither
//! supervisord nor a Python runtime just to sequence five processes — and so
//! that a component dying takes the container with it, rather than leaving a
//! rental that looks alive but has no desktop behind it.

use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// How long the X server gets to accept connections before we give up. A slow
/// machine under load can take a few seconds; a minute means it is broken.
const X_TIMEOUT: Duration = Duration::from_secs(60);

struct Service {
    name: &'static str,
    child: Child,
}

fn main() -> std::process::ExitCode {
    let resolution = env_or("GLEEM_RESOLUTION", "1920x1080");
    let framerate = env_or("GLEEM_FRAMERATE", "30");
    let encoder = env_or("GLEEM_ENCODER", "nvh264enc");

    log(&format!("starting: {resolution} at {framerate}fps, encoder {encoder}"));

    let mut services: Vec<Service> = Vec::new();

    // 1. The display. Everything else needs it, so nothing else starts until
    //    it is actually accepting connections.
    match spawn("Xvfb", "Xvfb", &[":0", "-screen", "0", &format!("{resolution}x24"), "-nolisten", "tcp"]) {
        Ok(service) => services.push(service),
        Err(error) => return fail("could not start Xvfb", &error, &mut services),
    }

    if !wait_for_display() {
        return fail("the X server never accepted connections", "timed out", &mut services);
    }

    // 2. Audio. OBS refuses to configure an audio source if no sink exists,
    //    even when nobody is listening to it.
    //
    //    The socket and the null sink are both loaded explicitly: PULSE_SERVER
    //    names a socket, and pulseaudio will not autospawn to satisfy it, so
    //    leaving it to the defaults gets a server that never starts and audio
    //    that silently never works.
    match spawn(
        "pulseaudio",
        "pulseaudio",
        &[
            "--exit-idle-time=-1",
            "--disallow-exit",
            "-n",
            "--load=module-native-protocol-unix socket=/run/pulse/native",
            "--load=module-null-sink sink_name=gleem object.linger=1 media.class=Audio/Sink",
            "--load=module-always-sink",
        ],
    ) {
        Ok(service) => services.push(service),
        Err(error) => log(&format!("pulseaudio did not start ({error}); continuing without audio")),
    }

    wait_for_audio();

    // 3. A window manager, or OBS's dialogs open without decorations and
    //    cannot be moved.
    match spawn("openbox", "openbox", &[]) {
        Ok(service) => services.push(service),
        Err(error) => log(&format!("openbox did not start ({error}); continuing")),
    }

    // 4. OBS. Started before the streamer so the desktop the renter first
    //    sees already has something on it.
    let obs_args = obs_arguments();
    let obs_borrowed: Vec<&str> = obs_args.iter().map(String::as_str).collect();
    match spawn("obs", "obs", &obs_borrowed) {
        Ok(service) => services.push(service),
        Err(error) => return fail("could not start OBS", &error, &mut services),
    }

    // 5. The streamer. This is what the agent connects to.
    match spawn_selkies(&resolution, &framerate, &encoder) {
        Ok(service) => services.push(service),
        Err(error) => return fail("could not start the desktop streamer", &error, &mut services),
    }

    log("desktop is up");

    supervise(&mut services)
}

/// Wait until the X server answers, rather than sleeping a fixed guess.
fn wait_for_display() -> bool {
    let deadline = Instant::now() + X_TIMEOUT;

    while Instant::now() < deadline {
        let probe = Command::new("xdpyinfo")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        if matches!(probe, Ok(status) if status.success()) {
            return true;
        }

        thread::sleep(Duration::from_millis(250));
    }

    false
}

/// Give PulseAudio a moment to create its socket before OBS looks for it.
/// OBS caches "no audio devices" at startup and does not re-check.
fn wait_for_audio() {
    let deadline = Instant::now() + Duration::from_secs(10);

    while Instant::now() < deadline {
        if std::path::Path::new("/run/pulse/native").exists() {
            return;
        }

        thread::sleep(Duration::from_millis(200));
    }

    log("the audio socket never appeared; OBS will start without audio");
}

fn obs_arguments() -> Vec<String> {
    // OBS writes its profile and scene collection under $HOME and fails
    // noisily if it cannot.
    if std::env::var_os("HOME").is_none() {
        unsafe { std::env::set_var("HOME", "/root") };
    }

    let mut args = vec![
        "--startvirtualcam".to_string(),
        "--disable-shutdown-check".to_string(),
        // No first-run wizard: nobody is sitting in front of this to dismiss
        // it, and it would be the first thing the renter saw.
        "--disable-updater".to_string(),
    ];

    if let Ok(password) = std::env::var("GLEEM_OBS_WEBSOCKET_PASSWORD") {
        if !password.is_empty() {
            args.push("--websocket_port".into());
            args.push("4455".into());
            args.push("--websocket_password".into());
            args.push(password);
        }
    }

    args
}

fn spawn_selkies(resolution: &str, framerate: &str, encoder: &str) -> Result<Service, String> {
    // GStreamer comes from the distribution now, so it is already on the
    // default search paths. The bundled build was dropped because its 1.24
    // nvcodec could not open an NVENC session against driver 610.
    const WEB_ROOT: &str = "/opt/selkies-web";

    // Where the NVRTC wheel puts its library. GStreamer dlopens it by
    // SONAME, so it has to be on the loader path, not merely installed.
    let nvrtc = "/opt/selkies/lib/python3.13/site-packages/nvidia/cuda_nvrtc/lib";

    let mut command = Command::new("/opt/selkies/bin/selkies-gstreamer");
    command
        .env("LD_LIBRARY_PATH", nvrtc)
        .args([
            // Bound to all interfaces *inside* the container. The isolation
            // comes from the agent publishing this on the host's loopback
            // only; binding container-loopback would just make the published
            // port unreachable.
            "--addr=0.0.0.0".to_string(),
            "--port=8082".to_string(),
            "--enable_basic_auth=false".to_string(),
            format!("--web_root={WEB_ROOT}"),
        ])
        // Left to itself Selkies relays through a public TURN server and
        // Google's STUN. Gleem force-relays through its own coturn precisely
        // so that neither party learns the other's address, and sending the
        // media through a third party instead would defeat the whole reason
        // for doing it. No TURN configuration means no session, not a
        // fallback to somebody else's relay.
        .args(turn_arguments())
        .env("SELKIES_ENCODER", encoder)
        .env("SELKIES_FRAMERATE", framerate)
        .env("SELKIES_RESOLUTION", resolution)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    command
        .spawn()
        .map(|child| Service { name: "selkies", child })
        .map_err(|error| error.to_string())
}

/// TURN settings, from the environment the agent set out of the rental's
/// start command.
fn turn_arguments() -> Vec<String> {
    let host = env_or("GLEEM_TURN_HOST", "");
    let secret = env_or("GLEEM_TURN_SECRET", "");

    if host.is_empty() || secret.is_empty() {
        log("no TURN configuration; this session will not be able to connect");
        return Vec::new();
    }

    vec![
        format!("--turn_host={host}"),
        format!("--turn_port={}", env_or("GLEEM_TURN_PORT", "3478")),
        format!("--turn_protocol={}", env_or("GLEEM_TURN_PROTOCOL", "udp")),
        format!("--turn_shared_secret={secret}"),
        // Selkies' STUN default is Google's. Our own relay answers STUN too,
        // so there is no reason to tell a third party that a session started.
        format!("--stun_host={host}"),
        format!("--stun_port={}", env_or("GLEEM_TURN_PORT", "3478")),
    ]
}

fn spawn(name: &'static str, binary: &str, args: &[&str]) -> Result<Service, String> {
    Command::new(binary)
        .args(args)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map(|child| Service { name, child })
        .map_err(|error| error.to_string())
}

/// Watch the services, and reap whatever PID 1 inherits.
///
/// If any of them exits, the container exits: a rental with a dead streamer
/// or a dead OBS is not a degraded rental, it is a black screen the renter is
/// being charged for. Better it fails visibly so the agent can report it.
fn supervise(services: &mut [Service]) -> std::process::ExitCode {
    loop {
        for service in services.iter_mut() {
            match service.child.try_wait() {
                Ok(Some(status)) => {
                    log(&format!("{} exited ({status}); shutting down", service.name));
                    shutdown(services);
                    return std::process::ExitCode::FAILURE;
                }
                Ok(None) => {}
                Err(error) => {
                    log(&format!("could not check on {}: {error}", service.name));
                }
            }
        }

        reap_orphans();
        thread::sleep(Duration::from_secs(2));
    }
}

/// As PID 1, orphaned processes are reparented here and have to be collected
/// or they accumulate as zombies for the life of the rental.
fn reap_orphans() {
    loop {
        // SAFETY: waitpid with WNOHANG on any child; it either reports a pid
        // or returns immediately.
        let pid = unsafe { libc_waitpid() };

        if pid <= 0 {
            break;
        }
    }
}

unsafe extern "C" {
    #[link_name = "waitpid"]
    fn raw_waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
}

unsafe fn libc_waitpid() -> i32 {
    const WNOHANG: i32 = 1;
    let mut status: i32 = 0;

    unsafe { raw_waitpid(-1, &mut status, WNOHANG) }
}

fn shutdown(services: &mut [Service]) {
    for service in services.iter_mut().rev() {
        let _ = service.child.kill();
        let _ = service.child.wait();
    }
}

fn fail(message: &str, detail: &str, services: &mut [Service]) -> std::process::ExitCode {
    log(&format!("{message}: {detail}"));
    shutdown(services);
    std::process::ExitCode::FAILURE
}

fn env_or(name: &str, fallback: &str) -> String {
    std::env::var(name).ok().filter(|value| !value.is_empty()).unwrap_or_else(|| fallback.to_string())
}

fn log(message: &str) {
    println!("[gleem-runtime] {message}");
}
