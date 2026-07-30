// List a Blu-ray ISO's UDF tree without mounting it, and (optionally) extract a
// file to stdout. Validates the pure-Rust UDF reader against a real image.
//
//   udf_ls <image.iso> [dir]           # list a directory (default: recurse BDMV)
//   udf_ls <image.iso> --cat <path>    # stream a file to stdout
use std::fs::File;
use std::io::{self, Read, Write};

use ripsaw::rip::udf::{ExtentReader, Udf};

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: udf_ls <image.iso> [dir | --cat <path>]");
        std::process::exit(2);
    }
    let mut f = File::open(&args[1])?;
    let udf = Udf::open(&mut f)?;

    if args.get(2).map(|s| s.as_str()) == Some("--cat") {
        let path = args.get(3).expect("--cat needs a path");
        let (size, extents) = udf.extents(&mut f, path)?;
        let mut rd = ExtentReader::new(File::open(&args[1])?, size, extents);
        let mut buf = vec![0u8; 1 << 20];
        let mut out = io::stdout().lock();
        loop {
            let n = rd.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
        }
        return Ok(());
    }

    let start = args.get(2).map(|s| s.as_str()).unwrap_or("");
    fn walk(udf: &Udf, f: &mut File, path: &str, depth: usize) -> io::Result<()> {
        for e in udf.list(f, path)? {
            let kind = if e.is_dir { "d" } else { "-" };
            println!("{:indent$}{kind} {:>14} {}", "", e.size, e.name, indent = depth * 2);
            if e.is_dir && depth < 3 {
                let child = if path.is_empty() {
                    e.name.clone()
                } else {
                    format!("{path}/{}", e.name)
                };
                walk(udf, f, &child, depth + 1)?;
            }
        }
        Ok(())
    }
    walk(&udf, &mut f, start, 0)
}
