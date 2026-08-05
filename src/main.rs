mod platform;

use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Cursor};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use platform::wayland::{self, MimeSource, MimeType, Source, Watcher as WaylandWatcher};
use platform::x11::{Clipboard as XClipboard, OwnerToken, Target as XTarget, Watcher as XWatcher};
use quick_xml::{Reader, XmlVersion, events::Event};
use rustix::process::{Pid, Signal, kill_process};
use signal_hook::consts::{SIGCHLD, SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use url::Url;

const PNG_MIME: &str = "image/png";
const HTML_MIME: &str = "text/html";
const URI_LIST_MIME: &str = "text/uri-list";
const QQ_RICH_MIME: &str = "QQ_Unicode_RichEdit_Format";
const OWN_MIME: &str = "application/x-qq-wayland-clipboard";
const MAX_SELECTION_BYTES: usize = 512 * 1024 * 1024;
const X_READ_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
struct XTargets {
    image_png: XTarget,
    qq_rich: XTarget,
    text: XTarget,
    html: XTarget,
}

impl XTargets {
    fn new(clipboard: &XClipboard) -> Result<Self> {
        Ok(Self {
            image_png: clipboard.target(PNG_MIME)?,
            qq_rich: clipboard.target(QQ_RICH_MIME)?,
            text: clipboard.target("UTF8_STRING")?,
            html: clipboard.target(HTML_MIME)?,
        })
    }
}

/// ClipboardJob tracks the generation of the X11 and Wayland clipboards at the time a copy was observed.
/// If either generation has changed since the job was created, the content is considered outdated and should not be mirrored to the other side.
#[derive(Clone, Copy)]
struct ClipboardJob {
    owner: OwnerToken,
    x_generation: u64,
    wayland_generation: u64,
}

impl ClipboardJob {
    fn is_current(&self, x_generation: &AtomicU64, wayland_generation: &AtomicU64) -> bool {
        self.x_generation == x_generation.load(Ordering::SeqCst)
            && self.wayland_generation == wayland_generation.load(Ordering::SeqCst)
    }

    fn ensure_current(
        &self,
        x_generation: &AtomicU64,
        wayland_generation: &AtomicU64,
    ) -> Result<()> {
        if !self.is_current(x_generation, wayland_generation) {
            return Err(Superseded.into());
        }
        Ok(())
    }
}

#[derive(Debug)]
struct Superseded;

impl std::fmt::Display for Superseded {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("clipboard changed while processing")
    }
}

impl std::error::Error for Superseded {}

/// A buffer that holds the latest clipboard job, replacing any previous job if a new copy is observed.
#[derive(Default)]
struct LatestJob {
    job: Mutex<Option<ClipboardJob>>,
    ready: Condvar,
}

impl LatestJob {
    fn push(&self, job: ClipboardJob) {
        *self.job.lock().unwrap() = Some(job);
        self.ready.notify_one();
    }

    fn pop(&self) -> ClipboardJob {
        let mut job = self.job.lock().unwrap();
        loop {
            if let Some(job) = job.take() {
                return job;
            }
            job = self.ready.wait(job).unwrap();
        }
    }
}

fn normalize_png(data: Vec<u8>) -> Result<Option<Vec<u8>>> {
    let format = match image::guess_format(&data) {
        Ok(format) => format,
        Err(_) => return Ok(None),
    };
    if format == image::ImageFormat::Png {
        return Ok(Some(data));
    }

    let image = image::ImageReader::with_format(Cursor::new(&data), format)
        .decode()
        .with_context(|| format!("failed to decode {format:?} returned by QQ"))?;
    let mut png = Cursor::new(Vec::new());
    image
        .write_to(&mut png, image::ImageFormat::Png)
        .context("failed to encode image as PNG")?;
    let png = png.into_inner();
    ensure!(
        png.len() <= MAX_SELECTION_BYTES,
        "encoded PNG exceeds {MAX_SELECTION_BYTES} bytes"
    );
    Ok(Some(png))
}

