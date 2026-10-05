//! PID 1 for the Gleem OBS runtime container.
//!
//! Starts the desktop in the order it has to come up in, reaps zombies, and
//! shuts everything down when told. Exists so the image needs neither
//! supervisord nor a Python runtime just to sequence five processes — and so
//! that a component dying takes the container with it, rather than leaving a
//! rental that looks alive but has no desktop behind it.

use std::path::{Path, PathBuf};
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

/// How often OBS may be restarted within `OBS_RESTART_WINDOW` before the
/// container gives up. OBS going away is usually the renter closing its
/// window, or a crash on the way out, and either way the desktop is still
/// there to put it back on. One that dies this often is not coming back.
const OBS_MAX_RESTARTS: usize = 5;
const OBS_RESTART_WINDOW: Duration = Duration::from_secs(10 * 60);

/// Where OBS keeps its configuration: on the encrypted workspace, the only
/// place that outlives the container, so a renter's setup can be saved when
/// the rental ends and restored into the next one.
const OBS_CONFIG_HOME: &str = "/workspace/config";

/// Media and fonts a renter's scenes use. Fontconfig is pointed at the fonts
/// directory by /etc/fonts/conf.d/60-gleem-workspace.conf.
const WORKSPACE_MEDIA: &str = "/workspace/media";

/// Where OBS records to. On the workspace, like everything else a rental
/// writes, but outside the saved setup: a recording is not configuration.
/// Imported profiles point here, so it has to exist before OBS starts.
const WORKSPACE_RECORDINGS: &str = "/workspace/recordings";

/// The desktop background, rendered from assets/wallpaper/wallpaper.html.
const WALLPAPER: &str = "/usr/share/gleem/wallpaper.png";

/// VirtualGL's launcher. It preloads the library that sends OBS's OpenGL to
/// the GPU instead of Xvfb, then execs its arguments, so OBS keeps the PID
/// and still gets the SIGINT it saves on.
const VGLRUN: &str = "/opt/VirtualGL/bin/vglrun";

/// Where the container runtime puts the GPU's device nodes.
const DRI_DIR: &str = "/dev/dri";

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

    if let Err(error) = prepare_workspace(Path::new(OBS_CONFIG_HOME), Path::new(WORKSPACE_MEDIA), Path::new(WORKSPACE_RECORDINGS)) {
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
    //
    //    Xvfb has no GPU behind it, so on its own OBS would composite every
    //    scene with Mesa's software renderer, and the browser source would
    //    draw WebGL with SwiftShader: a single 1080p WebGL page cost about
    //    five CPU cores and still ran at 4 fps. Under VirtualGL both render on
    //    the GPU (measured on the reference RTX 3060: 30 fps, under one core).
    //    Only the GPU's own nodes are in /dev/dri: CDI injects nothing else.
    let (obs_binary, obs_args) = match gpu_card(Path::new(DRI_DIR)) {
        Some(card) => {
            log(&format!("rendering OBS on {} through VirtualGL", card.display()));
            let mut args = vec!["-d".to_string(), card.display().to_string(), "obs".to_string()];
            args.extend(obs_arguments());
            (VGLRUN, args)
        }
        None => {
            log("no GPU device node; OBS renders in software");
            ("obs", obs_arguments())
        }
    };
    let spawn_obs = || {
        clear_crash_sentinel(Path::new(OBS_CONFIG_HOME));
        let borrowed: Vec<&str> = obs_args.iter().map(String::as_str).collect();
        spawn_with_env("obs", obs_binary, &borrowed, &obs_env)
    };
    match spawn_obs() {
        Ok(service) => services.push(service),
        Err(error) => return fail("could not start OBS", &error, &mut services),
    }

    // 5. The streamer. This is what the agent connects to.
    match spawn_selkies(&resolution, &framerate, &encoder) {
        Ok(service) => services.push(service),
        Err(error) => return fail("could not start the desktop streamer", &error, &mut services),
    }

    log("desktop is up");

    supervise(&mut services, spawn_obs)
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

/// The first DRM card node, which VirtualGL opens through EGL. Card nodes,
/// not render nodes: VirtualGL's EGL back end takes a card.
fn gpu_card(dri: &Path) -> Option<PathBuf> {
    let mut cards: Vec<PathBuf> = std::fs::read_dir(dri)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("card"))
        .map(|entry| entry.path())
        .collect();
    cards.sort();
    cards.into_iter().next()
}

