use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use crate::app_server::{MAX_MESSAGE_SIZE, RpcClient};
use crate::config::Config;
use crate::{Error, Result};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_HANDSHAKE_SIZE: usize = 16 * 1024;

pub struct ControlClient {
    websocket: WebSocket,
    next_id: i64,
    timeout: Duration,
    deadline: Option<Instant>,
}

impl ControlClient {
    pub fn connect(config: &Config, socket: &Path) -> Result<Self> {
        Self::connect_with_timeout(config, socket, DEFAULT_TIMEOUT)
    }

    pub fn connect_with_timeout(
        _config: &Config,
        socket: &Path,
        timeout: Duration,
    ) -> Result<Self> {
        let deadline = Instant::now().checked_add(timeout).ok_or(Error::Timeout)?;
        let stream = connect_socket(socket, deadline)?;
        let websocket = WebSocket::upgrade(stream, deadline)?;
        let mut client = Self {
            websocket,
            next_id: 1,
            timeout,
            deadline: None,
        };
        client.initialize()?;
        Ok(client)
    }

    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    pub fn set_deadline(&mut self, deadline: Instant) {
        self.deadline = Some(deadline);
    }

    pub fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| Error::Protocol("app-server request id exhausted".into()))?;
        RpcClient::request(self, id, method, Some(params))
    }

    fn initialize(&mut self) -> Result<()> {
        RpcClient::request(
            self,
            0,
            "initialize",
            Some(json!({
                "clientInfo": {
                    "name": "cxa",
                    "title": "Codex Account Switcher",
                    "version": "1"
                },
                "capabilities": {"experimentalApi": true}
            })),
        )?;
        self.send(json!({"method": "initialized"}))
    }

    fn operation_deadline(&self) -> Result<Instant> {
        let now = Instant::now();
        let timeout_deadline = now.checked_add(self.timeout).ok_or(Error::Timeout)?;
        let deadline = self
            .deadline
            .map_or(timeout_deadline, |deadline| deadline.min(timeout_deadline));
        if deadline <= now {
            Err(Error::Timeout)
        } else {
            Ok(deadline)
        }
    }
}

impl RpcClient for ControlClient {
    fn send(&mut self, message: Value) -> Result<()> {
        let encoded =
            serde_json::to_vec(&message).map_err(|error| Error::Protocol(error.to_string()))?;
        let deadline = self.operation_deadline()?;
        self.websocket.send_frame(0x1, &encoded, deadline)
    }

    fn receive(&mut self, deadline: Instant) -> Result<Value> {
        let payload = self.websocket.receive_text(deadline)?;
        serde_json::from_slice(&payload)
            .map_err(|error| Error::Protocol(format!("invalid app-server JSON: {error}")))
    }

    fn request_timeout(&self) -> Duration {
        self.timeout
    }

    fn request_deadline(&self) -> Result<Instant> {
        self.operation_deadline()
    }

    fn handle_foreign_message(&mut self, message: &Value) -> Result<()> {
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return Ok(());
        };
        let Some(id) = message.get("id").cloned() else {
            return Ok(());
        };
        self.send(json!({
            "id": id,
            "error": {
                "code": -32601,
                "message": format!("unsupported server request: {method}")
            }
        }))
    }
}

struct WebSocket {
    stream: UnixStream,
}