fn qq_file_uri(data: &[u8]) -> Option<Vec<u8>> {
    let mut reader = Reader::from_reader(data);
    let mut file_path = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(element) | Event::Empty(element))
                if element.name().as_ref() == b"EditElement" =>
            {
                let mut element_type = None;
                let mut path = None;
                for attribute in element.attributes() {
                    let attribute = attribute.ok()?;
                    let value = attribute
                        .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
                        .ok()?
                        .into_owned();
                    match attribute.key.as_ref() {
                        b"type" => element_type = Some(value),
                        b"filepath" => path = Some(value),
                        _ => {}
                    }
                }
                if element_type.as_deref() == Some("4") {
                    if file_path.is_some() {
                        return None;
                    }
                    file_path = path;
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => return None,
        }
    }

    let file_path = file_path?;
    let path = std::path::Path::new(&file_path);
    if !path.is_file() {
        return None;
    }

    let mut uri = Url::from_file_path(path).ok()?.to_string().into_bytes();
    uri.extend(b"\r\n");
    Some(uri)
}

struct ChildGuard(Child);

impl ChildGuard {
    fn terminate(&mut self) -> io::Result<()> {
        if self.0.try_wait()?.is_none() {
            let _ = kill_process(Pid::from_child(&self.0), Signal::TERM);
            for _ in 0..40 {
                if self.0.try_wait()?.is_some() {
                    return Ok(());
                }
                thread::sleep(Duration::from_millis(50));
            }
            self.0.kill()?;
            self.0.wait()?;
        }
        Ok(())
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

#[derive(Parser)]
#[command(about = "Launch Linux QQ with Wayland clipboard compatibility")]
struct Cli {
    /// Use a fixed Xvfb display number instead of allocating one automatically
    #[arg(long, value_name = "NUMBER")]
    display: Option<u16>,

    #[arg(value_name = "QQ")]
    qq: OsString,

    #[arg(
        value_name = "QQ_ARGUMENT",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    qq_args: Vec<OsString>,
}

enum LaunchEvent {
    Signal(i32),
    Service(&'static str, thread::Result<Result<()>>),
}

fn main() -> Result<()> {
    let Cli {
        display: requested_display,
        qq: qq_command,
        qq_args,
    } = Cli::parse();
    std::env::var_os("WAYLAND_DISPLAY")
        .context("WAYLAND_DISPLAY is unset; this wrapper requires a Wayland session")?;

    let (events, event_receiver) = mpsc::channel();

    // Child Process 1: Start a dummy X server to isolate QQ from the real X11 clipboard so it doesn't interfere with other clipboards.
    let mut xvfb = Command::new("Xvfb");
    if let Some(number) = requested_display {
        xvfb.arg(format!(":{number}"));
    }
    let child = xvfb
        .args(["-displayfd", "1"])
        .args(["-nolisten", "tcp"])
        .args(["-screen", "0", "1x1x24"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to start Xvfb; install xorg-server-xvfb")?;
    let mut xvfb = ChildGuard(child);
    let stdout = xvfb.0.stdout.take().context("missing Xvfb stdout")?;
    let mut line = String::new();
    if BufReader::new(stdout).read_line(&mut line)? == 0 {
        let status = xvfb.0.wait()?;
        bail!("Xvfb exited during startup: {status}");
    }
    let number: u16 = line
        .trim()
        .parse()
        .with_context(|| format!("invalid Xvfb display number: {line:?}"))?;
    let display = format!(":{number}");

    // To prevent race conditions, we track the generation of the X11 and Wayland clipboards.
    // Each time a copy is observed, the corresponding generation is incremented.
    // If either generation has changed by the time the job is processed, it is considered outdated and will not be mirrored.
    let x_generation = Arc::new(AtomicU64::new(0));
    let wayland_generation = Arc::new(AtomicU64::new(0));
    let observed_wayland_generation = wayland_generation.clone();
    let wayland_watcher = WaylandWatcher::new(move |mime_types| {
        if !mime_types.iter().any(|mime_type| mime_type == OWN_MIME) {
            observed_wayland_generation.fetch_add(1, Ordering::SeqCst);
        }
    })?;
    let jobs = Arc::new(LatestJob::default());
    let x_watcher = XWatcher::new(&display).context("failed to watch isolated X11 clipboard")?;

    // Worker Type 1: Watch for Wayland clipboard changes and increment the generation when a copy is observed.
    let service_events = events.clone();
    thread::spawn(move || {
        let result = catch_unwind(AssertUnwindSafe(|| wayland_watcher.run()));
        let _ = service_events.send(LaunchEvent::Service("Wayland watcher", result));
    });

    // Worker Type 2: The producer that watches the isolated X11 clipboard for copies and pushes jobs to the queue.
    let observed_x_generation = x_generation.clone();
    let current_wayland_generation = wayland_generation.clone();
    let pending_jobs = jobs.clone();
    let service_events = events.clone();
    thread::spawn(move || {
        let result = catch_unwind(AssertUnwindSafe(|| {
            x_watcher.run(move |owner| {
                // When QQ copies to the X11 clipboard, we increment the generation and push a new job to the queue.
                let x_generation = observed_x_generation.fetch_add(1, Ordering::SeqCst) + 1;
                pending_jobs.push(ClipboardJob {
                    owner,
                    x_generation,
                    wayland_generation: current_wayland_generation.load(Ordering::SeqCst),
                });
            })
        }));
        let _ = service_events.send(LaunchEvent::Service("X11 watcher", result));
    });

    // Worker Type 3: The consumers that process jobs from the queue and mirror the content to the other clipboard.
    // We use two threads to allow concurrent mirroring, which is useful when user quickly copys twice in a row.
    for name in ["clipboard worker 1", "clipboard worker 2"] {
        let clipboard =
            XClipboard::new(&display).context("failed to open isolated X11 clipboard")?;
        let targets = XTargets::new(&clipboard)?;
        let jobs = jobs.clone();
        let x_generation = x_generation.clone();
        let wayland_generation = wayland_generation.clone();
        let service_events = events.clone();
        thread::spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                bridge(clipboard, targets, jobs, x_generation, wayland_generation)
            }));
            let _ = service_events.send(LaunchEvent::Service(name, result));
        });
    }

