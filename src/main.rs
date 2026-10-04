use std::{env, error::Error, path::Path, process::ExitCode};

async fn run() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<_> = env::args_os().skip(1).collect();
    if arguments.len() == 1 && (arguments[0] == "--help" || arguments[0] == "-h") {
        println!(
            "Usage:\n  trailsplice replay <evidence.json>\n  trailsplice observe <http-loopback-url> <transaction-hash> <new-file> [--wait-seconds <1..30>]\nReplay saved evidence, or poll a local node and save it. Observation waits up to 30 seconds by default, with 2-second request deadlines and 1-second polling."
        );
        return Ok(());
    }
    let report = if arguments.len() == 2 && arguments[0] == "replay" {
        trailsplice::read_report(Path::new(&arguments[1]))?
    } else if (arguments.len() == 4 || arguments.len() == 6) && arguments[0] == "observe" {
        let wait = if arguments.len() == 6 && arguments[4] == "--wait-seconds" {
            arguments[5]
                .to_str()
                .ok_or("wait must be a number")?
                .parse()?
        } else if arguments.len() == 4 {
            30
        } else {
            return Err("expected --wait-seconds <1..30>".into());
        };
        trailsplice::capture::observe(
            arguments[1].to_str().ok_or("endpoint must be UTF-8")?,
            arguments[2]
                .to_str()
                .ok_or("transaction hash must be UTF-8")?,
            Path::new(&arguments[3]),
            wait,
        )
        .await?
    } else {
        return Err("use trailsplice --help for usage".into());
    };
    print!("{report}");
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("trailsplice: {error}");
            ExitCode::FAILURE
        }
    }
}
