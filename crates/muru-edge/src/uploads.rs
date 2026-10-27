//! Upload offloading (design specification section 4.11). On a route with
//! `upload` set the edge receives the request body before the application
//! sees the request, stores each file in the spool, and passes the files
//! to the worker by descriptor with the request head. A multipart form
//! keeps its ordinary fields and has each file part replaced by text
//! fields that describe the stored file; a raw upload's whole body becomes
//! one stored file and the application sees an empty body.
//!
//! Files are unnamed (O_TMPFILE) where the file system allows it, so they
//! vanish with the last descriptor, crashes included. Elsewhere, macOS
//! among them, a file is named after the edge's pid in the application's
//! spool directory, removed when the request ends, and swept up by the
//! next edge when the one that wrote it has gone.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Seek as _};
use std::os::fd::{AsFd as _, BorrowedFd};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures::Stream;
use http::{HeaderMap, StatusCode};
use http_body_util::BodyExt as _;
use hyper::body::Incoming;
use muru_proto::edge::v1 as edge_pb;
use muru_proto::worker::v1 as pb;
use tokio::io::AsyncWriteExt as _;

use crate::errors::Problem;
use crate::metrics::Sample;

/// What the ordinary fields of a multipart form may hold together: they
/// stay in memory and reach the application as the request body.
pub const FIELDS_LIMIT: usize = 10 * 1024 * 1024;

/// How a route's uploads are received.
#[derive(Debug)]
pub struct Policy {
    /// The whole body is one file, rather than a multipart form.
    raw: bool,
    max_file_size: Option<u64>,
    max_files: usize,
    hash: Vec<Algorithm>,
    /// Ask the application with the request's headers alone first.
    pub precheck: bool,
    spool_quota: Option<u64>,
}

impl Policy {
    /// The policy a snapshot's route carries; none for a mode the edge
    /// does not know.
    pub fn from_message(message: &edge_pb::Upload) -> Option<Arc<Self>> {
        let raw = match message.mode.as_str() {
            "raw" => true,
            "multipart" => false,
            _ => return None,
        };

        Some(Arc::new(Self {
            raw,
            max_file_size: (message.max_file_size > 0).then_some(message.max_file_size),
            max_files: if message.max_files == 0 {
                100
            } else {
                message.max_files as usize
            },
            hash: message
                .hash
                .iter()
                .filter_map(|name| Algorithm::named(name))
                .collect(),
            precheck: message.precheck,
            spool_quota: (message.spool_quota > 0).then_some(message.spool_quota),
        }))
    }
}

/// A digest computed while a file arrives.
#[derive(Debug, Clone, Copy)]
enum Algorithm {
    Sha256,
    Sha384,
    Sha512,
    Sha1,
}

impl Algorithm {
    fn named(name: &str) -> Option<Self> {
        match name {
            "sha256" => Some(Self::Sha256),
            "sha384" => Some(Self::Sha384),
            "sha512" => Some(Self::Sha512),
            "sha1" => Some(Self::Sha1),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Sha256 => "sha256",
            Self::Sha384 => "sha384",
            Self::Sha512 => "sha512",
            Self::Sha1 => "sha1",
        }
    }

    fn context(self) -> ring::digest::Context {
        ring::digest::Context::new(match self {
            Self::Sha256 => &ring::digest::SHA256,
            Self::Sha384 => &ring::digest::SHA384,
            Self::Sha512 => &ring::digest::SHA512,
            Self::Sha1 => &ring::digest::SHA1_FOR_LEGACY_USE_ONLY,
        })
    }
}

/// Where uploads are stored, with what they hold for the requests in
/// progress, by application and in all.
#[derive(Debug)]
pub struct Spool {
    root: PathBuf,
    quota: Option<u64>,
    used: AtomicU64,
    apps: Mutex<HashMap<String, Arc<AtomicU64>>>,
    next: AtomicU64,
}