    let wayland = std::env::var("WAYLAND_DISPLAY").unwrap_or_default();
    eprintln!("QQ Wayland clipboard wrapper: Wayland={wayland}, isolated X11={display}");

    // Child Process 2: Start QQ, of course.
    let child = Command::new(&qq_command)
        .arg("--ozone-platform=wayland")
        .args(qq_args)
        .env("DISPLAY", &display)
        .env("ELECTRON_OZONE_PLATFORM_HINT", "wayland")
        .spawn()
        .with_context(|| format!("failed to start {}", qq_command.to_string_lossy()))?;
    let mut qq = ChildGuard(child);

    // Worker Type 4: Forward signals to the main event loop so we can handle all events in a single place.
    let mut signals = Signals::new([SIGCHLD, SIGHUP, SIGINT, SIGTERM])?;
    let signal_events = events.clone();
    thread::spawn(move || {
        for signal in signals.forever() {
            if signal_events.send(LaunchEvent::Signal(signal)).is_err() {
                break;
            }
        }
    });

    // Send a SIGCHLD immediately to check if QQ or Xvfb exited during startup.
    if events.send(LaunchEvent::Signal(SIGCHLD)).is_err() {
        bail!("event loop disconnected");
    }

    loop {
        match event_receiver.recv().context("event loop disconnected")? {
            LaunchEvent::Signal(SIGCHLD) => {
                if let Some(status) = qq.0.try_wait()? {
                    ensure!(status.success(), "QQ exited: {status}");
                    return Ok(());
                }
                if let Some(status) = xvfb.0.try_wait()? {
                    bail!("Xvfb exited: {status}");
                }
            }
            LaunchEvent::Signal(_) => {
                qq.terminate()?;
                return Ok(());
            }
            LaunchEvent::Service(name, result) => match result {
                Ok(Ok(())) => bail!("{name} exited"),
                Ok(Err(error)) => return Err(error).with_context(|| format!("{name} exited")),
                Err(_) => bail!("{name} panicked"),
            },
        }
    }
}

