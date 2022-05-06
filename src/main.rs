#![warn(rust_2018_idioms)]

use std::io;
use std::io::Write;

use anyhow::bail;
use rand::{RngCore, SeedableRng};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
};

const LISTEN: &str = "127.0.0.1:59416";
const _MAX_CONNECTIONS: usize = 1024;
const REQUEST_HEADERS_SIZE: usize = 64;
const REQUEST_BUFFER_SIZE: usize = 4096;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;

const KB: u64 = 1000;
const MB: u64 = 1000 * KB;
const GB: u64 = 1000 * MB;

const DEFAULT_RESPONSE_SIZE: u64 = 100 * MB;

fn find_subsequence<T>(haystack: &[T], needle: &[T]) -> Option<usize>
where
    for<'a> &'a [T]: PartialEq,
{
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_http_path(mut path: &str) -> anyhow::Result<Option<u64>> {
    if path.get(0..1) != Some("/") {
        bail!("path '{}' should begin with '/'", path);
    }
    path = &path[1..];

    // Parse number
    if path.is_empty() {
        return Ok(None);
    }
    let (size, n) = match path[0..]
        .char_indices()
        .take_while(|(_, c)| (c.is_ascii_digit()))
        .last()
    {
        Some((idx, _)) => {
            let n = idx + 1;
            (path[..n].parse::<u64>()?, n)
        }
        None => bail!("invalid digit in path '{}' ('{}')", path, &path[0..1]),
    };
    path = &path[n..];

    // Parse unit
    let units = [
        ("K", KIB),
        ("M", MIB),
        ("G", GIB),
        ("KB", KB),
        ("MB", MB),
        ("GB", GB),
        ("KiB", KIB),
        ("MiB", MIB),
        ("GiB", GIB),
    ];
    let mut unit_parsed_opt = None;
    let mut unit_len = 0;
    for (u, unit) in units {
        unit_len = u.len();
        if let Some(s) = path.get(0..unit_len) {
            let str_eq_unit = s.eq_ignore_ascii_case(u);
            let no_text_after = path
                .get(unit_len..unit_len + 1)
                .map(|s| s.chars().all(|c| !c.is_alphabetic()))
                .unwrap_or(true);
            if str_eq_unit && no_text_after {
                unit_parsed_opt = Some(unit);
                break;
            }
        }
    }
    let unit_parsed = match unit_parsed_opt {
        Some(u) => u,
        None => bail!("could not parse data size unit at '{}'", path),
    };
    let path = &path[unit_len..];

    // Parse some optional extension
    if !path.is_empty() {
        if &path[..1] == "." && path[1..].chars().all(char::is_alphanumeric) {
            // There is some valid extension here, and we are not interested
            // in it.
        } else {
            bail!("invalid characters after unit: '{}'", path);
        }
    }

    Ok(Some(size * unit_parsed))
}

async fn plain_response_extra_headers<F>(
    stream: &mut TcpStream,
    status: &str,
    body: &str,
    mut extra_headers: F,
) -> anyhow::Result<()>
where
    F: FnMut(&mut dyn io::Write) -> io::Result<()>,
{
    let mut buf = [0u8; 4096];
    let mut cursor = io::Cursor::new(&mut buf[..]);
    write!(cursor, "HTTP/1.1 {}\r\n", status)?;
    write!(cursor, "Content-Type: text/plain\r\n")?;
    write!(cursor, "Content-Length: {}\r\n", body.len())?;
    write!(cursor, "\r\n")?; // End of headers
    write!(cursor, "{}", body)?;
    extra_headers(&mut cursor)?;

    let position = cursor
        .position()
        .try_into()
        .expect("value should be smaller than 4096");
    let buf = cursor.into_inner();
    stream.write_all(&buf[0..position]).await?;
    Ok(())
}

async fn plain_response(stream: &mut TcpStream, status: &str, body: &str) -> anyhow::Result<()> {
    plain_response_extra_headers(stream, status, body, |_| Ok(())).await
}

async fn process_socket(mut stream: TcpStream) -> anyhow::Result<()> {
    // Read the request from the client
    let mut data = [0; REQUEST_BUFFER_SIZE];
    let data_capacity = data.len();
    let mut data_read = 0;

    loop {
        stream.readable().await?;
        match stream.try_read(&mut data[data_read..]) {
            Ok(n) => data_read += n,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
            Err(err) => return Err(err.into()),
        }

        if find_subsequence(&data[..data_read], b"\r\n\r\n").is_some() {
            // We read a complete HTTP request
            break;
        }

        if data_read == data_capacity {
            plain_response(&mut stream, "400 Bad Request", "request too big\n").await?;
            return Ok(());
        }
    }

    // Parse the request
    let mut headers = [httparse::EMPTY_HEADER; REQUEST_HEADERS_SIZE];
    let mut request = httparse::Request::new(&mut headers);
    request.parse(&data[..data_read])?;

    // Check that we are dealing with a GET or HEAD request
    let method_is = |method| match request.method {
        Some(s) => s.eq_ignore_ascii_case(method),
        None => false,
    };
    if !method_is("GET") && !method_is("HEAD") {
        // Not a GET request, return 405 Method Not Allowed
        let body = format!(
            "method ('{}') not allowed, use 'GET' or 'HEAD' method\n",
            request.method.unwrap_or("")
        );
        plain_response_extra_headers(&mut stream, "405 Method Not Allowed", &body, |w| {
            write!(w, "Allow: GET, HEAD\r\n")
        })
        .await?;
        return Ok(());
    }

    // Construct the response
    let path = match request.path {
        Some(path) => path,
        None => bail!("no path in HTTP request"),
    };
    let response_size = match parse_http_path(path) {
        Ok(o) => usize::try_from(o.unwrap_or(DEFAULT_RESPONSE_SIZE))?,
        Err(err) => {
            let body = format!("file not found: {}\n", err);
            plain_response(&mut stream, "404 Not Found", &body).await?;
            return Ok(());
        }
    };

    // Return regular OK response
    let mut buf = [0u8; 4096];
    let mut cursor = io::Cursor::new(&mut buf[..]);
    write!(cursor, "HTTP/1.1 200 OK\r\n")?;
    write!(cursor, "Content-Type: text/plain\r\n")?;
    write!(cursor, "Content-Length: {}\r\n", response_size)?;
    write!(cursor, "\r\n")?; // End of headers
    let header_length = cursor.position().try_into()?;
    let headerstr = &buf[0..header_length];
    stream.write_all(headerstr).await?;

    if method_is("HEAD") {
        return Ok(());
    }

    // Start dumping random data to the client
    let mut rng = rand::rngs::SmallRng::from_entropy();
    let mut bytes_written = 0;
    let mut buf = [0; 128 * 1024];
    let mut rerandomize = buf.len();
    while bytes_written < response_size {
        rng.fill_bytes(&mut buf[..rerandomize]);
        let bytes_left = response_size - bytes_written;
        let chunk_size = if bytes_left < buf.len() {
            bytes_left
        } else {
            buf.len()
        };
        let n = stream.write(&buf[buf.len() - chunk_size..]).await?;
        bytes_written += n;
        rerandomize = n;
    }

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();
    let listener = TcpListener::bind(LISTEN).await?;
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(async {
            let peer_addr = stream.peer_addr();
            if let Err(err) = process_socket(stream).await {
                log::error!(
                    "error: [{}] {:#}",
                    peer_addr.map_or_else(|_| "unknown".to_string(), |a| a.to_string()),
                    err
                );
            }
        });
    }
}
