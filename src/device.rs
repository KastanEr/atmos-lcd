//! Cooler Master MasterLiquid Atmos II LCD protocol. See PROTOCOL.md.

use std::io;
use std::sync::mpsc::Receiver;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::hid::{self, HidDevice};

const REPORT_LEN: usize = 1024;
const CMD_REPORT: u8 = 0x1E;
const DATA_REPORT: u8 = 0x1F;
const RESP_REPORT: u8 = 0x20;
const FRAME_MARK: u8 = 0x5A;
const CHUNK_LEN: usize = 1000;
/// Data chunk flag for realtime frames.
const REALTIME_FLAG: u8 = 1;
/// Data chunk flag for files announced by a `transport` command.
const FILE_FLAG: u8 = 0;

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
/// Storage commands can wait while the device writes a previous file to flash.
const STORAGE_TIMEOUT: Duration = Duration::from_secs(30);

/// How often realtime mode must be refreshed; the device's keepAliveTimeout is 5 s.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(4);

pub struct Atmos {
    hid: HidDevice,
    responses: Receiver<Vec<u8>>,
    session: u8,
}

impl Atmos {
    pub fn open() -> io::Result<Atmos> {
        let path = hid::device_paths()
            .into_iter()
            .find(|p| {
                let p = p.to_ascii_lowercase();
                p.contains("vid_2516&pid_0222") && p.contains("mi_00")
            })
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Atmos II LCD (2516:0222) not found"))?;
        let (hid, responses) = HidDevice::open(&path, REPORT_LEN)?;
        // Without this the firmware ignores every command.
        hid.set_output_report(&[0x02, 0x04])?;
        Ok(Atmos { hid, responses, session: 0 })
    }

    /// Sends a command and waits for its response text.
    pub fn request(&self, method: &str, name: &str, json: &str, seq: u32) -> io::Result<String> {
        self.request_within(method, name, json, seq, RESPONSE_TIMEOUT)
    }

    fn request_within(&self, method: &str, name: &str, json: &str, seq: u32, timeout: Duration) -> io::Result<String> {
        let date = unix_time().as_secs();
        let text = format!(
            "{method} {name} 1\r\nSeqNumber={seq}\r\nDate={date}\r\nContentType=json\r\nContentLength={}\r\n\r\n{json}",
            json.len()
        );
        self.exchange(&text, timeout)
    }

    /// Current device parameters as JSON (brightness, degree, keepAliveTimeout, ...).
    pub fn params(&self) -> io::Result<String> {
        let reply = self.exchange("POST param 1\r\n", RESPONSE_TIMEOUT)?;
        Ok(body(&reply).to_string())
    }

    pub fn set_brightness(&self, percent: u8) -> io::Result<String> {
        self.request("POST", "brightness", &format!("{{\"value\":{}}}", percent.min(100)), 44)
    }

    pub fn set_rotation(&self, degree: u16) -> io::Result<String> {
        self.request("POST", "rotate", &format!("{{\"degree\":{degree}}}"), 45)
    }

    /// Enters (or keeps) realtime mode. Call at least every `KEEPALIVE_INTERVAL` while streaming.
    pub fn keep_realtime_alive(&self) -> io::Result<()> {
        let ms = unix_time().as_millis();
        self.request("STATE", "timestamp", &format!("{{\"timestamp\":{ms}}}"), 42)?;
        self.request("POST", "realtimeDisplay", "{\"enable\":true}", 42)?;
        Ok(())
    }

    /// `false` leaves realtime mode so the device plays its stored media right away.
    pub fn set_realtime(&self, enable: bool) -> io::Result<String> {
        self.request("POST", "realtimeDisplay", &format!("{{\"enable\":{enable}}}"), 42)
    }

    /// Removes a stored file, e.g. `suspend_0.jpg` of kind `suspend`.
    pub fn delete_media(&self, file_name: &str, kind: &str) -> io::Result<String> {
        self.request("POST", "delMedia", &format!("{{\"fileName\":\"{file_name}\",\"type\":\"{kind}\"}}"), 42)
    }

    /// Stores a file on the device: `transport`, the data, then `transported`.
    /// Kinds seen: `suspend` (`suspend_<0-4>.jpg|.mp4`, played when no host streams)
    /// and `logo` (`logo_0.mp4`).
    pub fn upload(&mut self, file_name: &str, kind: &str, data: &[u8]) -> io::Result<()> {
        let announce = format!("{{\"fileName\":\"{file_name}\",\"fileSize\":{},\"type\":\"{kind}\"}}", data.len());
        expect_success(&self.request_within("POST", "transport", &announce, 42, STORAGE_TIMEOUT)?)?;
        // Pauses as in MasterCTRL; the device prepares storage after `transport`.
        std::thread::sleep(Duration::from_millis(300));
        self.send_data(data, FILE_FLAG)?;
        std::thread::sleep(Duration::from_millis(500));
        // The device acknowledges the data with "AckNumber=0"; it carries no status.
        let _ = self.responses.recv_timeout(Duration::from_secs(5));
        // MasterCTRL always sends this placeholder checksum.
        let done = format!("{{\"fileName\":\"{file_name}\",\"md5\":1111111}}");
        expect_success(&self.request_within("POST", "transported", &done, 42, STORAGE_TIMEOUT)?)
    }