/// Bridge X11 [clipboard] with supported [targets] to Wayland, processing jobs from the [jobs] queue.
///
/// The [x_generation] and [wayland_generation] are used to ensure that only the latest copy is mirrored.
fn bridge(
    mut clipboard: XClipboard,
    targets: XTargets,
    jobs: Arc<LatestJob>,
    x_generation: Arc<AtomicU64>,
    wayland_generation: Arc<AtomicU64>,
) -> Result<()> {
    loop {
        let job = jobs.pop();

        let result = mirror(
            &mut clipboard,
            targets,
            job,
            &x_generation,
            &wayland_generation,
        );
        if let Err(error) = result
            && !error.is::<Superseded>()
        {
            eprintln!("failed to mirror isolated X11 clipboard: {error:#}");
        }

        if job.x_generation == x_generation.load(Ordering::SeqCst)
            && job.wayland_generation != wayland_generation.load(Ordering::SeqCst)
        {
            clipboard.clear_if_owner(job.owner)?;
        }
    }
}

fn mirror(
    clipboard: &mut XClipboard,
    targets: XTargets,
    job: ClipboardJob,
    x_generation: &AtomicU64,
    wayland_generation: &AtomicU64,
) -> Result<()> {
    // Note in this method how we insert many checkpoints to ensure that the job is still current.
    // We are actually manually implementing a "cancellation token" pattern here.

    job.ensure_current(x_generation, wayland_generation)?;
    let offered = match clipboard.targets(MAX_SELECTION_BYTES, X_READ_TIMEOUT, || {
        job.ensure_current(x_generation, wayland_generation)
    }) {
        Ok(targets) => targets,
        Err(error) if error.is::<Superseded>() => return Err(error),
        // FIXME: allow one retry to fix some annoying situations.
        Err(first_error) => match clipboard.targets(MAX_SELECTION_BYTES, X_READ_TIMEOUT, || {
            job.ensure_current(x_generation, wayland_generation)
        }) {
            Ok(targets) => targets,
            Err(error) if error.is::<Superseded>() => return Err(error),
            Err(error) => {
                eprintln!(
                    "failed to read X11 clipboard targets after retry: {error} (first attempt: {first_error})"
                );
                return Ok(());
            }
        },
    };

    // Case 1: Handle pure image.
    // Note that QQ ALWAYS offers image/png even when the content is not a PNG (e.g. a JPEG).
    if offered.contains(&targets.image_png) {
        let image = match clipboard
            .read(
                targets.image_png,
                MAX_SELECTION_BYTES,
                X_READ_TIMEOUT,
                || job.ensure_current(x_generation, wayland_generation),
            )
            .and_then(|image| image.map_or(Ok(None), normalize_png))
        {
            Ok(Some(image)) => image,
            Ok(None) => {
                eprintln!("X11 image/png owner returned no supported image");
                return Ok(());
            }
            Err(error) if error.is::<Superseded>() => return Err(error),
            Err(error) => {
                eprintln!("failed to read or convert image from isolated X11: {error}");
                return Ok(());
            }
        };
        let size = image.len();
        if publish(
            clipboard,
            job,
            x_generation,
            wayland_generation,
            vec![MimeSource {
                source: Source::Bytes(image.into_boxed_slice()),
                mime_type: MimeType::Specific(PNG_MIME.to_owned()),
            }],
        )? {
            eprintln!("mirrored image/png: {size} bytes");
        }
        return Ok(());
    }

    // Case 2: Handle text, preferring QQ rich text before its plain text and HTML fallbacks.
    if !offered.contains(&targets.qq_rich) && !offered.contains(&targets.text) {
        eprintln!(
            "left unsupported X11 clipboard owner unchanged: {}",
            job.owner.id()
        );
        return Ok(());
    }

    let mut sources = Vec::new();
    let mut file_uri = None;
    for (target, mime_type) in [
        (targets.qq_rich, MimeType::Specific(QQ_RICH_MIME.to_owned())),
        (targets.text, MimeType::Text),
        (targets.html, MimeType::Specific(HTML_MIME.to_owned())),
    ] {
        if !offered.contains(&target) {
            continue;
        }
        let data = match clipboard.read(target, MAX_SELECTION_BYTES, X_READ_TIMEOUT, || {
            job.ensure_current(x_generation, wayland_generation)
        }) {
            Ok(Some(data)) => data,
            Ok(None) => {
                eprintln!("X11 clipboard owner refused {mime_type:?}");
                return Ok(());
            }
            Err(error) if error.is::<Superseded>() => return Err(error),
            Err(error) => {
                eprintln!("failed to read {mime_type:?} from isolated X11: {error}");
                return Ok(());
            }
        };
        if target == targets.qq_rich {
            file_uri = qq_file_uri(&data);
        }
        sources.push(MimeSource {
            source: Source::Bytes(data.into_boxed_slice()),
            mime_type,
        });
    }
    if let Some(uri) = file_uri {
        sources.push(MimeSource {
            source: Source::Bytes(uri.into_boxed_slice()),
            mime_type: MimeType::Specific(URI_LIST_MIME.to_owned()),
        });
    }
    let count = sources.len();
    if publish(clipboard, job, x_generation, wayland_generation, sources)? {
        eprintln!("mirrored text clipboard: {count} MIME types");
    }
    Ok(())
}