impl Spool {
    /// The spool under `root`, created when missing, open to the edge's
    /// user alone, with the named files of edges that have gone removed.
    /// `quota` bounds what every application's uploads hold together.
    pub fn new(root: PathBuf, quota: Option<u64>) -> io::Result<Arc<Self>> {
        private_directory(&root)?;
        sweep(&root);

        Ok(Arc::new(Self {
            root,
            quota,
            used: AtomicU64::new(0),
            apps: Mutex::new(HashMap::new()),
            next: AtomicU64::new(0),
        }))
    }

    /// Bytes held by each application's uploads in progress, as
    /// muru_upload_spool_bytes.
    pub fn samples(&self) -> Vec<Sample> {
        let apps = self.apps.lock().expect("spool poisoned");

        apps.iter()
            .map(|(app, used)| {
                Sample::new(
                    "muru_upload_spool_bytes",
                    &[("app", app.as_str())],
                    used.load(Ordering::Relaxed) as f64,
                )
            })
            .collect()
    }

    fn usage(&self, app: &str) -> Arc<AtomicU64> {
        let mut apps = self.apps.lock().expect("spool poisoned");
        Arc::clone(apps.entry(app.to_owned()).or_default())
    }

    /// The application's own directory, so that applications never see
    /// each other's files (section 10.9).
    fn directory(&self, app: &str) -> io::Result<PathBuf> {
        let name = if app.is_empty() || app.contains('/') || app.starts_with('.') {
            "_"
        } else {
            app
        };
        let directory = self.root.join(name);
        private_directory(&directory)?;
        Ok(directory)
    }
}

fn private_directory(path: &Path) -> io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