impl WebSocket {
    fn upgrade(mut stream: UnixStream, deadline: Instant) -> Result<Self> {
        let mut nonce = [0_u8; 16];
        random_bytes(&mut nonce)?;
        let key = STANDARD.encode(nonce);
        let request = format!(
            "GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        write_all(&mut stream, request.as_bytes(), deadline)?;

        let mut response = Vec::new();
        while !response.ends_with(b"\r\n\r\n") {
            if response.len() == MAX_HANDSHAKE_SIZE {
                return Err(Error::Protocol("oversized WebSocket handshake".into()));
            }
            let mut byte = [0_u8; 1];
            read_exact(&mut stream, &mut byte, deadline)?;
            response.push(byte[0]);
        }
        validate_upgrade(&response, &key)?;
        Ok(Self { stream })
    }

    fn send_frame(&mut self, opcode: u8, payload: &[u8], deadline: Instant) -> Result<()> {
        if payload.len() > MAX_MESSAGE_SIZE {
            return Err(Error::Protocol("oversized app-server request".into()));
        }
        if opcode & 0x08 != 0 && payload.len() > 125 {
            return Err(Error::Protocol("oversized WebSocket control frame".into()));
        }
        let mut header = Vec::with_capacity(14);
        header.push(0x80 | opcode);
        match payload.len() {
            length @ 0..=125 => header.push(0x80 | length as u8),
            length @ 126..=65535 => {
                header.push(0x80 | 126);
                header.extend_from_slice(&(length as u16).to_be_bytes());
            }
            length => {
                header.push(0x80 | 127);
                header.extend_from_slice(&(length as u64).to_be_bytes());
            }
        }
        let mut mask = [0_u8; 4];
        random_bytes(&mut mask)?;
        header.extend_from_slice(&mask);
        write_all(&mut self.stream, &header, deadline)?;
        let masked: Vec<u8> = payload
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % 4])
            .collect();
        write_all(&mut self.stream, &masked, deadline)
    }

    fn receive_text(&mut self, deadline: Instant) -> Result<Vec<u8>> {
        let mut message = Vec::new();
        let mut fragmented = false;
        loop {
            let frame = self.read_frame(deadline)?;
            match frame.opcode {
                0x0 if fragmented => {
                    append_payload(&mut message, &frame.payload)?;
                    if frame.final_frame {
                        return valid_text(message);
                    }
                }
                0x0 => {
                    return Err(Error::Protocol(
                        "unexpected WebSocket continuation frame".into(),
                    ));
                }
                0x1 if fragmented => {
                    return Err(Error::Protocol(
                        "new WebSocket data frame during fragmented message".into(),
                    ));
                }
                0x1 => {
                    append_payload(&mut message, &frame.payload)?;
                    if frame.final_frame {
                        return valid_text(message);
                    }
                    fragmented = true;
                }
                0x2 => {
                    return Err(Error::Protocol(
                        "binary app-server WebSocket message is unsupported".into(),
                    ));
                }
                0x8 => {
                    validate_close_payload(&frame.payload)?;
                    let _ = self.send_frame(0x8, &frame.payload, deadline);
                    return Err(Error::Protocol(
                        "app server closed the control socket".into(),
                    ));
                }
                0x9 => self.send_frame(0xA, &frame.payload, deadline)?,
                0xA => {}
                opcode => {
                    return Err(Error::Protocol(format!(
                        "unsupported WebSocket opcode {opcode:#x}"
                    )));
                }
            }
        }
    }

    fn read_frame(&mut self, deadline: Instant) -> Result<Frame> {
        let mut prefix = [0_u8; 2];
        read_exact(&mut self.stream, &mut prefix, deadline)?;
        if prefix[0] & 0x70 != 0 {
            return Err(Error::Protocol("WebSocket frame uses reserved bits".into()));
        }
        if prefix[1] & 0x80 != 0 {
            return Err(Error::Protocol(
                "app server sent a masked WebSocket frame".into(),
            ));
        }
        let final_frame = prefix[0] & 0x80 != 0;
        let opcode = prefix[0] & 0x0f;
        let control = opcode & 0x08 != 0;
        let length = match prefix[1] & 0x7f {
            length @ 0..=125 => length as u64,
            126 => {
                let mut bytes = [0_u8; 2];
                read_exact(&mut self.stream, &mut bytes, deadline)?;
                let length = u16::from_be_bytes(bytes) as u64;
                if length < 126 {
                    return Err(Error::Protocol(
                        "non-canonical WebSocket frame length".into(),
                    ));
                }
                length
            }
            127 => {
                let mut bytes = [0_u8; 8];
                read_exact(&mut self.stream, &mut bytes, deadline)?;
                let length = u64::from_be_bytes(bytes);
                if length & (1 << 63) != 0 {
                    return Err(Error::Protocol("invalid WebSocket frame length".into()));
                }
                if length <= u16::MAX as u64 {
                    return Err(Error::Protocol(
                        "non-canonical WebSocket frame length".into(),
                    ));
                }
                length
            }
            _ => unreachable!(),
        };
        if control && (!final_frame || length > 125) {
            return Err(Error::Protocol("invalid WebSocket control frame".into()));
        }
        let length = usize::try_from(length)
            .map_err(|_| Error::Protocol("oversized app-server WebSocket frame".into()))?;
        if length > MAX_MESSAGE_SIZE {
            return Err(Error::Protocol(
                "oversized app-server WebSocket frame".into(),
            ));
        }
        let mut payload = vec![0_u8; length];
        read_exact(&mut self.stream, &mut payload, deadline)?;
        Ok(Frame {
            final_frame,
            opcode,
            payload,
        })
    }
}