    /// Which of the stored-media slots hold a file (`suspend_<i>`).
    pub fn suspend_slots(&self) -> io::Result<Vec<bool>> {
        let params = self.params()?;
        let list = params
            .split_once("\"suspendMediaActive\":[")
            .and_then(|(_, rest)| rest.split_once(']'))
            .map(|(list, _)| list)
            .ok_or_else(|| io::Error::other("no suspendMediaActive in params"))?;
        Ok(list.split(',').map(|v| v.trim() == "1").collect())
    }

    /// Streams one 480x480 baseline JPEG in realtime mode.
    pub fn send_frame(&mut self, jpeg: &[u8]) -> io::Result<()> {
        self.send_data(jpeg, REALTIME_FLAG)
    }

    fn send_data(&mut self, data: &[u8], flag: u8) -> io::Result<()> {
        self.session = (self.session + 1) % 60;
        let total = data.len().div_ceil(CHUNK_LEN);
        // Build every report first so they can be queued back to back.
        let mut reports = vec![0u8; total * REPORT_LEN];
        for ((i, chunk), r) in data.chunks(CHUNK_LEN).enumerate().zip(reports.chunks_mut(REPORT_LEN)) {
            r[0] = DATA_REPORT;
            r[1] = 0x5C;
            r[2..4].copy_from_slice(&((chunk.len() + 20) as u16).to_be_bytes());
            r[4] = self.session;
            r[5..7].copy_from_slice(&(total as u16).to_be_bytes());
            r[7..9].copy_from_slice(&(i as u16).to_be_bytes());
            r[9] = flag;
            r[24..24 + chunk.len()].copy_from_slice(chunk);
        }
        self.hid.write_all(&reports)
    }

    fn exchange(&self, text: &str, timeout: Duration) -> io::Result<String> {
        while self.responses.try_recv().is_ok() {}
        self.hid.write(&encode_command(text.as_bytes()))?;
        let report = self
            .responses
            .recv_timeout(timeout)
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no response from device"))?;
        decode_response(&report)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed response"))
    }
}

/// `[1E][5A][len][text][checksum][5A]`, len = text + 5, checksum = sum(len..text).
fn encode_command(text: &[u8]) -> [u8; REPORT_LEN] {
    assert!(text.len() <= REPORT_LEN - 9, "command too long");
    let mut r = [0u8; REPORT_LEN];
    r[0] = CMD_REPORT;
    r[1] = FRAME_MARK;
    r[2..4].copy_from_slice(&((text.len() + 5) as u16).to_be_bytes());
    r[4..4 + text.len()].copy_from_slice(text);
    let end = 4 + text.len();
    r[end] = r[2..end].iter().fold(0u8, |a, &b| a.wrapping_add(b));
    r[end + 1] = FRAME_MARK;
    r
}

/// Extracts the response text. The firmware inserts a stray control byte every
/// 128 body bytes that no length field counts, so those are skipped.
fn decode_response(r: &[u8]) -> Option<String> {
    if r.len() < 4 || r[0] != RESP_REPORT || r[1] != FRAME_MARK {
        return None;
    }
    let want = (u16::from_be_bytes([r[2], r[3]]) as usize).checked_sub(5)?;
    let text: Vec<u8> = r[4..]
        .iter()
        .copied()
        .filter(|&b| b >= 0x20 || b == b'\r' || b == b'\n')
        .take(want)
        .collect();
    Some(String::from_utf8_lossy(&text).into_owned())
}

fn expect_success(reply: &str) -> io::Result<()> {
    if body(reply).contains("\"success\"") {
        Ok(())
    } else {
        Err(io::Error::other(format!("device refused: {}", reply.replace("\r\n", " "))))
    }
}

/// The part of a response after the header block.
pub fn body(reply: &str) -> &str {
    reply.split_once("\r\n\r\n").map_or("", |(_, b)| b)
}

fn unix_time() -> Duration {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured from MasterCTRL: STATE timestamp command, checksum 0x0c.
    #[test]
    fn command_framing_matches_capture() {
        let text = b"STATE timestamp 1\r\nSeqNumber=42\r\nDate=1792810097\r\nContentType=json\r\nContentLength=27\r\n\r\n{\"timestamp\":1792810097605}";
        let r = encode_command(text);
        assert_eq!(&r[..4], &[0x1E, 0x5A, 0x00, 0x78]);
        assert_eq!(&r[4 + text.len()..4 + text.len() + 3], &[0x0C, 0x5A, 0x00]);
    }

    #[test]
    fn response_skips_stray_byte() {
        let text = b"1 200\r\nAckNumber=1\r\n\r\n{\"a\":[\x021]}";
        let mut r = vec![0x20, 0x5A, 0x00, (text.len() - 1 + 5) as u8];
        r.extend_from_slice(text);
        r.extend_from_slice(&[0x00, 0x5A]);
        assert_eq!(decode_response(&r).unwrap(), "1 200\r\nAckNumber=1\r\n\r\n{\"a\":[1]}");
    }
}
