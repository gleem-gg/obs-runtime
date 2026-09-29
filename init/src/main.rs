//! PID 1 for the Gleem OBS runtime container.
//!
//! Starts the desktop in the order it has to come up in, reaps zombies, and
//! shuts everything down when told. Exists so the image needs neither
//! supervisord nor a Python runtime just to sequence five processes — and so
//! that a component dying takes the container with it, rather than leaving a
//! rental that looks alive but has no desktop behind it.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// How long the X server gets to accept connections before we give up. A slow
/// machine under load can take a few seconds; a minute means it is broken.
const X_TIMEOUT: Duration = Duration::from_secs(60);

/// How long OBS gets to save and exit after being asked to stop. It saved and
/// exited in well under ten seconds when measured; podman's stop timeout has
/// to be longer than this, or OBS is killed mid-save.
const OBS_STOP_TIMEOUT: Duration = Duration::from_secs(20);

/// Where OBS keeps its configuration: on the encrypted workspace, the only
/// place that outlives the container, so a renter's setup can be saved when
/// the rental ends and restored into the next one.
const OBS_CONFIG_HOME: &str = "/workspace/config";

/// Media and fonts a renter's scenes use. Fontconfig is pointed at the fonts
/// directory by /etc/fonts/conf.d/60-gleem-workspace.conf.
const WORKSPACE_MEDIA: &str = "/workspace/media";

/// The desktop background, rendered from assets/wallpaper/wallpaper.html.
const WALLPAPER: &str = "/usr/share/gleem/wallpaper.png";

/// Set by the signal handler when the container is asked to stop.
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

struct Service {
    name: &'static str,
    child: Child,
}