struct Frame {
    final_frame: bool,
    opcode: u8,
    payload: Vec<u8>,
}

fn append_payload(message: &mut Vec<u8>, payload: &[u8]) -> Result<()> {
    if message.len().saturating_add(payload.len()) > MAX_MESSAGE_SIZE {
        return Err(Error::Protocol(
            "oversized app-server WebSocket message".into(),
        ));
    }
    message.extend_from_slice(payload);
    Ok(())
}

fn valid_text(message: Vec<u8>) -> Result<Vec<u8>> {
    std::str::from_utf8(&message)
        .map_err(|error| Error::Protocol(format!("invalid UTF-8 WebSocket message: {error}")))?;
    Ok(message)
}

fn validate_close_payload(payload: &[u8]) -> Result<()> {
    if payload.is_empty() {
        return Ok(());
    }
    if payload.len() == 1 {
        return Err(Error::Protocol("invalid WebSocket close payload".into()));
    }
    let code = u16::from_be_bytes([payload[0], payload[1]]);
    if !(1000..5000).contains(&code) || matches!(code, 1004 | 1005 | 1006 | 1015) {
        return Err(Error::Protocol(format!(
            "invalid WebSocket close code {code}"
        )));
    }
    std::str::from_utf8(&payload[2..])
        .map_err(|error| Error::Protocol(format!("invalid WebSocket close reason: {error}")))?;
    Ok(())
}

fn read_exact(stream: &mut UnixStream, buffer: &mut [u8], deadline: Instant) -> Result<()> {
    let mut offset = 0;
    while offset < buffer.len() {
        set_read_deadline(stream, deadline)?;
        match stream.read(&mut buffer[offset..]) {
            Ok(0) => {
                return Err(Error::Protocol(
                    "app server closed the control socket".into(),
                ));
            }
            Ok(length) => offset += length,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(socket_error(error)),
        }
    }
    Ok(())
}

fn write_all(stream: &mut UnixStream, buffer: &[u8], deadline: Instant) -> Result<()> {
    let mut offset = 0;
    while offset < buffer.len() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Error::Timeout);
        }
        stream
            .set_write_timeout(Some(remaining))
            .map_err(|error| Error::io("app-server control socket", error))?;
        match stream.write(&buffer[offset..]) {
            Ok(0) => {
                return Err(Error::Protocol(
                    "app server closed the control socket".into(),
                ));
            }
            Ok(length) => offset += length,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(socket_error(error)),
        }
    }
    Ok(())
}

fn set_read_deadline(stream: &UnixStream, deadline: Instant) -> Result<()> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(Error::Timeout);
    }
    stream
        .set_read_timeout(Some(remaining))
        .map_err(|error| Error::io("app-server control socket", error))
}

