//! Converts an `ostrace` trace image from a RAM dump to bytrace text.

use std::env;
use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;

use ostrace::TraceImageSnapshot;

struct Args {
    ram: PathBuf,
    out: PathBuf,
    ram_base: u64,
    trace_base: u64,
}

fn main() -> io::Result<()> {
    let args = parse_args()?;
    let mut ram = Vec::new();
    File::open(&args.ram)?.read_to_end(&mut ram)?;
    let snapshot = TraceImageSnapshot::parse_from_ram(&ram, args.ram_base, args.trace_base)
        .map_err(|err| invalid_data(format!("failed to parse trace image: {err:?}")))?;
    let mut out = File::create(&args.out)?;
    ostrace::export::bytrace::write_image_bytrace(snapshot, &mut out)?;
    Ok(())
}

fn parse_args() -> io::Result<Args> {
    let mut ram = None;
    let mut out = None;
    let mut ram_base = None;
    let mut trace_base = None;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--ram" => ram = args.next().map(PathBuf::from),
            "--out" => out = args.next().map(PathBuf::from),
            "--ram-base" => ram_base = args.next().and_then(|value| parse_u64(&value).ok()),
            "--trace-base" => trace_base = args.next().and_then(|value| parse_u64(&value).ok()),
            _ => return Err(invalid_input(format!("unknown argument: {arg}"))),
        }
    }
    Ok(Args {
        ram: ram.ok_or_else(|| invalid_input("missing --ram"))?,
        out: out.ok_or_else(|| invalid_input("missing --out"))?,
        ram_base: ram_base.ok_or_else(|| invalid_input("missing --ram-base"))?,
        trace_base: trace_base.ok_or_else(|| invalid_input("missing --trace-base"))?,
    })
}

fn parse_u64(value: &str) -> Result<u64, std::num::ParseIntError> {
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u64::from_str_radix(hex, 16)
    } else {
        value.parse()
    }
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
