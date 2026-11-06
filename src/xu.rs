//! Reading the card's vendor controls (the UVC Extension Unit), for working out
//! what they do. Read-only: this module never sends SET requests.
//!
//! The 4K X has one Extension Unit (ID 4, GUID 961073c7-49f7-44f2-ab42-e940405940c2)
//! with up to 17 controls. `lucentia xu-dump` prints every control's size, flags and
//! values; `--watch` reprints whatever changes, so you can flip a setting on the
//! console and see which control follows it.

use std::collections::BTreeMap;
use std::os::fd::AsRawFd;

use crate::log;

// UVC request codes (UVC 1.5, table A-8).
const GET_CUR: u8 = 0x81;
const GET_MIN: u8 = 0x82;
const GET_MAX: u8 = 0x83;
const GET_RES: u8 = 0x84;
const GET_LEN: u8 = 0x85;
const GET_INFO: u8 = 0x86;
const GET_DEF: u8 = 0x87;

/// struct uvc_xu_control_query from <linux/uvcvideo.h>.
#[repr(C)]
struct XuQuery {
    unit: u8,
    selector: u8,
    query: u8,
    size: u16,
    data: *mut u8,
}

/// _IOWR('u', 0x21, struct uvc_xu_control_query)
const UVCIOC_CTRL_QUERY: libc::c_ulong = (3 << 30)
    | ((std::mem::size_of::<XuQuery>() as libc::c_ulong) << 16)
    | ((b'u' as libc::c_ulong) << 8)
    | 0x21;

fn query(fd: i32, unit: u8, selector: u8, request: u8, len: usize) -> Option<Vec<u8>> {
    let mut data = vec![0u8; len.max(1)];
    let mut q = XuQuery {
        unit,
        selector,
        query: request,
        size: len as u16,
        data: data.as_mut_ptr(),
    };
    // SAFETY: q points at a buffer of exactly `size` bytes that outlives the call;
    // all requests here are GET requests, which only write into that buffer.
    let r = unsafe { libc::ioctl(fd, UVCIOC_CTRL_QUERY as _, &mut q as *mut XuQuery) };
    (r == 0).then_some(data)
}

#[derive(Clone, PartialEq)]
struct Control {
    len: usize,
    info: u8,
    cur: Option<Vec<u8>>,
    min: Option<Vec<u8>>,
    max: Option<Vec<u8>>,
    res: Option<Vec<u8>>,
    def: Option<Vec<u8>>,
}

fn read_all(fd: i32, unit: u8) -> BTreeMap<u8, Control> {
    let mut out = BTreeMap::new();
    for sel in 1..=32u8 {
        let Some(len) = query(fd, unit, sel, GET_LEN, 2) else {
            continue;
        };
        let len = u16::from_le_bytes([len[0], len[1]]) as usize;
        let info = query(fd, unit, sel, GET_INFO, 1).map(|i| i[0]).unwrap_or(0);
        let get = |r| if len > 0 { query(fd, unit, sel, r, len) } else { None };
        out.insert(
            sel,
            Control {
                len,
                info,
                // GET_CUR only when the control says it supports GET.
                cur: if info & 1 != 0 { get(GET_CUR) } else { None },
                min: get(GET_MIN),
                max: get(GET_MAX),
                res: get(GET_RES),
                def: get(GET_DEF),
            },
        );
    }
    out
}

fn hex(v: &Option<Vec<u8>>) -> String {
    match v {
        None => "-".into(),
        Some(b) if b.len() > 32 => {
            let head: Vec<String> = b[..32].iter().map(|x| format!("{x:02x}")).collect();
            format!("{} … ({} bytes)", head.join(" "), b.len())
        }
        Some(b) => b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" "),
    }
}

fn flags(info: u8) -> String {
    let mut f = Vec::new();
    if info & 0x01 != 0 {
        f.push("get");
    }
    if info & 0x02 != 0 {
        f.push("set");
    }
    if info & 0x04 != 0 {
        f.push("disabled-by-auto");
    }
    if info & 0x08 != 0 {
        f.push("autoupdate");
    }
    if info & 0x10 != 0 {
        f.push("async");
    }
    f.join(",")
}

/// Classic hexdump: offset, 16 bytes, and the printable characters.
fn hexdump(b: &[u8]) {
    for (i, row) in b.chunks(16).enumerate() {
        let hex: Vec<String> = row.iter().map(|x| format!("{x:02x}")).collect();
        let text: String = row
            .iter()
            .map(|&c| if (0x20..0x7f).contains(&c) { c as char } else { '.' })
            .collect();
        println!("      {:04x}  {:<47}  {}", i * 16, hex.join(" "), text);
    }
}

fn print_control(sel: u8, c: &Control, full: bool) {
    println!(
        "{} len {:3}  [{}]",
        log::cyan(&format!("selector {sel:2}")),
        c.len,
        flags(c.info)
    );
    match (&c.cur, full) {
        (Some(b), true) if b.len() > 16 => {
            println!("    cur ({} bytes)", b.len());
            hexdump(b);
        }
        _ => println!("    cur {}", hex(&c.cur)),
    }
    for (name, v) in [("min", &c.min), ("max", &c.max), ("res", &c.res), ("def", &c.def)] {
        if v.is_some() {
            println!("    {name} {}", hex(v));
        }
    }
}

/// `lucentia xu-dump [--device /dev/videoN] [--unit N] [--watch]`
pub fn run(args: &[String], default_device: Option<String>) -> Result<(), String> {
    let mut device = default_device;
    let mut unit = 4u8;
    let mut watch = false;
    let mut full = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--device" => device = it.next().cloned(),
            "--unit" => {
                unit = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or("--unit needs a number")?
            }
            "--watch" => watch = true,
            "--full" => full = true,
            "--debug" | "-d" => {}
            other => return Err(format!("unknown option `{other}`")),
        }
    }
    let device = device.ok_or("no capture card found; pass --device /dev/videoN")?;
    // PipeWire names it "v4l2:/dev/videoN".
    let device = device.trim_start_matches("v4l2:").to_string();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&device)
        .map_err(|e| format!("could not open {device}: {e}"))?;
    let fd = file.as_raw_fd();

    println!("{} {device}, unit {unit} (read-only)", log::bold("extension unit:"));
    let mut last = read_all(fd, unit);
    if last.is_empty() {
        return Err(format!("no controls answered on unit {unit}"));
    }
    for (sel, c) in &last {
        print_control(*sel, c, full);
    }
    if !watch {
        return Ok(());
    }
    println!(
        "\n{} change one thing on the source at a time. Nothing prints until a value \
         changes; Ctrl+C to stop.",
        log::bold("watching:")
    );
    loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let now = read_all(fd, unit);
        for (sel, c) in &now {
            if last.get(sel).map(|o| &o.cur) != Some(&c.cur) {
                let old = last.get(sel).map(|o| hex(&o.cur)).unwrap_or_default();
                println!(
                    "{} {}  {} -> {}",
                    crate::record::local_timestamp(),
                    log::yellow(&format!("selector {sel:2}")),
                    old,
                    hex(&c.cur)
                );
                if full {
                    if let Some(b) = &c.cur {
                        hexdump(b);
                    }
                }
            }
        }
        last = now;
    }
}