fn socket_error(error: std::io::Error) -> Error {
    if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) {
        Error::Timeout
    } else if error.kind() == ErrorKind::UnexpectedEof {
        Error::Protocol("app server closed the control socket".into())
    } else {
        Error::io("app-server control socket", error)
    }
}

fn connect_socket(path: &Path, deadline: Instant) -> Result<UnixStream> {
    let bytes = path.as_os_str().as_bytes();
    let path_offset = std::mem::offset_of!(libc::sockaddr_un, sun_path);
    let mut address = unsafe { std::mem::zeroed::<libc::sockaddr_un>() };
    if bytes.is_empty() || bytes.len() >= address.sun_path.len() {
        return Err(Error::Protocol(format!(
            "app-server socket path is too long: {}",
            path.display()
        )));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (target, byte) in address.sun_path.iter_mut().zip(bytes) {
        *target = *byte as libc::c_char;
    }
    let address_length = (path_offset + bytes.len() + 1) as libc::socklen_t;
    #[cfg(any(
        target_os = "aix",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "haiku",
        target_os = "macos",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    {
        address.sun_len = address_length as u8;
    }

    let raw_fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if raw_fd == -1 {
        return Err(Error::io(path, std::io::Error::last_os_error()));
    }
    let owned_fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    set_fd_flags(raw_fd, path)?;
    loop {
        let connected = unsafe {
            libc::connect(
                raw_fd,
                (&raw const address).cast::<libc::sockaddr>(),
                address_length,
            )
        };
        if connected == 0 {
            break;
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EISCONN) => break,
            Some(libc::EINPROGRESS) | Some(libc::EALREADY) => {
                wait_for_connect(raw_fd, deadline, path)?;
                break;
            }
            Some(libc::EAGAIN) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(Error::Timeout);
                }
                std::thread::sleep(remaining.min(Duration::from_millis(5)));
            }
            _ => return Err(Error::io(path, error)),
        }
    }
    let stream = UnixStream::from(owned_fd);
    stream
        .set_nonblocking(false)
        .map_err(|error| Error::io(path, error))?;
    Ok(stream)
}

fn set_fd_flags(raw_fd: libc::c_int, path: &Path) -> Result<()> {
    for (get, set, flag) in [
        (libc::F_GETFD, libc::F_SETFD, libc::FD_CLOEXEC),
        (libc::F_GETFL, libc::F_SETFL, libc::O_NONBLOCK),
    ] {
        let current = unsafe { libc::fcntl(raw_fd, get) };
        if current == -1 || unsafe { libc::fcntl(raw_fd, set, current | flag) } == -1 {
            return Err(Error::io(path, std::io::Error::last_os_error()));
        }
    }
    Ok(())
}

fn wait_for_connect(raw_fd: libc::c_int, deadline: Instant, path: &Path) -> Result<()> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Error::Timeout);
        }
        let timeout_ms = remaining.as_millis().clamp(1, libc::c_int::MAX as u128) as libc::c_int;
        let mut descriptor = libc::pollfd {
            fd: raw_fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if result == 0 {
            return Err(Error::Timeout);
        }
        if result == -1 {
            let error = std::io::Error::last_os_error();
            if error.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err(Error::io(path, error));
        }
        let mut socket_error = 0;
        let mut length = std::mem::size_of_val(&socket_error) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                raw_fd,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&raw mut socket_error).cast(),
                &mut length,
            )
        } == -1
        {
            return Err(Error::io(path, std::io::Error::last_os_error()));
        }
        return if socket_error == 0 {
            Ok(())
        } else {
            Err(Error::io(
                path,
                std::io::Error::from_raw_os_error(socket_error),
            ))
        };
    }
}

fn random_bytes(buffer: &mut [u8]) -> Result<()> {
    File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(buffer))
        .map_err(|error| Error::io("/dev/urandom", error))
}

