//! CLI application for flashing multiple drives in parallel.

extern crate clap;
extern crate libc;
extern crate popsicle;
extern crate pbr;

use clap::{App, Arg};
use pbr::{MultiBar, ProgressBar, Units};
use std::{process, thread};
use std::cell::RefCell;
use std::io::{self, Write};
use std::sync::Arc;

use popsicle::{DiskError, Image, Mount};

fn popsicle() -> Result<(), String> {
    let matches = App::new(env!("CARGO_PKG_NAME"))
        .about(env!("CARGO_PKG_DESCRIPTION"))
        .version(env!("CARGO_PKG_VERSION"))
        .arg(
            Arg::with_name("IMAGE")
                .help("Input image file")
                .required(true),
        )
        .arg(
            Arg::with_name("DISKS")
                .help("Output disk devices")
                .multiple(true),
        )
        .arg(
            Arg::with_name("all")
                .help("Flash all detected USB drives")
                .short("a")
                .long("all"),
        )
        .arg(
            Arg::with_name("check")
                .help("Check written image matches read image")
                .short("c")
                .long("check"),
        )
        .arg(
            Arg::with_name("unmount")
                .help("Unmount mounted devices")
                .short("u")
                .long("unmount"),
        )
        .arg(
            Arg::with_name("yes")
                .help("Continue without confirmation")
                .short("y")
                .long("yes"),
        )
        .get_matches();

    let image_path = matches.value_of("IMAGE").expect("IMAGE not set");
    let mut image = match Image::new(&image_path) {
        Ok(image) => image,
        Err(err) => {
            return Err(format!("error with image at '{}': {}", image_path, err));
        }
    };

    let image_size = image.get_size();

    let mut disk_args = vec![];
    if matches.is_present("all") {
        if let Err(err) = popsicle::get_disk_args(&mut disk_args) {
            return Err(format!("error getting USB disks: {}", err));
        }
    } else {
        if let Some(disks) = matches.values_of("DISKS") {
            for arg in disks {
                disk_args.push(arg.to_string());
            }
        }
    }

    if disk_args.is_empty() {
        return Err(format!("no disks specified"));
    }

    let mounts = match Mount::all() {
        Ok(mounts) => mounts,
        Err(err) => {
            return Err(format!("error reading mounts: {}", err));
        }
    };

    let disks = popsicle::disks_from_args(
        disk_args.into_iter(),
        &mounts,
        matches.is_present("unmount"),
    ).map_err(|why| format!("disk error: {}", why))?;

    let image_data = {
        let mut pb = ProgressBar::new(image_size);
        pb.message("Reading image: ");
        pb.set_units(Units::Bytes);
        let mut data = Vec::new();
        image
            .read(&mut data, |total| {
                pb.set(total);
            })
            .map_err(|err| format!("image error with image at '{}': {}", image_path, err))?;

        pb.finish();
        Arc::new(data)
    };

    if !matches.is_present("yes") {
        println!(
            "Are you sure you want to flash '{}' to the following drives?",
            image_path
        );
        for ref disk_tuple in disks.iter() {
            println!("  - {}", disk_tuple.0);
        }

        print!("y/N: ");
        io::stdout().flush().unwrap();

        let mut confirm = String::new();
        io::stdin().read_line(&mut confirm).unwrap();

        if confirm.trim() != "y" && confirm.trim() != "yes" {
            return Err(format!("exiting without flashing"));
        }
    }

    let check = matches.is_present("check");

    println!("");

    let mut mb = MultiBar::new();

    let mut threads = Vec::new();
    for (disk_path, mut disk) in disks {
        let mut pb = mb.create_bar(image_size);
        pb.message(&format!("W {}: ", disk_path));
        pb.set_units(Units::Bytes);
        pb.set(0);

        let image_data = image_data.clone();
        let pb = RefCell::new(pb);
        threads.push(thread::spawn(move || -> Result<(), DiskError> {
            popsicle::write_to_disk(
                |msg| pb.borrow_mut().message(msg),
                || pb.borrow_mut().finish(),
                |progress| {
                    pb.borrow_mut().set(progress);
                },
                disk,
                disk_path,
                image_size,
                &&image_data,
                check,
            )
        }));
    }

    mb.listen();

    for thread in threads {
        thread
            .join()
            .unwrap()
            .map_err(|why| format!("disk error: {}", why))?;
    }

    Ok(())
}

fn main() {
    match popsicle() {
        Ok(()) => (),
        Err(err) => {
            writeln!(io::stderr(), "popsicle: {}", err).unwrap();
            process::exit(1);
        }
    }
}