fn main() -> std::process::ExitCode {
    let resolution = env_or("GLEEM_RESOLUTION", "1920x1080");
    let framerate = env_or("GLEEM_FRAMERATE", "30");
    let encoder = env_or("GLEEM_ENCODER", "nvh264enc");

    log(&format!("starting: {resolution} at {framerate}fps, encoder {encoder}"));

    // Taken out of the environment before anything is spawned, so only OBS
    // gets them and nothing else inherits them by accident.
    let mut obs_env = take_obs_only_env();
    obs_env.push(("XDG_CONFIG_HOME", OBS_CONFIG_HOME.to_string()));

    // As PID 1, a signal with no handler is ignored. Without this, `podman
    // stop` waits out its timeout and then kills OBS without a chance to save.
    install_stop_handlers();

    if let Err(error) = prepare_workspace(Path::new(OBS_CONFIG_HOME), Path::new(WORKSPACE_MEDIA)) {
        log(&format!("could not prepare the workspace ({error}); OBS may start without a saved setup"));
    }

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

    paint_wallpaper();

    // 4. OBS. Started before the streamer so the desktop the renter first
    //    sees already has something on it.
    let obs_args = obs_arguments();
    let obs_borrowed: Vec<&str> = obs_args.iter().map(String::as_str).collect();
    match spawn_with_env("obs", "obs", &obs_borrowed, &obs_env) {
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

/// Paint the desktop behind OBS. Openbox draws no background, so without this
/// whatever OBS does not cover streams as black. Painted once: nothing
/// repaints the root window, and the resolution is fixed for the rental.
fn paint_wallpaper() {
    let painted = |binary: &str, args: &[&str]| {
        Command::new(binary)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };

    if painted("hsetroot", &["-cover", WALLPAPER]) {
        return;
    }

    log("could not paint the wallpaper; falling back to a solid colour");
    // The wallpaper's own base colour, so a failure still looks intended.
    painted("xsetroot", &["-solid", "#131317"]);
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
    // OBS itself is pointed at the workspace through XDG_CONFIG_HOME, but
    // other things it loads still expect a HOME.
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

/// The environment meant for OBS alone: the Developer API token and URL that
/// plugins such as OBS IRL Control use. The token reads the renter's IRL
/// Sidekicks, and no other service in the container has any use for it.
const OBS_ONLY_ENV: [&str; 2] = ["GLEEM_API_TOKEN", "GLEEM_API_URL"];

fn take_obs_only_env() -> Vec<(&'static str, String)> {
    OBS_ONLY_ENV
        .into_iter()
        .filter_map(|name| {
            let value = std::env::var(name).ok().filter(|value| !value.is_empty());
            // Still single-threaded here: nothing has been spawned yet.
            unsafe { std::env::remove_var(name) };
            value.map(|value| (name, value))
        })
        .collect()
}

fn spawn(name: &'static str, binary: &str, args: &[&str]) -> Result<Service, String> {
    spawn_with_env(name, binary, args, &[])
}

fn spawn_with_env(
    name: &'static str,
    binary: &str,
    args: &[&str],
    env: &[(&'static str, String)],
) -> Result<Service, String> {
    Command::new(binary)
        .args(args)
        .envs(env.iter().map(|(key, value)| (*key, value.as_str())))
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
        if STOP_REQUESTED.load(Ordering::SeqCst) {
            log("asked to stop; letting OBS save first");
            stop_gracefully(services);
            return std::process::ExitCode::SUCCESS;
        }

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
        // Short, so a stop request is acted on promptly: every moment spent
        // here comes out of podman's stop timeout.
        thread::sleep(Duration::from_millis(250));
    }
}

/// Stop OBS the way it saves on the way out, then everything else.
///
/// OBS 30 has no SIGTERM handler, so the signal podman sends kills it before
/// it writes the scene collection and profile. SIGINT it handles: it closes
/// the main window, saves and exits 0. Measured in this image, not assumed.
fn stop_gracefully(services: &mut [Service]) {
    if let Some(obs) = services.iter_mut().find(|service| service.name == "obs") {
        // SAFETY: kill(2) on a pid this process spawned and has not reaped.
        unsafe { raw_kill(obs.child.id() as i32, SIGINT) };

        let deadline = Instant::now() + OBS_STOP_TIMEOUT;
        loop {
            match obs.child.try_wait() {
                Ok(Some(status)) => {
                    log(&format!("obs exited ({status})"));
                    break;
                }
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(100)),
                _ => {
                    log("obs did not exit in time; killing it");
                    break;
                }
            }
        }
    }

    shutdown(services);
}

/// Make sure OBS's configuration and the media directories exist on the
/// workspace, and that OBS will not open its first-run wizard: nobody is at
/// this desktop to dismiss it, and it would be the first thing a renter saw.
///
/// `--disable-updater` does not suppress the wizard. OBS runs it when
/// `[General] FirstRun` is missing from global.ini, so a fresh workspace gets
/// a global.ini that says the first run is over. An existing one, restored
/// from the renter's saved setup, is left alone.
fn prepare_workspace(config_home: &Path, media: &Path) -> std::io::Result<()> {
    let obs = config_home.join("obs-studio");

    std::fs::create_dir_all(obs.join("basic/scenes"))?;
    std::fs::create_dir_all(obs.join("basic/profiles"))?;
    std::fs::create_dir_all(media.join("fonts"))?;

    let global = obs.join("global.ini");
    if !global.exists() {
        std::fs::write(&global, "[General]\nFirstRun=true\n")?;
    }

    Ok(())
}

const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;

extern "C" fn request_stop(_signal: i32) {
    // Only an atomic store: nothing else is safe in a signal handler.
    STOP_REQUESTED.store(true, Ordering::SeqCst);
}

fn install_stop_handlers() {
    // SAFETY: signal(2) with a handler that only stores to an atomic.
    unsafe {
        raw_signal(SIGTERM, request_stop);
        raw_signal(SIGINT, request_stop);
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

    #[link_name = "kill"]
    fn raw_kill(pid: i32, signal: i32) -> i32;

    #[link_name = "signal"]
    fn raw_signal(signal: i32, handler: extern "C" fn(i32)) -> usize;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepares_a_fresh_workspace_without_the_first_run_wizard() {
        let root = std::env::temp_dir().join(format!("runtime-init-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        prepare_workspace(&root.join("config"), &root.join("media")).unwrap();

        assert!(root.join("config/obs-studio/basic/scenes").is_dir());
        assert!(root.join("config/obs-studio/basic/profiles").is_dir());
        assert!(root.join("media/fonts").is_dir());
        assert_eq!(
            std::fs::read_to_string(root.join("config/obs-studio/global.ini")).unwrap(),
            "[General]\nFirstRun=true\n"
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn leaves_a_restored_global_ini_alone() {
        let root = std::env::temp_dir().join(format!("runtime-init-restored-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config/obs-studio")).unwrap();
        std::fs::write(root.join("config/obs-studio/global.ini"), "[General]\nFirstRun=true\nLanguage=de-DE\n").unwrap();

        prepare_workspace(&root.join("config"), &root.join("media")).unwrap();

        assert_eq!(
            std::fs::read_to_string(root.join("config/obs-studio/global.ini")).unwrap(),
            "[General]\nFirstRun=true\nLanguage=de-DE\n"
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn hands_the_api_token_to_obs_alone() {
        unsafe {
            std::env::set_var("GLEEM_API_TOKEN", "gleem_pat_a_b");
            std::env::set_var("GLEEM_API_URL", "");
        }

        let env = take_obs_only_env();

        // An empty URL means "not given", so the plugin falls back to its
        // own default rather than an empty base URL.
        assert_eq!(env, vec![("GLEEM_API_TOKEN", "gleem_pat_a_b".to_string())]);
        // Gone from init's environment, so no other service inherits it.
        assert!(std::env::var_os("GLEEM_API_TOKEN").is_none());
        assert!(std::env::var_os("GLEEM_API_URL").is_none());
    }
}