/// Remove the named files whose edge has gone: they were left behind by
/// a crash, since an edge removes its own when each request ends.
fn sweep(root: &Path) {
    let Ok(apps) = std::fs::read_dir(root) else {
        return;
    };

    for app in apps.flatten() {
        let Ok(files) = std::fs::read_dir(app.path()) else {
            continue;
        };

        for file in files.flatten() {
            let name = file.file_name();
            let pid = name
                .to_str()
                .and_then(|name| name.split_once('-'))
                .and_then(|(pid, _)| pid.parse::<i32>().ok());

            if let Some(pid) = pid
                && !alive(pid)
            {
                let _ = std::fs::remove_file(file.path());
            }
        }
    }
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 delivers nothing; it only asks whether the
    // process exists.

    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// The bytes one stored file holds against the quotas, given back when
/// it is dropped.
struct Charge {
    spool: Arc<Spool>,
    app: Arc<AtomicU64>,
    bytes: u64,
}

impl Charge {
    /// Count `bytes` more, and say whether the quotas still hold.
    fn add(&mut self, bytes: u64, app_quota: Option<u64>) -> bool {
        let app = self.app.fetch_add(bytes, Ordering::SeqCst) + bytes;
        let node = self.spool.used.fetch_add(bytes, Ordering::SeqCst) + bytes;
        self.bytes += bytes;
        app_quota.is_none_or(|quota| app <= quota)
            && self.spool.quota.is_none_or(|quota| node <= quota)
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        self.app.fetch_sub(self.bytes, Ordering::SeqCst);
        self.spool.used.fetch_sub(self.bytes, Ordering::SeqCst);
    }
}

/// A spool file's name, where it has one, removed when dropped.
struct Name(PathBuf);

impl Drop for Name {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A file received and stored for a request, held until the request
/// ends.
pub struct Stored {
    file: File,
    name: Option<Name>,
    field: Option<String>,
    filename: String,
    content_type: String,
    size: u64,
    digests: Vec<(&'static str, String)>,
    _charge: Charge,
}

/// What the edge received for an upload request: the body the
/// application sees and the files it stored.
pub struct Received {
    /// The rewritten form, or nothing for a raw upload.
    pub body: Bytes,
    /// The files, in the order they arrived.
    pub files: Vec<Stored>,
}

impl Received {
    /// The files as the request head describes them, matched to the
    /// descriptors by order.
    pub fn refs(&self) -> Vec<pb::UploadRef> {
        self.files
            .iter()
            .enumerate()
            .map(|(index, stored)| pb::UploadRef {
                r#ref: (index + 1).to_string(),
                field: stored.field.clone().unwrap_or_default(),
                filename: stored.filename.clone(),
                content_type: stored.content_type.clone(),
                size: stored.size,
                digests: stored
                    .digests
                    .iter()
                    .map(|(name, digest)| ((*name).to_owned(), digest.clone()))
                    .collect(),
                path: stored
                    .name
                    .as_ref()
                    .map(|name| name.0.display().to_string())
                    .unwrap_or_default(),
            })
            .collect()
    }

    /// The files' descriptors, in order.
    pub fn fds(&self) -> Vec<BorrowedFd<'_>> {
        self.files
            .iter()
            .map(|stored| stored.file.as_fd())
            .collect()
    }
}

/// Why an upload was refused.
#[derive(Debug)]
pub enum Refusal {
    /// The body as received is over the route's max_body_size.
    BodyTooLarge,
    /// A file is over the route's max_file_size.
    FileTooLarge(u64),
    /// The form has more files than the route's max_files.
    TooManyFiles(usize),
    /// The form's ordinary fields are over `FIELDS_LIMIT`.
    FieldsTooLarge,
    /// The multipart body could not be parsed.
    Malformed(String),
    /// A quota is reached, or the spool could not store the file.
    SpoolFull(Option<io::Error>),
    /// The client stopped sending the body.
    Stalled,
    /// The body could not be read.
    Read(String),
}

impl Refusal {
    /// The response for the refusal; `None` for a body over
    /// max_body_size, which has its own.
    pub fn problem(&self) -> Option<Problem> {
        Some(match self {
            Self::BodyTooLarge => return None,
            Self::FileTooLarge(limit) => Problem::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "upload_too_large",
                format!("A file is larger than the route's max_file_size allows ({limit} bytes)."),
            ),
            Self::TooManyFiles(limit) => Problem::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "too_many_files",
                format!(
                    "The request carries more files than the route's max_files allows ({limit})."
                ),
            ),
            Self::FieldsTooLarge => Problem::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "body_too_large",
                format!(
                    "The form's fields other than files are larger than the edge holds ({FIELDS_LIMIT} bytes)."
                ),
            ),
            Self::Malformed(_) => Problem::new(
                StatusCode::BAD_REQUEST,
                "upload_malformed",
                "The multipart form could not be parsed.",
            ),
            Self::SpoolFull(_) => Problem::new(
                StatusCode::INSUFFICIENT_STORAGE,
                "upload_spool_full",
                "There is no room to store the upload.",
            ),
            Self::Stalled => Problem::new(
                StatusCode::REQUEST_TIMEOUT,
                "client_timeout",
                "The request body stopped arriving.",
            ),
            Self::Read(_) => Problem::new(
                StatusCode::BAD_REQUEST,
                "malformed",
                "The request body could not be read.",
            ),
        })
    }
}

/// Why the body stream stopped, carried through the multipart parser.
#[derive(Debug)]
enum StreamError {
    TooLarge,
    Stalled,
    Read(String),
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge => formatter.write_str("the body is over max_body_size"),
            Self::Stalled => formatter.write_str("the body stopped arriving"),
            Self::Read(error) => formatter.write_str(error),
        }
    }
}

impl std::error::Error for StreamError {}

impl From<StreamError> for Refusal {
    fn from(error: StreamError) -> Self {
        match error {
            StreamError::TooLarge => Self::BodyTooLarge,
            StreamError::Stalled => Self::Stalled,
            StreamError::Read(error) => Self::Read(error),
        }
    }
}

/// The body's data, bounded by `max_body_size` and by `idle` between
/// pieces.
fn body_stream(
    body: &mut Incoming,
    max_body_size: u64,
    idle: Duration,
) -> impl Stream<Item = Result<Bytes, StreamError>> + Send + '_ {
    futures::stream::unfold((body, 0u64, false), move |(body, total, done)| async move {
        if done {
            return None;
        }

        loop {
            match tokio::time::timeout(idle, body.frame()).await {
                Err(_) => return Some((Err(StreamError::Stalled), (body, total, true))),
                Ok(None) => return None,
                Ok(Some(Err(error))) => {
                    return Some((
                        Err(StreamError::Read(error.to_string())),
                        (body, total, true),
                    ));
                }
                Ok(Some(Ok(frame))) => {
                    // Trailers carry nothing to store.

                    let Ok(data) = frame.into_data() else {
                        continue;
                    };
                    let total = total + data.len() as u64;

                    if total > max_body_size {
                        return Some((Err(StreamError::TooLarge), (body, total, true)));
                    }

                    return Some((Ok(data), (body, total, false)));
                }
            }
        }
    })
}