fn validate_upgrade(response: &[u8], key: &str) -> Result<()> {
    let response = std::str::from_utf8(response)
        .map_err(|_| Error::Protocol("WebSocket handshake is not valid HTTP".into()))?;
    let mut lines = response.split("\r\n");
    let status = lines.next().unwrap_or_default();
    let mut status_parts = status.split_ascii_whitespace();
    if !status_parts
        .next()
        .is_some_and(|version| version.starts_with("HTTP/1."))
        || status_parts.next() != Some("101")
    {
        return Err(Error::Protocol(format!(
            "WebSocket upgrade failed: {status}"
        )));
    }
    let mut upgrade = false;
    let mut connection = false;
    let mut accept = None;
    for line in lines.take_while(|line| !line.is_empty()) {
        let Some((name, value)) = line.split_once(':') else {
            return Err(Error::Protocol(
                "malformed WebSocket handshake header".into(),
            ));
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "upgrade" => upgrade |= header_has_token(value, "websocket"),
            "connection" => connection |= header_has_token(value, "upgrade"),
            "sec-websocket-accept" => accept = Some(value),
            _ => {}
        }
    }
    let expected = websocket_accept(key);
    if !upgrade || !connection || accept != Some(expected.as_str()) {
        return Err(Error::Protocol(
            "invalid WebSocket upgrade response headers".into(),
        ));
    }
    Ok(())
}

fn header_has_token(value: &str, expected: &str) -> bool {
    value
        .split(',')
        .any(|token| token.trim().eq_ignore_ascii_case(expected))
}