fn publish(
    clipboard: &XClipboard,
    job: ClipboardJob,
    x_generation: &AtomicU64,
    wayland_generation: &AtomicU64,
    mut sources: Vec<MimeSource>,
) -> Result<bool> {
    job.ensure_current(x_generation, wayland_generation)?;
    if !clipboard.clear_if_owner(job.owner)? {
        return Ok(false);
    }
    job.ensure_current(x_generation, wayland_generation)?;
    if !clipboard.is_empty()? {
        return Ok(false);
    }

    sources.push(MimeSource {
        source: Source::Bytes(Vec::new().into_boxed_slice()),
        mime_type: MimeType::Specific(OWN_MIME.to_owned()),
    });
    wayland::copy(sources)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_current_job_can_commit() {
        let x_generation = AtomicU64::new(3);
        let wayland_generation = AtomicU64::new(5);
        let job = ClipboardJob {
            owner: OwnerToken::new(1, 1),
            x_generation: 3,
            wayland_generation: 5,
        };

        assert!(job.is_current(&x_generation, &wayland_generation));
        x_generation.store(4, Ordering::SeqCst);
        assert!(!job.is_current(&x_generation, &wayland_generation));
        x_generation.store(3, Ordering::SeqCst);
        wayland_generation.store(6, Ordering::SeqCst);
        assert!(!job.is_current(&x_generation, &wayland_generation));
    }

    #[test]
    fn pending_job_is_replaced_by_newer_copy() {
        let jobs = LatestJob::default();
        for x_generation in [1, 2] {
            jobs.push(ClipboardJob {
                owner: OwnerToken::new(x_generation, x_generation),
                x_generation: x_generation.into(),
                wayland_generation: 0,
            });
        }

        assert_eq!(jobs.pop().x_generation, 2);
    }

    #[test]
    fn qq_file_is_exposed_as_uri_list() {
        let path = std::env::current_exe().unwrap();
        let xml = format!(
            r#"<QQRichEditFormat><EditElement type="4" filepath="{}" shortcut=""/></QQRichEditFormat>"#,
            path.display()
        );
        let uri = qq_file_uri(xml.as_bytes()).unwrap();
        let expected = format!("{}\r\n", Url::from_file_path(path).unwrap());

        assert_eq!(uri, expected.as_bytes());
    }

    #[test]
    fn relative_qq_file_path_is_ignored() {
        let xml =
            r#"<QQRichEditFormat><EditElement type="4" filepath="Cargo.toml"/></QQRichEditFormat>"#;

        assert_eq!(qq_file_uri(xml.as_bytes()), None);
    }
}