/// Receive the body of an upload request into the spool: none when the
/// route takes multipart forms and the body is not one, which then goes
/// to the application as it is.
pub async fn receive(
    policy: &Policy,
    spool: &Arc<Spool>,
    app: &str,
    headers: &HeaderMap,
    body: &mut Incoming,
    max_body_size: u64,
    idle: Duration,
) -> Result<Option<Received>, Refusal> {
    let boundary = if policy.raw {
        None
    } else {
        match headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| multer::parse_boundary(value).ok())
        {
            Some(boundary) => Some(boundary),
            None => return Ok(None),
        }
    };

    let directory = spool
        .directory(app)
        .map_err(|error| Refusal::SpoolFull(Some(error)))?;
    let usage = spool.usage(app);
    let stream = body_stream(body, max_body_size, idle);
    let open = || Writer::create(spool, &directory, &usage, policy);

    let Some(boundary) = boundary else {
        // Raw: the whole body is the file, named by a Content-Disposition
        // the client may send.

        let mut stream = std::pin::pin!(stream);
        let mut writer = open()?;

        while let Some(chunk) = futures::StreamExt::next(&mut stream).await {
            writer.write(&chunk?).await?;
        }

        let filename = headers
            .get(http::header::CONTENT_DISPOSITION)
            .and_then(|value| value.to_str().ok())
            .and_then(disposition_filename)
            .unwrap_or_default();
        let content_type = header_text(headers, http::header::CONTENT_TYPE);
        let stored = writer.finish(None, filename, content_type).await?;

        return Ok(Some(Received {
            body: Bytes::new(),
            files: vec![stored],
        }));
    };

    let mut multipart = multer::Multipart::new(stream, boundary.clone());
    let mut rewritten = BytesMut::new();
    let mut files: Vec<Stored> = Vec::new();
    let mut fields = 0usize;

    while let Some(mut field) = multipart.next_field().await.map_err(refusal)? {
        let name = field.name().unwrap_or_default().to_owned();

        let Some(filename) = field.file_name().map(str::to_owned) else {
            // An ordinary field goes through as it came.

            let part_headers = field.headers().clone();
            let mut value = BytesMut::new();

            while let Some(chunk) = field.chunk().await.map_err(refusal)? {
                fields += chunk.len();

                if fields > FIELDS_LIMIT {
                    return Err(Refusal::FieldsTooLarge);
                }

                value.extend_from_slice(&chunk);
            }

            part(&mut rewritten, &boundary, &part_headers, &value);
            continue;
        };

        if files.len() == policy.max_files {
            return Err(Refusal::TooManyFiles(policy.max_files));
        }

        let content_type = header_text(field.headers(), http::header::CONTENT_TYPE);
        let mut writer = open()?;

        while let Some(chunk) = field.chunk().await.map_err(refusal)? {
            writer.write(&chunk).await?;
        }

        let stored = writer
            .finish(Some(name.clone()), filename, content_type)
            .await?;
        let reference = (files.len() + 1).to_string();
        let mut described = vec![
            ("name", stored.filename.clone()),
            ("content_type", stored.content_type.clone()),
            ("size", stored.size.to_string()),
        ];
        described.extend(
            stored
                .digests
                .iter()
                .map(|(algorithm, digest)| (*algorithm, digest.clone())),
        );
        described.push(("ref", reference));

        for (suffix, value) in described {
            text_part(
                &mut rewritten,
                &boundary,
                &format!("{name}.{suffix}"),
                &value,
            );
        }

        files.push(stored);
    }

    rewritten.extend_from_slice(b"--");
    rewritten.extend_from_slice(boundary.as_bytes());
    rewritten.extend_from_slice(b"--\r\n");

    Ok(Some(Received {
        body: rewritten.freeze(),
        files,
    }))
}