fn websocket_accept(key: &str) -> String {
    // RFC 6455 requires SHA-1 only for this public handshake challenge. It is
    // not used for authentication, signatures, or storage.
    let mut challenge = Vec::with_capacity(key.len() + 36);
    challenge.extend_from_slice(key.as_bytes());
    challenge.extend_from_slice(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    STANDARD.encode(sha1(&challenge))
}

fn sha1(input: &[u8]) -> [u8; 20] {
    let bit_length = (input.len() as u64).wrapping_mul(8);
    let mut padded = input.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_length.to_be_bytes());

    let mut state = [
        0x6745_2301_u32,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    for chunk in padded.chunks_exact(64) {
        let mut words = [0_u32; 80];
        for (index, bytes) in chunk.chunks_exact(4).enumerate() {
            words[index] = u32::from_be_bytes(bytes.try_into().unwrap());
        }
        for index in 16..80 {
            words[index] =
                (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16])
                    .rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = state;
        for (index, word) in words.iter().enumerate() {
            let (function, constant) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let next = a
                .rotate_left(5)
                .wrapping_add(function)
                .wrapping_add(e)
                .wrapping_add(constant)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = next;
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
    }
    let mut digest = [0_u8; 20];
    for (output, word) in digest.chunks_exact_mut(4).zip(state) {
        output.copy_from_slice(&word.to_be_bytes());
    }
    digest
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command};
    use std::thread;

    use super::*;

    const PYTHON_SERVER: &str = r#"
import base64, hashlib, json, socket, struct, sys, time

socket_path, record_path, ready_path, mode = sys.argv[1:]
server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
server.bind(socket_path)
server.listen(1)
open(ready_path, 'w').close()
connection, _ = server.accept()

def wait_for_client_close():
    connection.settimeout(1)
    try:
        while connection.recv(1024):
            pass
    except (ConnectionResetError, socket.timeout):
        pass

request = b''
while not request.endswith(b'\r\n\r\n'):
    request += connection.recv(1)
headers = {}
lines = request.decode('ascii').split('\r\n')
for line in lines[1:]:
    if ':' in line:
        name, value = line.split(':', 1)
        headers[name.lower()] = value.strip()
key = headers['sec-websocket-key']
accept = base64.b64encode(hashlib.sha1(
    (key + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode('ascii')
).digest()).decode('ascii')
if mode == 'bad_accept':
    accept = 'invalid'
upgrade = (
    'HTTP/1.1 101 Switching Protocols\r\n'
    'Upgrade: websocket\r\n'
    'Connection: keep-alive, Upgrade\r\n'
    f'Sec-WebSocket-Accept: {accept}\r\n\r\n'
).encode('ascii')
for byte in upgrade:
    connection.sendall(bytes([byte]))
if mode == 'bad_accept':
    wait_for_client_close()
    sys.exit(0)

def exact(length):
    data = b''
    while len(data) < length:
        chunk = connection.recv(length - len(data))
        if not chunk:
            raise EOFError()
        data += chunk
    return data

def receive_frame():
    first, second = exact(2)
    length = second & 0x7f
    if length == 126:
        length = struct.unpack('!H', exact(2))[0]
    elif length == 127:
        length = struct.unpack('!Q', exact(8))[0]
    assert second & 0x80, 'client frame was not masked'
    mask = exact(4)
    payload = exact(length)
    payload = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
    return first & 0x0f, payload

def receive_data():
    while True:
        opcode, payload = receive_frame()
        if opcode in (1, 8):
            return opcode, payload

def frame(opcode, payload, final=True):
    prefix = bytes([(0x80 if final else 0) | opcode])
    length = len(payload)
    if length <= 125:
        prefix += bytes([length])
    elif length <= 65535:
        prefix += bytes([126]) + struct.pack('!H', length)
    else:
        prefix += bytes([127]) + struct.pack('!Q', length)
    return prefix + payload

def send_bytes(data):
    for index in range(0, len(data), 3):
        connection.sendall(data[index:index + 3])

def send_json(value):
    send_bytes(frame(1, json.dumps(value, separators=(',', ':')).encode('utf-8')))

messages = []
opcode, payload = receive_data()
assert opcode == 1
messages.append(json.loads(payload))
response = json.dumps({'id': 0, 'result': {}}, separators=(',', ':')).encode('utf-8')
split = len(response) // 2
send_bytes(frame(1, response[:split], False))
send_bytes(frame(9, b'health'))
send_bytes(frame(0, response[split:]))
opcode, pong = receive_frame()
assert opcode == 10 and pong == b'health'
opcode, payload = receive_data()
messages.append(json.loads(payload))
opcode, payload = receive_data()
messages.append(json.loads(payload))

if mode == 'timeout':
    time.sleep(0.3)
elif mode == 'trickle':
    response = frame(1, json.dumps({'id': 1, 'result': {'ok': True}}).encode('utf-8'))
    try:
        for byte in response:
            connection.sendall(bytes([byte]))
            time.sleep(0.01)
    except BrokenPipeError:
        pass
elif mode == 'oversized':
    connection.sendall(bytes([0x81, 127]) + struct.pack('!Q', 4 * 1024 * 1024 + 1))
    wait_for_client_close()
else:
    send_json({'method': 'account/updated', 'params': {}})
    send_json({'id': 'approval-1', 'method': 'item/commandExecution/requestApproval', 'params': {}})
    opcode, payload = receive_data()
    messages.append(json.loads(payload))
    send_json({'id': 1, 'result': {'ok': True, 'padding': 'x' * 200}})

with open(record_path, 'w') as output:
    json.dump({
        'request_line': lines[0],
        'headers': headers,
        'messages': messages,
        'pong': pong.decode('ascii'),
    }, output)
"#;

    struct PythonServer {
        child: Child,
        _directory: tempfile::TempDir,
        socket: PathBuf,
        record: PathBuf,
    }

    impl PythonServer {
        fn start(mode: &str) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let script = directory.path().join("server.py");
            let socket = directory.path().join("control.sock");
            let record = directory.path().join("record.json");
            let ready = directory.path().join("ready");
            fs::write(&script, PYTHON_SERVER).unwrap();
            let child = Command::new("python3")
                .args([&script, &socket, &record, &ready, Path::new(mode)])
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            while !ready.exists() {
                assert!(Instant::now() < deadline, "Python server did not bind");
                thread::sleep(Duration::from_millis(5));
            }
            Self {
                child,
                _directory: directory,
                socket,
                record,
            }
        }

        fn finish(mut self) -> Value {
            let status = self.child.wait().unwrap();
            assert!(status.success());
            fs::read(&self.record)
                .map(|record| serde_json::from_slice(&record).unwrap())
                .unwrap_or(Value::Null)
        }
    }

    impl Drop for PythonServer {
        fn drop(&mut self) {
            if self.child.try_wait().ok().flatten().is_none() {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }

    fn config() -> Config {
        Config {
            codex_home: PathBuf::from("/unused/codex-home"),
            codex_binary: None,
            account_store: PathBuf::from("/unused/accounts"),
            switch_lock: PathBuf::from("/unused/accounts/switch.lock"),
            session_auth: PathBuf::from("/unused/codex-home/auth.json"),
            usage_ttl_seconds: 120,
            skip_usage_refresh: false,
        }
    }

    #[test]
    fn sha1_matches_rfc_websocket_example() {
        assert_eq!(
            websocket_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn exchanges_masked_json_across_fragmented_frames_and_rejects_server_requests() {
        let server = PythonServer::start("exchange");
        let mut client = ControlClient::connect(&config(), &server.socket).unwrap();
        let result = client
            .request("account/rateLimits/read", json!({}))
            .unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(result["padding"].as_str().unwrap().len(), 200);
        drop(client);

        let record = server.finish();
        assert_eq!(record["request_line"], "GET / HTTP/1.1");
        assert_eq!(record["headers"]["upgrade"], "websocket");
        assert_eq!(record["headers"]["sec-websocket-version"], "13");
        assert_eq!(record["pong"], "health");
        assert_eq!(record["messages"][0]["method"], "initialize");
        assert_eq!(
            record["messages"][0]["params"]["capabilities"]["experimentalApi"],
            true
        );
        assert_eq!(record["messages"][1]["method"], "initialized");
        assert_eq!(record["messages"][2]["method"], "account/rateLimits/read");
        assert_eq!(record["messages"][3]["id"], "approval-1");
        assert_eq!(record["messages"][3]["error"]["code"], -32601);
    }

    #[test]
    fn rejects_an_invalid_upgrade_accept() {
        let server = PythonServer::start("bad_accept");
        let error = ControlClient::connect(&config(), &server.socket)
            .err()
            .expect("invalid accept should fail");
        assert!(
            matches!(&error, Error::Protocol(message) if message.contains("upgrade response")),
            "unexpected error: {error:?}"
        );
        server.finish();
    }

    #[test]
    fn enforces_response_size_before_allocating_payload() {
        let server = PythonServer::start("oversized");
        let mut client = ControlClient::connect(&config(), &server.socket).unwrap();
        let error = client.request("account/read", json!({})).unwrap_err();
        assert!(
            matches!(&error, Error::Protocol(message) if message.contains("oversized")),
            "unexpected error: {error:?}"
        );
        drop(client);
        server.finish();
    }

    #[test]
    fn applies_request_timeout_to_socket_reads() {
        let server = PythonServer::start("timeout");
        let mut client = ControlClient::connect(&config(), &server.socket).unwrap();
        client.set_timeout(Duration::from_millis(25));
        assert!(matches!(
            client.request("account/read", json!({})),
            Err(Error::Timeout)
        ));
        drop(client);
        server.finish();
    }

    #[test]
    fn absolute_deadline_stops_a_trickling_frame() {
        let server = PythonServer::start("trickle");
        let mut client = ControlClient::connect(&config(), &server.socket).unwrap();
        client.set_timeout(Duration::from_millis(25));
        let started = Instant::now();
        assert!(matches!(
            client.request("account/read", json!({})),
            Err(Error::Timeout)
        ));
        assert!(started.elapsed() < Duration::from_millis(150));
        drop(client);
        server.finish();
    }
}
