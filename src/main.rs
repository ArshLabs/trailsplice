use std::{env, error::Error, path::Path, process::ExitCode};

fn run() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<_> = env::args_os().skip(1).collect();
    if arguments.len() == 1 && (arguments[0] == "--help" || arguments[0] == "-h") {
        println!(
            "Usage: trailsplice replay <evidence.json>\nExplain saved observations for one transaction and source."
        );
        return Ok(());
    }
    if arguments.len() != 2 || arguments[0] != "replay" {
        return Err("usage: trailsplice replay <evidence.json>".into());
    }
    let report = trailsplice::read_report(Path::new(&arguments[1]))?;
    print!("{report}");
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("trailsplice: {error}");
            ExitCode::FAILURE
        }
    }
}