/// A parser failure as a refusal: the body stream's own failures keep
/// their meaning, anything else is a malformed form.
fn refusal(error: multer::Error) -> Refusal {
    if let multer::Error::StreamReadFailed(inner) = &error
        && let Some(stream) = inner.downcast_ref::<StreamError>()
    {
        return match stream {
            StreamError::TooLarge => Refusal::BodyTooLarge,
            StreamError::Stalled => Refusal::Stalled,
            StreamError::Read(text) => Refusal::Read(text.clone()),
        };
    }

    Refusal::Malformed(error.to_string())
}

fn header_text(headers: &HeaderMap, name: http::header::HeaderName) -> String {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

/// The filename parameter of a Content-Disposition value.
fn disposition_filename(value: &str) -> Option<String> {
    value.split(';').find_map(|parameter| {
        let (key, value) = parameter.trim().split_once('=')?;

        key.trim()
            .eq_ignore_ascii_case("filename")
            .then(|| value.trim().trim_matches('"').to_owned())
    })
}

/// Append an ordinary part with its own headers.
fn part(body: &mut BytesMut, boundary: &str, headers: &HeaderMap, value: &[u8]) {
    body.extend_from_slice(b"--");
    body.extend_from_slice(boundary.as_bytes());
    body.extend_from_slice(b"\r\n");

    for (name, header) in headers {
        body.extend_from_slice(name.as_str().as_bytes());
        body.extend_from_slice(b": ");
        body.extend_from_slice(header.as_bytes());
        body.extend_from_slice(b"\r\n");
    }

    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(value);
    body.extend_from_slice(b"\r\n");
}

/// Append a text field, its name escaped as browsers escape one.
fn text_part(body: &mut BytesMut, boundary: &str, name: &str, value: &str) {
    let escaped = name
        .replace('"', "%22")
        .replace('\r', "%0D")
        .replace('\n', "%0A");

    body.extend_from_slice(b"--");
    body.extend_from_slice(boundary.as_bytes());
    body.extend_from_slice(b"\r\nContent-Disposition: form-data; name=\"");
    body.extend_from_slice(escaped.as_bytes());
    body.extend_from_slice(b"\"\r\n\r\n");
    body.extend_from_slice(value.as_bytes());
    body.extend_from_slice(b"\r\n");
}

/// A spool file being written, with its digests and its charge against
/// the quotas.
struct Writer {
    file: tokio::fs::File,
    name: Option<Name>,
    size: u64,
    max: Option<u64>,
    quota: Option<u64>,
    digests: Vec<(Algorithm, ring::digest::Context)>,
    charge: Charge,
}

impl Writer {
    fn create(
        spool: &Arc<Spool>,
        directory: &Path,
        usage: &Arc<AtomicU64>,
        policy: &Policy,
    ) -> Result<Self, Refusal> {
        let counter = spool.next.fetch_add(1, Ordering::Relaxed);
        let (file, name) =
            create_file(directory, counter).map_err(|error| Refusal::SpoolFull(Some(error)))?;

        Ok(Self {
            file: tokio::fs::File::from_std(file),
            name,
            size: 0,
            max: policy.max_file_size,
            quota: policy.spool_quota,
            digests: policy
                .hash
                .iter()
                .map(|algorithm| (*algorithm, algorithm.context()))
                .collect(),
            charge: Charge {
                spool: Arc::clone(spool),
                app: Arc::clone(usage),
                bytes: 0,
            },
        })
    }

    async fn write(&mut self, chunk: &[u8]) -> Result<(), Refusal> {
        let length = chunk.len() as u64;

        if let Some(max) = self.max
            && self.size + length > max
        {
            return Err(Refusal::FileTooLarge(max));
        }

        if !self.charge.add(length, self.quota) {
            return Err(Refusal::SpoolFull(None));
        }

        for (_, context) in &mut self.digests {
            context.update(chunk);
        }

        self.file
            .write_all(chunk)
            .await
            .map_err(|error| Refusal::SpoolFull(Some(error)))?;
        self.size += length;
        Ok(())
    }

    /// The stored file, rewound so that a descriptor shared with the
    /// worker reads from the start.
    async fn finish(
        mut self,
        field: Option<String>,
        filename: String,
        content_type: String,
    ) -> Result<Stored, Refusal> {
        self.file
            .flush()
            .await
            .map_err(|error| Refusal::SpoolFull(Some(error)))?;
        let mut file = self.file.into_std().await;
        file.rewind()
            .map_err(|error| Refusal::SpoolFull(Some(error)))?;

        Ok(Stored {
            file,
            name: self.name,
            field,
            filename,
            content_type,
            size: self.size,
            digests: self
                .digests
                .into_iter()
                .map(|(algorithm, context)| (algorithm.name(), hex(context.finish().as_ref())))
                .collect(),
            _charge: self.charge,
        })
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut text, byte| {
            let _ = write!(text, "{byte:02x}");
            text
        })
}

/// A new spool file in `directory`: unnamed where the file system allows
/// it, otherwise named after this process. Either way it is readable by
/// others, since nothing reaches it but its descriptor or, for a named
/// one, a path inside a directory open to the edge alone; a worker
/// running as another user then reopens it through /proc/self/fd.
fn create_file(directory: &Path, counter: u64) -> io::Result<(File, Option<Name>)> {
    #[cfg(target_os = "linux")]
    {
        match OpenOptions::new()
            .read(true)
            .write(true)
            .mode(0o644)
            .custom_flags(libc::O_TMPFILE)
            .open(directory)
        {
            Ok(file) => return Ok((file, None)),
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(libc::EOPNOTSUPP | libc::EISDIR | libc::EINVAL)
                ) => {}
            Err(error) => return Err(error),
        }
    }

    let path = directory.join(format!("{}-{counter}", std::process::id()));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&path)?;
    Ok((file, Some(Name(path))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_filename_is_read_from_a_disposition() {
        assert_eq!(
            disposition_filename(r#"attachment; filename="report.pdf""#).as_deref(),
            Some("report.pdf")
        );
        assert_eq!(
            disposition_filename("inline; FileName=plain.txt").as_deref(),
            Some("plain.txt")
        );
        assert_eq!(disposition_filename("attachment"), None);
    }

    #[test]
    fn digests_are_lower_case_hex() {
        let mut context = Algorithm::Sha256.context();
        context.update(b"abc");
        assert_eq!(
            hex(context.finish().as_ref()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn a_text_field_name_is_escaped() {
        let mut body = BytesMut::new();
        text_part(&mut body, "b", "a\"b.name", "x");
        assert_eq!(
            &body[..],
            b"--b\r\nContent-Disposition: form-data; name=\"a%22b.name\"\r\n\r\nx\r\n"
        );
    }

    #[test]
    fn quotas_are_charged_and_given_back() {
        let directory = tempfile::tempdir().unwrap();
        let spool = Spool::new(directory.path().join("spool"), Some(10)).unwrap();
        let usage = spool.usage("shop");
        let mut charge = Charge {
            spool: Arc::clone(&spool),
            app: Arc::clone(&usage),
            bytes: 0,
        };

        assert!(charge.add(6, Some(8)));
        assert!(!charge.add(4, Some(8)), "over the application's quota");
        assert_eq!(usage.load(Ordering::SeqCst), 10);
        drop(charge);
        assert_eq!(usage.load(Ordering::SeqCst), 0);
        assert_eq!(spool.used.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn named_files_of_a_gone_edge_are_swept() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("spool");
        let app = root.join("shop");
        std::fs::create_dir_all(&app).unwrap();

        // A pid that cannot be running, and this process's own.

        let gone = app.join("999999999-1");
        let mine = app.join(format!("{}-1", std::process::id()));
        std::fs::write(&gone, b"left").unwrap();
        std::fs::write(&mine, b"live").unwrap();

        Spool::new(root, None).unwrap();
        assert!(!gone.exists());
        assert!(mine.exists());
    }
}
