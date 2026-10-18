use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use colophon_mcp::server::Server;
use rmcp::ServiceExt;
use rmcp::service::ServerInitializeError;
use rmcp::transport::stdio;

const USAGE: &str = "usage: colophon-mcp --roms DIRECTORY

  --roms DIRECTORY  the firmware, as the emulator's `make roms` fetches it";

fn roms(arguments: &[OsString]) -> Result<PathBuf, String> {
    let [flag, directory] = arguments else {
        return Err(USAGE.to_owned());
    };
    if flag != "--roms" {
        return Err(USAGE.to_owned());
    }
    let directory = PathBuf::from(directory);
    if !directory.is_dir() {
        return Err(format!(
            "--roms names {}, which is no directory\n\n{USAGE}",
            directory.display()
        ));
    }
    Ok(directory)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let arguments: Vec<OsString> = env::args_os().skip(1).collect();
    let roms = match roms(&arguments) {
        Ok(roms) => roms,
        Err(problem) => {
            eprintln!("{problem}");
            return ExitCode::from(2);
        }
    };
    let service = match Server::new(roms).serve(stdio()).await {
        Ok(service) => service,
        // A client may ask who the server is and go.
        Err(ServerInitializeError::ConnectionClosed(_)) => return ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("the server could not start: {error}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = service.waiting().await {
        eprintln!("the server stopped: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