fn obs_arguments() -> Vec<String> {
    // OBS itself is pointed at the workspace through XDG_CONFIG_HOME, but
    // other things it loads still expect a HOME.
    if std::env::var_os("HOME").is_none() {
        unsafe { std::env::set_var("HOME", "/root") };
    }

    // No --startvirtualcam: the virtual camera needs a v4l2loopback device,
    // which a rental container never has, and since OBS 31 asking for it
    // anyway greets the renter with a "Failed to start virtual camera" error.
    // The first-run wizard is kept away by prepare_workspace, not by a flag.
    let mut args = vec!["--disable-shutdown-check".to_string(), "--disable-updater".to_string()];

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
/// OBS is put back when it exits: a renter who closes its window, or an OBS
/// that crashes on the way out, still has a desktop and a stream, and ending
/// the whole rental over it left them on a dead session. Anything else
/// exiting takes the container with it: a rental with a dead display or
/// streamer is not a degraded rental, it is a black screen the renter is
/// being charged for. Better it fails visibly so the agent can report it.
fn supervise(
    services: &mut [Service],
    spawn_obs: impl Fn() -> Result<Service, String>,
) -> std::process::ExitCode {
    let mut obs_restarts = RestartBudget::new(OBS_MAX_RESTARTS, OBS_RESTART_WINDOW);

    loop {
        if STOP_REQUESTED.load(Ordering::SeqCst) {
            log("asked to stop; letting OBS save first");
            stop_gracefully(services);
            return std::process::ExitCode::SUCCESS;
        }

        for service in services.iter_mut() {
            match service.child.try_wait() {
                Ok(Some(status)) if service.name == "obs" && obs_restarts.allow(Instant::now()) => {
                    log(&format!("obs exited ({status}); starting it again"));

                    match spawn_obs() {
                        Ok(obs) => *service = obs,
                        Err(error) => {
                            log(&format!("could not start OBS again ({error}); shutting down"));
                            shutdown(services);
                            return std::process::ExitCode::FAILURE;
                        }
                    }
                }
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

/// Restarts allowed within a sliding window.
struct RestartBudget {
    limit: usize,
    window: Duration,
    recent: Vec<Instant>,
}

impl RestartBudget {
    fn new(limit: usize, window: Duration) -> Self {
        Self { limit, window, recent: Vec::new() }
    }

    /// Whether one more restart at `now` is within budget; counts it if so.
    fn allow(&mut self, now: Instant) -> bool {
        self.recent.retain(|at| now.duration_since(*at) < self.window);

        if self.recent.len() >= self.limit {
            return false;
        }

        self.recent.push(now);
        true
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
/// `--disable-updater` does not suppress the wizard. OBS runs it on a first
/// start that has no `[General] LastVersion` in global.ini, so a fresh
/// workspace gets a global.ini with one. `FirstRun` would suppress it too,
/// but since OBS 31 the same flag also stops OBS from giving a new scene
/// collection its Desktop Audio source, and a renter would start without
/// sound. The value only has to exist and be 31.0.0 or later, so OBS does not
/// try to migrate settings from an OBS 30 layout; OBS overwrites it with its
/// own version on the first start.
///
/// Anything restored from the renter's saved setup is left alone. That
/// includes a setup saved by OBS 30, which has a global.ini and no user.ini:
/// OBS moves the user settings across itself, and refuses to if a user.ini
/// already exists.
fn prepare_workspace(config_home: &Path, media: &Path, recordings: &Path) -> std::io::Result<()> {
    let obs = config_home.join("obs-studio");

    std::fs::create_dir_all(obs.join("basic/scenes"))?;
    std::fs::create_dir_all(obs.join("basic/profiles"))?;
    std::fs::create_dir_all(media.join("fonts"))?;
    std::fs::create_dir_all(recordings)?;

    let global = obs.join("global.ini");
    if !global.exists() && !obs.join("user.ini").exists() {
        std::fs::write(&global, format!("[General]\nLastVersion={}\n", OBS_31))?;
    }

    // obs-websocket's server, on for every rental: renters connect their own
    // tools to it through Gleem's gateway. --websocket_port and
    // --websocket_password only override those two settings; with
    // server_enabled left at its default of false the server never starts.
    // Written fresh each time, since it is the platform's setting, not the
    // renter's, and the password on the command line wins over this one.
    let websocket = obs.join("plugin_config/obs-websocket");
    std::fs::create_dir_all(&websocket)?;
    std::fs::write(websocket.join("config.json"), OBS_WEBSOCKET_CONFIG)?;

    Ok(())
}

/// Forget that OBS last exited uncleanly.
///
/// OBS 32 ignores `--disable-shutdown-check` and, after any unclean exit,
/// opens a modal "Crash Detected" dialog offering safe mode, behind which the
/// whole window waits. The sentinel lives on the workspace, so it also rides
/// along in a saved setup and would greet the renter at the start of their
/// next rental. Nobody here can answer it before connecting, and the answer
/// is always to start normally.
fn clear_crash_sentinel(config_home: &Path) {
    let sentinel = config_home.join("obs-studio/.sentinel");

    match std::fs::remove_dir_all(&sentinel) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => log(&format!("could not clear OBS's crash sentinel ({error}); it may ask about safe mode")),
    }
}

/// OBS 31.0.0 as OBS packs a version: major, minor and patch in one integer.
const OBS_31: u32 = 31 << 24;

const OBS_WEBSOCKET_CONFIG: &str = r#"{
  "alerts_enabled": false,
  "auth_required": true,
  "first_load": false,
  "server_enabled": true,
  "server_port": 4455
}
"#;

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
    fn picks_the_first_card_node_and_ignores_render_nodes() {
        let dri = std::env::temp_dir().join(format!("runtime-init-dri-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dri);
        std::fs::create_dir_all(&dri).unwrap();
        assert_eq!(gpu_card(&dri), None);

        for name in ["renderD128", "card2", "card1", "by-path"] {
            std::fs::write(dri.join(name), "").unwrap();
        }
        assert_eq!(gpu_card(&dri), Some(dri.join("card1")));
        assert_eq!(gpu_card(&dri.join("missing")), None);

        std::fs::remove_dir_all(&dri).unwrap();
    }

    #[test]
    fn prepares_a_fresh_workspace_without_the_first_run_wizard() {
        let root = std::env::temp_dir().join(format!("runtime-init-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        prepare_workspace(&root.join("config"), &root.join("media"), &root.join("recordings")).unwrap();

        assert!(root.join("config/obs-studio/basic/scenes").is_dir());
        assert!(root.join("config/obs-studio/basic/profiles").is_dir());
        assert!(root.join("media/fonts").is_dir());
        assert!(root.join("recordings").is_dir());
        assert!(
            std::fs::read_to_string(root.join("config/obs-studio/plugin_config/obs-websocket/config.json"))
                .unwrap()
                .contains(r#""server_enabled": true"#)
        );
        assert_eq!(
            std::fs::read_to_string(root.join("config/obs-studio/global.ini")).unwrap(),
            "[General]\nLastVersion=520093696\n"
        );
        // FirstRun belongs to OBS: set early, it costs a new scene collection
        // its Desktop Audio source.
        assert!(!root.join("config/obs-studio/user.ini").exists());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn leaves_a_restored_global_ini_alone() {
        let root = std::env::temp_dir().join(format!("runtime-init-restored-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config/obs-studio")).unwrap();
        std::fs::write(root.join("config/obs-studio/global.ini"), "[General]\nFirstRun=true\nLanguage=de-DE\n").unwrap();

        prepare_workspace(&root.join("config"), &root.join("media"), &root.join("recordings")).unwrap();

        assert_eq!(
            std::fs::read_to_string(root.join("config/obs-studio/global.ini")).unwrap(),
            "[General]\nFirstRun=true\nLanguage=de-DE\n"
        );
        // Saved by OBS 30: OBS 32 migrates it only while user.ini is absent.
        assert!(!root.join("config/obs-studio/user.ini").exists());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn clears_the_crash_sentinel_so_obs_starts_without_asking() {
        let root = std::env::temp_dir().join(format!("runtime-init-sentinel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("obs-studio/.sentinel")).unwrap();
        std::fs::write(root.join("obs-studio/.sentinel/run_1"), "").unwrap();

        clear_crash_sentinel(&root);
        assert!(!root.join("obs-studio/.sentinel").exists());
        // Nothing to clear is not an error.
        clear_crash_sentinel(&root);

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn allows_a_few_restarts_and_then_gives_up() {
        let start = Instant::now();
        let mut budget = RestartBudget::new(2, Duration::from_secs(60));

        assert!(budget.allow(start));
        assert!(budget.allow(start + Duration::from_secs(1)));
        assert!(!budget.allow(start + Duration::from_secs(2)));
        // Once the early restarts fall out of the window there is room again.
        assert!(budget.allow(start + Duration::from_secs(61)));
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
